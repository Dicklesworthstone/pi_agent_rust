use super::*;
use crate::ask::{AskAnswer, AskPolicy, AskResponse};
use async_trait::async_trait;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

fn params() -> Value {
    json!({"message":"Choose", "requestedSchema":{
        "type":"object", "properties":{"value":{"type":"string"}},
        "required":["value"]
    }})
}

fn admitted(registry: &Arc<ElicitationRegistry>, task: bool) -> Subscription {
    let dispatch = Arc::new(RequestDispatch::new());
    assert!(dispatch.claim());
    registry.subscribe(dispatch, 7, task).unwrap()
}

fn take(subscription: &Subscription) -> Poll<ServerRequest> {
    subscription.poll_request(&mut Context::from_waker(Waker::noop()), false)
}

#[test]
fn unsolicited_queued_ambiguous_old_and_overlapping_prompts_do_not_acquire_an_owner() {
    let registry = Arc::new(ElicitationRegistry::default());
    assert_eq!(
        registry.route(&json!(1), Some(&params()), 7).unwrap()["result"]["action"],
        "cancel"
    );
    let dispatch = Arc::new(RequestDispatch::new());
    let queued = registry.subscribe(Arc::clone(&dispatch), 7, false).unwrap();
    assert_eq!(
        registry.route(&json!(2), Some(&params()), 7).unwrap()["result"]["action"],
        "cancel"
    );
    assert!(dispatch.claim());
    assert_eq!(
        registry.route(&json!(3), Some(&params()), 8).unwrap()["result"]["action"],
        "cancel"
    );
    let other = admitted(&registry, false);
    assert_eq!(
        registry.route(&json!(4), Some(&params()), 7).unwrap()["result"]["action"],
        "cancel"
    );
    drop(other);
    assert!(registry.route(&json!(5), Some(&params()), 7).is_none());
    assert!(
        registry.route(&json!(5), Some(&params()), 7).is_none(),
        "duplicate must not receive a second reply"
    );
    assert!(
        registry
            .route(&json!(5), Some(&json!({"mode":"url"})), 7)
            .is_none(),
        "malformed duplicate must not race the original answer"
    );
    assert_eq!(
        registry.route(&json!(6), Some(&params()), 7).unwrap()["result"]["action"],
        "cancel"
    );
    assert!(matches!(take(&queued), Poll::Ready(request) if request.authorized));
    queued.complete();
    drop(queued);
    assert!(lock(&registry.slots).is_empty());
}

#[test]
fn receipt_race_defers_ui_until_exact_task_identity_is_bound() {
    let registry = Arc::new(ElicitationRegistry::default());
    let subscription = admitted(&registry, true);
    let mut request = params();
    request["_meta"] = json!({(RELATED_TASK):{"taskId":"one"}});
    assert!(registry.route(&json!(1), Some(&request), 7).is_none());
    assert!(take(&subscription).is_pending());
    subscription.task_binding().bind("one");
    assert!(matches!(take(&subscription), Poll::Ready(request) if request.authorized));
    subscription.complete();
    let other = admitted(&registry, true);
    request["_meta"][RELATED_TASK]["taskId"] = json!("two");
    assert!(registry.route(&json!(2), Some(&request), 7).is_none());
    assert!(take(&other).is_pending());
    other.task_binding().bind("different");
    assert!(matches!(take(&other), Poll::Ready(request) if !request.authorized));
    other.complete();
    request["_meta"][RELATED_TASK]["taskId"] = json!("one");
    assert!(registry.route(&json!(3), Some(&request), 7).is_none());
    assert!(matches!(take(&subscription), Poll::Ready(request) if request.authorized));
    assert!(take(&other).is_pending());
}

#[test]
fn cancelled_form_and_request_count_are_bounded_per_owner() {
    let registry = Arc::new(ElicitationRegistry::default());
    let subscription = admitted(&registry, false);
    for id in 0..MAX_REQUESTS_PER_CALL {
        assert!(registry.route(&json!(id), Some(&params()), 7).is_none());
        registry.cancel(Some(&json!({"requestId":id})), 8);
        registry.cancel(Some(&json!({"requestId":id})), 7);
        let Poll::Ready(request) = take(&subscription) else {
            panic!("queued form")
        };
        assert!(request.cancelled.load(Ordering::Acquire));
        subscription.complete();
    }
    assert_eq!(
        registry.route(&json!(100), Some(&params()), 7).unwrap()["result"]["action"],
        "cancel"
    );
    for params in [json!({"mode":"url"}), json!({"task":{"ttl":10}})] {
        assert_eq!(
            registry.route(&json!(101), Some(&params), 7).unwrap()["error"]["code"],
            -32602
        );
    }
}

