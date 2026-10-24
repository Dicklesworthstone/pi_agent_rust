//! Controlled-provider execution tests, not live-model or compilation evidence.
use super::*;
use crate::eval::EvalTool;
use crate::judgment::Judge;
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, UserContent,
};
use crate::provider::{Context, Provider, StreamOptions};
use crate::tools::{Tool, ToolOutput};
use futures::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

#[derive(Default)]
struct Fixture {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<(String, String, Option<u32>)>>,
    dropped: Arc<AtomicBool>,
}

enum Reply {
    Text(String),
    BadStop,
    NoTerminal,
    Tool,
    Failed,
    Pending,
}

struct Pending(Arc<AtomicBool>);
impl Stream for Pending {
    type Item = crate::error::Result<StreamEvent>;
    fn poll_next(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[async_trait::async_trait]
impl Provider for Fixture {
    fn name(&self) -> &str {
        "completion-fixture"
    }
    fn api(&self) -> &str {
        "fixture"
    }
    fn model_id(&self) -> &str {
        "host-model"
    }
    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> crate::error::Result<Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>>
    {
        assert!(context.tools.is_empty(), "no execution authority");
        assert_eq!(context.messages.len(), 1, "no conversation replay");
        assert_eq!(options.api_key.as_deref(), Some("HOST-CREDENTIAL"));
        assert_eq!(
            options.headers.get("X-Host").map(String::as_str),
            Some("fixed")
        );
        let Message::User(user) = &context.messages[0] else {
            panic!("user");
        };
        let UserContent::Text(prompt) = &user.content else {
            panic!("text");
        };
        self.requests.lock().unwrap().push((
            context.system_prompt.as_deref().unwrap().to_owned(),
            prompt.clone(),
            options.max_tokens,
        ));
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Text("answer".into()));
        let (text, reason) = match reply {
            Reply::Text(text) => (text, StopReason::Stop),
            Reply::BadStop => ("partial".into(), StopReason::Length),
            Reply::NoTerminal => return Ok(Box::pin(futures::stream::empty())),
            Reply::Failed => return Err(Error::api("PRIVATE-PROVIDER-PAYLOAD")),
            Reply::Pending => return Ok(Box::pin(Pending(Arc::clone(&self.dropped)))),
            Reply::Tool => ("not executable".into(), StopReason::ToolUse),
        };
        let mut message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            stop_reason: reason,
            ..Default::default()
        };
        message.usage.input = 13;
        message.usage.output = 3;
        Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
            reason,
            message,
        })])))
    }
}

fn fixture(replies: impl IntoIterator<Item = Reply>) -> Arc<Fixture> {
    Arc::new(Fixture {
        replies: Mutex::new(replies.into_iter().collect()),
        ..Default::default()
    })
}

fn binding(provider: &Arc<Fixture>) -> Judge {
    Judge::new(
        provider.clone(),
        StreamOptions {
            api_key: Some("HOST-CREDENTIAL".into()),
            headers: [("X-Host".into(), "fixed".into())].into_iter().collect(),
            max_tokens: Some(99),
            ..Default::default()
        },
    )
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn code(kernel: &str, input: &Value) -> String {
    if kernel == "js" {
        format!("JSON.parse(await tool.call('__pi_complete', {input})).text")
    } else {
        let encoded = serde_json::to_string(&input.to_string()).unwrap();
        format!(
            "import json\nanswer = json.loads(await tool.call('__pi_complete', json.loads({encoded})))\nanswer['text']"
        )
    }
}

#[test]
fn public_python_and_javascript_cells_use_the_same_binding_and_receipts() {
    for kernel in ["js", "python"] {
        if kernel == "python" && !python() {
            eprintln!("python3 unavailable");
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let provider = fixture([]);
        let tool = EvalTool::new(dir.path()).with_judge(binding(&provider));
        let out = runtime().block_on(tool.execute("complete",json!({"kernel":kernel,
            "code":code(kernel,&json!({"prompt":"  literal {previous}\n雪  ","system":"answer literally","max_tokens":7}))
        }),None)).unwrap();
        assert!(!out.is_error, "{}", text(&out));
        assert!(text(&out).contains("answer"));
        let details = out.details.unwrap();
        let receipt = &details["judgments"]["attempts"][0];
        assert_eq!(receipt["operation"], "completion");
        assert_eq!(receipt["status"], "completed");
        assert_eq!(receipt["usage"]["input"], 13);
        assert_eq!(receipt["usageKnown"], true);
        assert!(!details.to_string().contains("HOST-CREDENTIAL"));
        assert_eq!(
            provider.requests.lock().unwrap().as_slice(),
            &[(
                "answer literally".into(),
                "  literal {previous}\n雪  ".into(),
                Some(7)
            )]
        );
    }
}

#[test]
fn request_tokens_can_only_shorten_the_bound_host_ceiling() {
    let provider = fixture([]);
    let judge = binding(&provider);
    runtime().block_on(async {
        for limit in [None, Some(1), Some(4096)] {
            let mut input = json!({"prompt":"request"});
            if let Some(limit) = limit {
                input["max_tokens"] = json!(limit);
            }
            let out = judge
                .complete(CompletionRequest::parse(input).unwrap())
                .await
                .unwrap();
            assert_eq!(out["schema"], "pi.completion.v1");
            assert_eq!(out["provider"], "completion-fixture");
            assert_eq!(out["text"], "answer");
        }
    });
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.iter().map(|r| r.2).collect::<Vec<_>>(),
        vec![Some(99), Some(1), Some(99)]
    );
}

#[test]
fn incomplete_failed_empty_and_oversized_responses_never_become_answers() {
    let provider = fixture([
        Reply::BadStop,
        Reply::NoTerminal,
        Reply::Tool,
        Reply::Failed,
        Reply::Text(String::new()),
        Reply::Text("x".repeat(32 * 1024 + 1)),
    ]);
    let mut invocation = Invocation::new(Some(binding(&provider)), &json!({}), None).unwrap();
    runtime().block_on(async {
        for _ in 0..6 {
            let error = invocation
                .complete(Path::new("."), json!({"prompt":"x"}))
                .await
                .unwrap_err();
            assert!(error.contains("PI_COMPLETION_PROVIDER"), "{error}");
            assert!(!error.contains("PRIVATE-PROVIDER-PAYLOAD"));
        }
        assert!(
            invocation
                .complete(Path::new("."), json!({"prompt":"next"}))
                .await
                .is_ok()
        );
    });
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        7,
        "no automatic replay"
    );
    assert!(
        invocation.receipts[..6]
            .iter()
            .all(|r| r["status"] == "failed" && r["usageKnown"] == false)
    );
}

#[test]
fn completions_judgments_and_batches_share_one_attempt_budget() {
    let provider = fixture([
        Reply::Text("text".into()),
        Reply::Text(r#"{"q":0.8}"#.into()),
    ]);
    let mut invocation = Invocation::new(Some(binding(&provider)), &json!({}), None).unwrap();
    runtime().block_on(async {
        invocation
            .complete(Path::new("."), json!({"prompt":"x"}))
            .await
            .unwrap();
        invocation
            .evaluate(
                Path::new("."),
                json!({"state":{},"questions":{"q":{"type":"bool","instructions":"test"}}}),
            )
            .await
            .unwrap();
        for _ in 2..MAX_CALLS {
            invocation
                .complete(Path::new("."), json!({"prompt":"x"}))
                .await
                .unwrap();
        }
        let error = invocation
            .complete(Path::new("."), json!({"prompt":"one too many"}))
            .await
            .unwrap_err();
        assert!(error.contains("PI_COMPLETION_LIMIT"));
        let error = invocation
            .evaluate_batch(
                Path::new("."),
                json!({"states":[{}],"questions":{"q":{"type":"bool","instructions":"test"}}}),
            )
            .await
            .unwrap_err();
        assert!(error.contains("PI_JUDGE_LIMIT"));
    });
    assert_eq!(provider.requests.lock().unwrap().len(), MAX_CALLS);
    assert_eq!(invocation.receipts.len(), MAX_CALLS);
}

#[test]
fn privacy_covers_system_and_dynamic_prompt_and_does_not_survive_the_call() {
    let provider = fixture([]);
    let judge = binding(&provider);
    runtime().block_on(async {
        for replacement in ["FIRST", "SECOND"] {
            let protect = |parts: &[&str]| -> crate::error::Result<Vec<String>> {
                assert_eq!(parts.len(), 2);
                Ok(parts
                    .iter()
                    .map(|part| part.replace("SESSION-CANARY", replacement))
                    .collect())
            };
            let mut invocation =
                Invocation::new(Some(judge.clone()), &json!({}), Some(&protect)).unwrap();
            invocation
                .complete(
                    Path::new("."),
                    json!({"prompt":"SESSION-CANARY","system":"SESSION-CANARY"}),
                )
                .await
                .unwrap();
        }
        let block = |_: &[&str]| -> crate::error::Result<Vec<String>> {
            Err(Error::validation("PI_SECRET_BLOCK: SESSION-CANARY"))
        };
        let mut invocation =
            Invocation::new(Some(judge.clone()), &json!({}), Some(&block)).unwrap();
        let error = invocation
            .complete(Path::new("."), json!({"prompt":"SESSION-CANARY"}))
            .await
            .unwrap_err();
        assert!(error.contains("PI_SECRET_BLOCK"));
        assert!(!error.contains("SESSION-CANARY"));
        assert_eq!(invocation.receipts[0]["status"], "failed");
    });
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        (&requests[0].0, &requests[0].1),
        (&"FIRST".to_string(), &"FIRST".to_string())
    );
    assert_eq!(
        (&requests[1].0, &requests[1].1),
        (&"SECOND".to_string(), &"SECOND".to_string())
    );
}