#[test]
fn form_contract_enforces_formats_patterns_enums_arrays_and_primitive_limits() {
    let form = Form::parse(&json!({"message":"Profile", "requestedSchema":{
        "type":"object", "properties":{
            "email":{"type":"string","format":"email"},
            "count":{"type":"integer","minimum":2,"maximum":8},
            "code":{"type":"string","pattern":"^[A-Z]{2}$"},
            "color":{"type":"string","oneOf":[{"const":"g","title":"Green"},{"const":"r","title":"Red"}]},
            "tags":{"type":"array","minItems":1,"maxItems":2,"items":{"anyOf":[{"const":"x","title":"X"},{"const":"y","title":"Y"}]}}
        }, "required":["email","count","code","color","tags"]
    }})).expect("primitive form");
    let valid = json!({"email":"person@example.com", "count":7, "code":"AB", "color":"g", "tags":["x","y"]});
    assert!(form.schema.accepts(&valid));
    for (field, invalid) in [
        ("email", json!("no-at-sign")),
        ("count", json!(7.5)),
        ("code", json!("abc")),
        ("color", json!("Green")),
        ("tags", json!(["z"])),
    ] {
        let mut invalid_form = valid.clone();
        invalid_form[field] = invalid;
        assert!(!form.schema.accepts(&invalid_form), "{field}");
    }
    for schema in [
        json!({"type":"object"}),
        json!({"type":"string","$ref":"https://example.com/schema"}),
        json!({"type":"array","items":{"type":"object"}}),
        json!({"type":"integer","default":"7"}),
    ] {
        assert!(Field::parse("field", &schema, true).is_none());
    }
    let escaped = display("\u{1b}[2J\n\u{202e}[click](https://example.com) person@example.com");
    assert!(!escaped.chars().any(char::is_control));
    assert!(!escaped.contains('\u{202e}'));
    assert!(!escaped.contains("https:"));
    assert!(!escaped.contains('['));
}

struct ReplyTransport {
    replies: Mutex<Vec<Value>>,
    received: AtomicBool,
}