#[test]
fn invalid_requests_and_unbound_calls_never_consume_an_attempt_or_invoke_tools() {
    let dir = tempfile::tempdir().unwrap();
    let provider = fixture([]);
    let tool = EvalTool::new(dir.path()).with_judge(binding(&provider));
    runtime().block_on(async {
        let out = tool.execute("bad",json!({"kernel":"js","code":"try {await tool.call('__pi_complete',{prompt:'x',tools:['bash']})}catch(e){String(e)}"}),None).await.unwrap();
        assert!(text(&out).contains("PI_COMPLETION_INPUT"));
        assert!(out.details.unwrap().get("judgments").is_none());
        let out = tool.execute("nested",json!({"kernel":"js","code":"await tool.result('__pi_complete',{prompt:'x'})"}),None).await.unwrap();
        assert!(out.is_error && text(&out).contains("EVAL_BRIDGE_DENIED"));
        let unbound = EvalTool::new(dir.path());
        let out = unbound.execute("unbound",json!({"kernel":"js","code":code("js",&json!({"prompt":"x"}))}),None).await.unwrap();
        assert!(out.is_error && text(&out).contains("PI_COMPLETION_UNBOUND"));
    });
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(tool.effects_for_input(&json!({"code":"42"})).networks());
}

#[test]
fn dropping_an_active_completion_drops_the_provider_stream_without_replay() {
    let provider = fixture([Reply::Pending]);
    let mut invocation = Invocation::new(Some(binding(&provider)), &json!({}), None).unwrap();
    runtime().block_on(async {
        let mut pending = Box::pin(invocation.complete(Path::new("."), json!({"prompt":"x"})));
        assert!(matches!(futures::poll!(pending.as_mut()), Poll::Pending));
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        drop(pending);
        assert!(provider.dropped.load(Ordering::Acquire));
    });
    assert_eq!(invocation.receipts[0]["status"], "incomplete");
    assert_eq!(invocation.receipts[0]["usageKnown"], false);
}

#[test]
fn public_cell_deadline_owns_completion_and_retired_kernel_state() {
    let dir = tempfile::tempdir().unwrap();
    let provider = fixture([Reply::Pending]);
    let tool = EvalTool::new(dir.path()).with_judge(binding(&provider));
    let started = std::time::Instant::now();
    runtime().block_on(async {
        let out = tool.execute("timeout",json!({"kernel":"js","timeout_secs":2,
            "code":"globalThis.beforeCompletion=42; await tool.call('__pi_complete',{prompt:'x'})"}),None).await.unwrap();
        assert!(out.is_error,"{}",text(&out));
        assert!(text(&out).contains("EVAL_TIMEOUT"));
        assert!(provider.dropped.load(Ordering::Acquire));
        assert_eq!(out.details.unwrap()["judgments"]["attempts"][0]["status"],"incomplete");
        let next = tool.execute("fresh",json!({"kernel":"js","code":"typeof beforeCompletion"}),None).await.unwrap();
        assert_eq!(text(&next).trim(),"undefined");
    });
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[test]
fn completed_and_caught_requests_remain_in_host_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let provider = fixture([Reply::Failed, Reply::Text("discarded".into())]);
    let tool = EvalTool::new(dir.path()).with_judge(binding(&provider));
    let out = runtime().block_on(tool.execute("catch",json!({"kernel":"js","code":
        "try {await tool.call('__pi_complete',{prompt:'first'})} catch(e){}; await tool.call('__pi_complete',{prompt:'second'}); 'handled'"
    }),None)).unwrap();
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(text(&out).trim(), "handled");
    let details = out.details.unwrap();
    assert_eq!(details["judgments"]["attempts"][0]["status"], "failed");
    assert_eq!(details["judgments"]["attempts"][1]["status"], "completed");
    assert!(!details.to_string().contains("PRIVATE-PROVIDER-PAYLOAD"));
    assert!(!details.to_string().contains("discarded"));
    assert!(tool.description().contains("__pi_complete"));
    assert!(
        tool.parameters()["properties"]["judge_model"]["description"]
            .as_str()
            .unwrap()
            .contains("__pi_complete")
    );
}

fn extraction_request() -> Value {
    json!({"prompt":"Find the relevant path and count from the supplied evidence.",
        "schema":{"type":"object","required":["path","count"],"additionalProperties":false,
            "properties":{"path":{"type":"string"},"count":{"type":"integer","minimum":0}}}})
}