#[async_trait]
impl McpTransport for ReplyTransport {
    async fn request(&self, _: &str, _: Value, _: Duration) -> Result<Value> {
        unreachable!()
    }
    async fn notify(&self, _: &str, _: Value) -> Result<()> {
        unreachable!()
    }
    fn is_alive(&self) -> bool {
        true
    }
    fn abort(&self) {}
    async fn close(&self) {}
    fn diagnostics_tail(&self) -> String {
        String::new()
    }
    async fn respond_to_server_request(&self, response: Value, generation: u64) -> Result<()> {
        assert_eq!(generation, 7);
        lock(&self.replies).push(response);
        self.received.store(true, Ordering::Release);
        // A server can finish tools/call before acknowledging the response
        // POST. The observer must not send a second cancellation for this ID.
        let mut polls = 0;
        poll_fn(|task| {
            polls += 1;
            if polls > 2 {
                Poll::Ready(())
            } else {
                task.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        Ok(())
    }
}

#[test]
fn final_result_cancels_an_accepted_unshown_form_and_does_not_duplicate_inflight_answers() {
    for early in [true, false] {
        let registry = Arc::new(ElicitationRegistry::default());
        let subscription = admitted(&registry, false);
        let native = Arc::new(ReplyTransport {
            replies: Mutex::new(Vec::new()),
            received: AtomicBool::new(false),
        });
        let transport: Arc<dyn McpTransport> = native.clone();
        let ask = AskTool::new(AskPolicy::Recommended);
        let questions = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&questions);
        ask.set_handler(Arc::new(move |request| {
            count.fetch_add(1, Ordering::Relaxed);
            let question = &request.questions[0];
            let review = question.id.as_deref() == Some("mcp_review");
            let answer = AskAnswer {
                question_id: question.id.clone().unwrap(),
                selected: if review {
                    vec![SUBMIT.to_string()]
                } else {
                    Vec::new()
                },
                other: (!review).then(|| "answer".to_string()),
            };
            Box::pin(async move {
                Ok(AskResponse {
                    answers: vec![answer],
                    dismissed: false,
                })
            })
        }));
        let operation = async {
            assert!(registry.route(&json!("form"), Some(&params()), 7).is_none());
            if !early {
                poll_fn(|_| {
                    if native.received.load(Ordering::Acquire) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            }
            Ok(json!({"content":[]}))
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime
            .block_on(observe(
                operation,
                Some(subscription),
                Some(ask),
                "peer",
                &transport,
                None,
                || true,
            ))
            .unwrap();
        let replies = lock(&native.replies);
        assert_eq!(replies.len(), 1);
        assert_eq!(
            replies[0]["result"]["action"],
            if early { "cancel" } else { "accept" }
        );
        assert_eq!(questions.load(Ordering::Relaxed), if early { 0 } else { 2 });
        assert!(lock(&registry.slots).is_empty());
    }
}

#[test]
fn headless_recommended_policy_never_supplies_server_form_values() {
    let registry = Arc::new(ElicitationRegistry::default());
    let subscription = admitted(&registry, false);
    let native = Arc::new(ReplyTransport {
        replies: Mutex::new(Vec::new()),
        received: AtomicBool::new(false),
    });
    let transport: Arc<dyn McpTransport> = native.clone();
    let operation = async {
        assert!(registry.route(&json!(1), Some(&params()), 7).is_none());
        poll_fn(|_| {
            if native.received.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        Ok(json!({"content":[]}))
    };
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime
        .block_on(observe(
            operation,
            Some(subscription),
            Some(AskTool::new(AskPolicy::Recommended)),
            "peer",
            &transport,
            None,
            || true,
        ))
        .unwrap();
    assert_eq!(lock(&native.replies)[0]["result"]["action"], "cancel");
    assert!(lock(&native.replies)[0]["result"].get("content").is_none());
}

fn url_params() -> Value {
    json!({
        "mode":"url", "elicitationId":"owned-url",
        "message":"Connect account\u{1b}[2J\n\u{202e} https://misleading.example",
        "url":"https://Example.com:8443/connect?state=private-flow&next=%2Faccount#step-1"
    })
}

#[test]
fn url_requests_require_bounded_https_destinations_without_hidden_authority_or_form_fields() {
    let params = url_params();
    let parsed = UrlRequest::parse(&params).expect("valid URL request");
    assert_eq!(parsed.destination, "example.com:8443");
    assert_eq!(parsed.url, params["url"].as_str().unwrap());
    for invalid in [
        "",
        "https:///example.com",
        "http://example.com/",
        "file:///tmp/secret",
        "javascript:alert(1)",
        "https://user:secret@example.com/",
        "https://@example.com/",
        "https://example.com\\@elsewhere.invalid/",
        "https://example.com/\nnext",
        "https://example.com/\t",
        " https://example.com/",
        "https://münich.example/",
        "https://example.com/\u{202e}evil",
        "https://example.com/```hidden",
        "https://example.com/<hidden>",
        "https://example.com/\"hidden\"",
    ] {
        let mut request = params.clone();
        request["url"] = json!(invalid);
        assert!(
            UrlRequest::parse(&request).is_none(),
            "accepted {invalid:?}"
        );
        let registry = ElicitationRegistry::default();
        assert_eq!(
            registry.route(&json!(1), Some(&request), 7).unwrap()["error"]["code"],
            -32602
        );
    }
    for (key, value) in [
        (
            "url",
            json!(format!("https://example.com/{}", "x".repeat(MAX_URL_BYTES))),
        ),
        ("message", json!("x".repeat(MAX_VALUE_BYTES + 1))),
        ("elicitationId", json!("")),
        ("elicitationId", json!("x".repeat(1025))),
        ("elicitationId", json!(false)),
        ("mode", json!("URL")),
        ("requestedSchema", json!({"type":"object"})),
    ] {
        let mut request = params.clone();
        request[key] = value;
        assert!(UrlRequest::parse(&request).is_none(), "accepted {key}");
    }
}

#[test]
fn url_decisions_open_only_after_explicit_consent_and_never_return_url_or_free_text() {
    for (choice, fails, expected, opens) in [
        (Some(OPEN_URL), false, "accept", 1),
        (Some(OPEN_URL), true, "cancel", 1),
        (Some(OPENED_URL), false, "accept", 0),
        (Some(DECLINE), false, "decline", 0),
        (Some(CANCEL), false, "cancel", 0),
        (Some("free-text-secret"), false, "cancel", 0),
        (None, false, "cancel", 0),
    ] {
        let registry = Arc::new(ElicitationRegistry::default());
        let subscription = admitted(&registry, false);
        let opened = Arc::new(Mutex::new(Vec::new()));
        let browser_urls = Arc::clone(&opened);
        registry.set_url_opener(Arc::new(move |url| {
            lock(&browser_urls).push(url);
            Box::pin(async move {
                if fails {
                    Err(io::Error::other("not installed"))
                } else {
                    Ok(())
                }
            })
        }));
        let native = Arc::new(ReplyTransport {
            replies: Mutex::new(Vec::new()),
            received: AtomicBool::new(false),
        });
        let transport: Arc<dyn McpTransport> = native.clone();
        let ask = AskTool::new(AskPolicy::Recommended);
        let questions = Arc::new(AtomicUsize::new(0));
        if let Some(choice) = choice {
            let count = Arc::clone(&questions);
            let opened_before_consent = Arc::clone(&opened);
            ask.set_handler(Arc::new(move |request| {
                assert!(
                    lock(&opened_before_consent).is_empty(),
                    "never navigate before consent"
                );
                count.fetch_add(1, Ordering::Relaxed);
                crate::ask::validate_request(request.clone()).unwrap();
                let question = &request.questions[0];
                assert_eq!(question.header.as_deref(), Some("MCP URL"));
                assert_eq!(question.recommended, None);
                assert!(question.question.contains("MCP server \"peer\""));
                assert!(
                    question
                        .question
                        .contains("Destination host: example.com:8443")
                );
                assert!(
                    question
                        .question
                        .contains(url_params()["url"].as_str().unwrap())
                );
                assert!(!question.question.contains('\u{1b}'));
                assert!(!question.question.contains('\u{202e}'));
                assert!(!question.question.contains("https://misleading.example"));
                let free = choice == "free-text-secret";
                let answer = AskAnswer {
                    question_id: question.id.clone().unwrap(),
                    selected: if free {
                        Vec::new()
                    } else {
                        vec![choice.to_string()]
                    },
                    other: free.then(|| "free-text-secret".to_string()),
                };
                Box::pin(async move {
                    Ok(AskResponse {
                        answers: vec![answer],
                        dismissed: false,
                    })
                })
            }));
        }
        let operation = async {
            assert!(
                registry
                    .route(&json!("url-request"), Some(&url_params()), 7)
                    .is_none()
            );
            // Repeated transport delivery cannot add another picker/navigation.
            assert!(
                registry
                    .route(&json!("url-request"), Some(&url_params()), 7)
                    .is_none()
            );
            poll_fn(|_| {
                if native.received.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            Ok(json!({"content":[]}))
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime
            .block_on(observe(
                operation,
                Some(subscription),
                Some(ask),
                "peer",
                &transport,
                None,
                || true,
            ))
            .unwrap();
        let replies = lock(&native.replies);
        assert_eq!(
            replies.as_slice(),
            &[json!({"jsonrpc":"2.0","id":"url-request","result":{"action":expected}})]
        );
        assert_eq!(lock(&opened).len(), opens);
        if opens != 0 {
            assert_eq!(lock(&opened)[0], url_params()["url"].as_str().unwrap());
        }
        assert_eq!(
            questions.load(Ordering::Relaxed),
            usize::from(choice.is_some())
        );
        assert!(lock(&registry.slots).is_empty());
    }
}

#[test]
fn cancellation_or_replaced_caller_after_a_picker_answer_never_opens_the_url() {
    for replace in [false, true] {
        let registry = Arc::new(ElicitationRegistry::default());
        let subscription = admitted(&registry, false);
        registry.set_url_opener(Arc::new(|_| panic!("obsolete answer launched a browser")));
        let native = Arc::new(ReplyTransport {
            replies: Mutex::new(Vec::new()),
            received: AtomicBool::new(false),
        });
        let transport: Arc<dyn McpTransport> = native.clone();
        let current = Arc::new(AtomicBool::new(true));
        let current_in_picker = Arc::clone(&current);
        let cancellation = Arc::clone(&registry);
        let ask = AskTool::new(AskPolicy::Recommended);
        ask.set_handler(Arc::new(move |request| {
            if replace {
                current_in_picker.store(false, Ordering::Release);
            } else {
                cancellation.cancel(Some(&json!({"requestId":"url-request"})), 7);
            }
            let answer = AskAnswer {
                question_id: request.questions[0].id.clone().unwrap(),
                selected: vec![OPEN_URL.to_string()],
                other: None,
            };
            Box::pin(async move {
                Ok(AskResponse {
                    answers: vec![answer],
                    dismissed: false,
                })
            })
        }));
        let operation = async {
            assert!(
                registry
                    .route(&json!("url-request"), Some(&url_params()), 7)
                    .is_none()
            );
            poll_fn(|_| {
                if native.received.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            Ok(json!({"content":[]}))
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let result = runtime.block_on(observe(
            operation,
            Some(subscription),
            Some(ask),
            "peer",
            &transport,
            None,
            || current.load(Ordering::Acquire),
        ));
        assert_eq!(result.is_err(), replace);
        if !replace {
            assert_eq!(
                lock(&native.replies)[0]["result"],
                json!({"action":"cancel"})
            );
        }
        assert!(lock(&registry.slots).is_empty());
    }
}