#[test]
fn typed_extraction_feeds_native_reads_and_retains_data_in_both_kernels() {
    for kernel in ["js", "python"] {
        if kernel == "python" && !python() {
            eprintln!("python3 unavailable");
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("evidence.txt"), "schema-payload").unwrap();
        let answer = "  {\"path\":\"evidence.txt\",\"count\":7}\n";
        let provider = fixture([Reply::Text(answer.into())]);
        let tool = EvalTool::new(dir.path()).with_judge(binding(&provider));
        let request = extraction_request();
        let source = if kernel == "js" {
            format!(
                "globalThis.typed = JSON.parse(await tool.call('__pi_complete', {request})); \
                const body = await tool.call('read', {{path:typed.data.path}}); \
                typed.schemaValid && typed.data.count === 7 && body.includes('schema-payload')"
            )
        } else {
            let encoded = serde_json::to_string(&request.to_string()).unwrap();
            format!(
                "import json\ntyped = json.loads(await tool.call('__pi_complete', json.loads({encoded})))\n\
                body = await tool.call('read', {{'path': typed['data']['path']}})\n\
                typed['schemaValid'] and typed['data']['count'] == 7 and 'schema-payload' in body"
            )
        };
        runtime().block_on(async {
            let out = tool
                .execute("typed", json!({"kernel":kernel,"code":source}), None)
                .await
                .unwrap();
            assert!(!out.is_error, "{}", text(&out));
            assert_eq!(text(&out).trim().to_ascii_lowercase(), "true");
            assert_eq!(
                out.details.unwrap()["judgments"]["attempts"][0]["status"],
                "completed"
            );
            let out = tool
                .execute(
                    "reuse",
                    json!({"kernel":kernel,"code":if kernel=="js" {
                "typed.data.count" } else { "typed['data']['count']" }}),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(text(&out).trim(), "7");
            assert!(out.details.unwrap().get("judgments").is_none());
        });
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the read/reuse must not regenerate data");
        let envelope: Value = serde_json::from_str(&requests[0].1).unwrap();
        assert_eq!(envelope["prompt"], request["prompt"]);
        assert_eq!(envelope["schema"], request["schema"]);
        assert!(requests[0].0.contains("exactly one JSON value"));
    }
}

#[test]
fn invalid_schema_and_expansion_are_refused_before_attempt_reservation() {
    let provider = fixture([]);
    let mut invocation = Invocation::new(Some(binding(&provider)), &json!({}), None).unwrap();
    runtime().block_on(async {
        for schema in [
            Value::Null,
            json!([]),
            json!({"type":"CANARY"}),
            json!({"$ref":"https://schema.invalid/CANARY"}),
            json!({"description":"x".repeat(16*1024)}),
        ] {
            let error = invocation
                .complete(Path::new("."), json!({"prompt":"x","schema":schema}))
                .await
                .unwrap_err();
            assert!(error.contains("PI_COMPLETION_SCHEMA"), "{error}");
            assert!(!error.contains("CANARY"));
        }
        let error = invocation
            .complete(
                Path::new("."),
                json!({"prompt":"\0".repeat(64*1024),"schema":true}),
            )
            .await
            .unwrap_err();
        assert!(error.contains("PI_COMPLETION_INPUT"));
    });
    assert!(invocation.receipts.is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[test]
fn schema_failures_consume_one_attempt_without_echoing_or_repairing_data() {
    let provider = fixture([
        Reply::Text(r#"{"path":false,"count":7,"secret":"OUTPUT-CANARY"}"#.into()),
        Reply::Text(r#"{"path":"a","path":"b","count":7}"#.into()),
        Reply::Text("```json\n{\"path\":\"a\",\"count\":7}\n```".into()),
        Reply::Text(r#"{"path":"a","count":7}"#.into()),
    ]);
    let mut invocation = Invocation::new(Some(binding(&provider)), &json!({}), None).unwrap();
    runtime().block_on(async {
        for _ in 0..3 {
            let error = invocation
                .complete(Path::new("."), extraction_request())
                .await
                .unwrap_err();
            assert!(error.contains("PI_COMPLETION_RESPONSE"), "{error}");
            assert!(!error.contains("OUTPUT-CANARY"));
        }
        let reply: Value = serde_json::from_str(
            &invocation
                .complete(Path::new("."), extraction_request())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(reply["data"], json!({"path":"a","count":7}));
        assert_eq!(reply["schemaValid"], true);
        assert_eq!(reply["usage"]["input"], 13);
    });
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        4,
        "no corrective generation"
    );
    assert_eq!(invocation.receipts.len(), 4);
    assert!(
        invocation.receipts[..3]
            .iter()
            .all(|receipt| receipt["status"] == "failed" && receipt["usageKnown"] == false)
    );
    assert!(
        !serde_json::to_string(&invocation.receipts)
            .unwrap()
            .contains("OUTPUT-CANARY")
    );
}

#[test]
fn structured_null_and_scalar_results_are_distinct_from_unrequested_data() {
    let provider = fixture([
        Reply::Text("null".into()),
        Reply::Text("false".into()),
        Reply::Text("  7 \n".into()),
        Reply::Text("{not JSON}".into()),
    ]);
    let judge = binding(&provider);
    runtime().block_on(async {
        for (schema, expected) in [
            (json!({"type":"null"}), Value::Null),
            (json!(true), json!(false)),
            (json!({"type":"integer"}), json!(7)),
        ] {
            let reply = judge
                .complete(CompletionRequest::parse(json!({"prompt":"x","schema":schema})).unwrap())
                .await
                .unwrap();
            assert_eq!(reply.get("data"), Some(&expected));
            assert_eq!(reply["schemaValid"], true);
            if expected == json!(7) {
                assert_eq!(reply["text"], "  7 \n");
            }
        }
        let reply = judge
            .complete(CompletionRequest::parse(json!({"prompt":"x"})).unwrap())
            .await
            .unwrap();
        assert_eq!(reply["text"], "{not JSON}");
        assert!(reply.get("data").is_none() && reply.get("schemaValid").is_none());
    });
}

#[test]
fn privacy_screens_schema_annotations_and_refuses_any_contract_change() {
    let provider = fixture([Reply::Text("\"valid\"".into())]);
    let judge = binding(&provider);
    runtime().block_on(async {
        let request = json!({"prompt":"PROMPT-CANARY","system":"SYSTEM-CANARY",
            "schema":{"type":"string","description":"SCHEMA-CANARY"}});
        let protect = |parts: &[&str]| -> crate::error::Result<Vec<String>> {
            assert_eq!(parts.len(), 2);
            assert!(parts[0].contains("SYSTEM-CANARY"));
            assert!(parts[1].contains("SCHEMA-CANARY") && parts[1].contains("PROMPT-CANARY"));
            Ok(parts
                .iter()
                .map(|part| part.replace("CANARY", "redacted"))
                .collect())
        };
        let mut invocation =
            Invocation::new(Some(judge.clone()), &json!({}), Some(&protect)).unwrap();
        let error = invocation
            .complete(Path::new("."), request)
            .await
            .unwrap_err();
        assert!(error.contains("PI_COMPLETION_PRIVACY"));
        assert!(!error.contains("CANARY"));
        assert_eq!(invocation.receipts[0]["status"], "failed");
        assert!(provider.requests.lock().unwrap().is_empty());

        let protect = |parts: &[&str]| -> crate::error::Result<Vec<String>> {
            Ok(parts
                .iter()
                .map(|part| part.replace("PROMPT-CANARY", "redacted evidence"))
                .collect())
        };
        let mut invocation = Invocation::new(Some(judge), &json!({}), Some(&protect)).unwrap();
        let reply: Value = serde_json::from_str(
            &invocation
                .complete(
                    Path::new("."),
                    json!({"prompt":"PROMPT-CANARY","schema":{"type":"string"}}),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(reply["data"], "valid");
    });
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].1.contains("CANARY"));
    assert_eq!(
        serde_json::from_str::<Value>(&requests[0].1).unwrap()["prompt"],
        "redacted evidence"
    );
}

#[test]
fn caught_schema_failure_retains_kernel_state_and_failed_native_receipt() {
    for kernel in ["js", "python"] {
        if kernel == "python" && !python() {
            eprintln!("python3 unavailable");
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let provider = fixture([Reply::Text("not JSON".into())]);
        let tool = EvalTool::new(dir.path()).with_judge(binding(&provider));
        let source = if kernel == "js" {
            "globalThis.kept=41; try { await tool.call('__pi_complete',{prompt:'x',schema:true}); } catch(e) { if(!String(e).includes('PI_COMPLETION_RESPONSE')) throw e; } kept+1"
        } else {
            "kept=41\ntry:\n await tool.call('__pi_complete',{'prompt':'x','schema':True})\nexcept Exception as e:\n assert 'PI_COMPLETION_RESPONSE' in str(e)\nkept+1"
        };
        runtime().block_on(async {
            let out = tool
                .execute("caught", json!({"kernel":kernel,"code":source}), None)
                .await
                .unwrap();
            assert!(!out.is_error, "{}", text(&out));
            assert_eq!(text(&out).trim(), "42");
            assert_eq!(
                out.details.unwrap()["judgments"]["attempts"][0]["status"],
                "failed"
            );
            let out = tool
                .execute("kept", json!({"kernel":kernel,"code":"kept"}), None)
                .await
                .unwrap();
            assert_eq!(text(&out).trim(), "41");
        });
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn dropping_structured_generation_releases_the_same_owned_stream() {
    let provider = fixture([Reply::Pending]);
    let mut invocation = Invocation::new(Some(binding(&provider)), &json!({}), None).unwrap();
    runtime().block_on(async {
        let mut pending = Box::pin(invocation.complete(Path::new("."), extraction_request()));
        assert!(matches!(futures::poll!(pending.as_mut()), Poll::Pending));
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        drop(pending);
        assert!(provider.dropped.load(Ordering::Acquire));
    });
    assert_eq!(invocation.receipts[0]["status"], "incomplete");
    assert_eq!(invocation.receipts[0]["usageKnown"], false);
}

#[test]
fn model_facing_guidance_exposes_typed_completion_data_without_new_authority() {
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    for term in [
        "schema?",
        "schemaValid=true",
        "typed extraction",
        "External schema resources",
    ] {
        assert!(tool.description().contains(term), "{term}");
    }
    assert!(
        tool.parameters()["properties"]["judge_model"]["description"]
            .as_str()
            .unwrap()
            .contains("schema-validated")
    );
    assert!(!tool.effects_for_input(&json!({"code":"42"})).networks());
    assert!(
        tool.effects_for_input(&json!({"code":"42","judge_model":{"provider":"p","model":"m"}}))
            .networks()
    );
}
