//! Native jobs and policy-controlled host seams; no live model acceptance.
use super::*;
use crate::error::{Error, Result as PiResult};
use crate::model::{ContentBlock, TextContent, ToolCall};
use crate::text_completion::{AuxiliaryPrivacy, AuxiliaryPrivacySnapshot};
use crate::tools::{JobsTool, Tool, ToolInvocationHost, ToolOutput};
use futures::channel::oneshot;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

fn event(case: &str) {
    eprintln!(
        "{}",
        json!({"suite":"batch_drain","case":case,"phase":"start"})
    );
}
fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
}
fn privacy() -> AuxiliaryPrivacySnapshot {
    AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::default())
}
fn verdict(value: &str) -> Value {
    json!({"schema":"pi.judgment.v1","answers":{"evidence":{"value":value}}})
}
#[allow(clippy::needless_pass_by_value)] // fixture builder; callers pass owned json! keys
fn completed(key: Value, value: &str) -> Value {
    json!({"key":key,"status":"completed","verdict":verdict(value)})
}
#[allow(clippy::needless_pass_by_value)] // fixture builder; callers pass owned json! keys
fn waiting(key: Value) -> Value {
    json!({"key":key,"status":"running"})
}
fn receipts(count: usize) -> Value {
    json!({"schema":"pi.eval.judgments.v1","attempts":vec![json!({});count]})
}
#[allow(clippy::needless_pass_by_value)] // fixture builder; callers pass owned vec! literals
fn partial(items: Vec<Value>) -> ToolOutput {
    let count = |state| items.iter().filter(|item| item["status"] == state).count();
    let succeeded = count("completed");
    let failed = count("failed");
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(
            "accepted batch progress",
        ))],
        details: Some(
            json!({"schema":"pi.eval.model-job.progress.v1","operation":"judgment_batch","complete":false,
            "total":items.len(),"completed":succeeded+failed,"succeeded":succeeded,"failed":failed,
            "queued":count("queued"),"running":count("running"),"judgments":receipts(items.len()),"items":items}),
        ),
        is_error: false,
    }
}
#[allow(clippy::needless_pass_by_value)] // fixture builder; callers pass owned vec! literals
fn final_result(items: Vec<Value>) -> ToolOutput {
    let succeeded = items.iter().filter(|item| item["ok"] == true).count();
    let failed = items.len() - succeeded;
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new("settled batch"))],
        details: Some(
            json!({"schema":"pi.eval.model-job.result.v1","operation":"judgment_batch",
            "result":{"schema":"pi.eval.judgment-batch.v1","total":items.len(),"succeeded":succeeded,
                "failed":failed,"allOk":failed==0,"items":items},"judgments":receipts(succeeded+failed)}),
        ),
        is_error: failed != 0,
    }
}
#[allow(clippy::needless_pass_by_value)] // fixture builder; callers hand over the output
fn job(result: Option<ToolOutput>, status: &str) -> Value {
    json!({"schema":"pi.host_job.v1","id":"fixture","command":"eval judgment_batch","status":status,"result":result})
}
fn progress(items: Vec<Value>) -> Value {
    let output = ToolOutput {
        content: vec![],
        details: Some(json!({"schema":"pi.host_job.progress.v1","partialResult":partial(items)})),
        is_error: false,
    };
    job(Some(output), "running")
}
#[allow(clippy::needless_pass_by_value)] // callers pass owned json! requests inline
fn parsed_reply(value: &Value, input: Value) -> Value {
    let request = Request::parse(&input).unwrap();
    serde_json::from_str(
        &Observation::parse(value, &request)
            .unwrap()
            .reply(&request)
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn cursor_preflight_rejects_foreign_malformed_and_ambiguous_acknowledgments() {
    event("cursor-preflight");
    for input in [
        json!(null),
        json!({"jobId":""}),
        json!({"jobId":"a b"}),
        json!({"jobId":"a","extra":1}),
        json!({"jobId":"a","timeoutMs":-1}),
        json!({"jobId":"a","timeoutMs":600_001}),
        json!({"jobId":"a","timeoutMs":0.5}),
        json!({"jobId":"a","cursor":{}}),
    ] {
        assert!(Request::parse(&input).is_err(), "{input}");
    }
    let cursor = json!({"schema":CURSOR_SCHEMA,"jobId":"fixture","seen":[],"manifest":null});
    let mut request = json!({"jobId":"fixture","cursor":cursor,"timeoutMs":0});
    assert!(Request::parse(&request).is_ok());
    request["cursor"]["jobId"] = json!("other");
    assert!(Request::parse(&request).is_err());
    request["cursor"]["jobId"] = json!("fixture");
    request["cursor"]["manifest"] = json!("a".repeat(64));
    for indices in [
        json!([0, 0]),
        json!([16]),
        json!([-1]),
        json!([false]),
        json!(["1"]),
    ] {
        request["cursor"]["seen"] = indices;
        assert!(Request::parse(&request).is_err());
    }
}

#[test]
fn out_of_order_results_are_keyed_replayable_and_cursor_acknowledged() {
    event("ordered-deltas");
    let first = progress(vec![
        waiting(json!("first")),
        completed(json!("雪/second"), "two"),
    ]);
    let request = json!({"jobId":"fixture","timeoutMs":0});
    let response = parsed_reply(&first, request.clone());
    assert_eq!(response["items"][0]["index"], 1);
    assert_eq!(response["items"][0]["key"], "雪/second");
    assert_eq!(response["cursor"]["seen"], json!([1]));
    assert_eq!(
        parsed_reply(&first, request),
        response,
        "same cursor is replayable, never destructive"
    );
    let cursor = response["cursor"].clone();
    assert_eq!(
        parsed_reply(&first, json!({"jobId":"fixture","cursor":cursor}))["items"],
        json!([])
    );
    let last = job(
        Some(final_result(vec![
            json!({"key":"first","ok":false,"error":"PI_JUDGE_PROVIDER"}),
            json!({"key":"雪/second","ok":true,"verdict":verdict("two")}),
        ])),
        "failed",
    );
    let response = parsed_reply(&last, json!({"jobId":"fixture","cursor":cursor}));
    assert_eq!(response["items"].as_array().unwrap().len(), 1);
    assert_eq!(response["items"][0]["ok"], false);
    assert_eq!(response["done"], true);
    assert_eq!(response["complete"], true);
    assert_eq!(response["jobFailed"], true);
    assert_eq!(response["cursor"]["seen"], json!([0, 1]));
}

#[test]
fn cursor_pins_key_types_order_and_only_settled_items() {
    event("manifest-identity");
    let value = progress(vec![completed(json!(0), "one"), waiting(json!(1))]);
    let response = parsed_reply(&value, json!({"jobId":"fixture"}));
    let input = json!({"jobId":"fixture","cursor":response["cursor"]});
    let request = Request::parse(&input).unwrap();
    let different = progress(vec![completed(json!("0"), "one"), waiting(json!("1"))]);
    assert!(
        Observation::parse(&different, &request)
            .unwrap_err()
            .contains("CURSOR")
    );
    let mut input = input;
    input["cursor"]["seen"] = json!([1]);
    assert!(Observation::parse(&value, &Request::parse(&input).unwrap()).is_err());
    let mut duplicate = value;
    duplicate["result"]["details"]["partialResult"]["details"]["items"][1]["key"] = json!(0);
    assert!(Observation::parse(&duplicate, &request).is_err());
}

#[test]
fn malformed_counts_pending_answers_and_wrong_job_kinds_are_refused() {
    event("protocol-shapes");
    let value = progress(vec![completed(json!(0), "one"), waiting(json!(1))]);
    let request = Request::parse(&json!({"jobId":"fixture"})).unwrap();
    for (pointer, replacement) in [
        ("/command", json!("eval model completion")),
        ("/id", json!("other")),
        ("/result/details/partialResult/details/succeeded", json!(2)),
        (
            "/result/details/partialResult/details/items/1/status",
            json!("completed"),
        ),
        (
            "/result/details/partialResult/details/complete",
            json!(true),
        ),
    ] {
        let mut malformed = value.clone();
        *malformed.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            Observation::parse(&malformed, &request).is_err(),
            "{pointer}"
        );
    }
    let mut malformed = value;
    malformed["result"]["details"]["partialResult"]["details"]["items"][1]["verdict"] =
        verdict("fabricated");
    assert!(Observation::parse(&malformed, &request).is_err());
}

#[test]
fn retries_are_unsettled_until_final_and_minimum_does_not_hide_failed_items() {
    event("retry-minimum");
    let mut running = progress(vec![waiting(json!(0)), completed(json!(1), "peer")]);
    let payload = &mut running["result"]["details"]["partialResult"]["details"];
    payload["retries"] = json!(1);
    payload["minOk"] = json!(1);
    payload["meetsMinimum"] = json!(true);
    payload["judgments"] = receipts(3);
    let first = parsed_reply(&running, json!({"jobId":"fixture"}));
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert_eq!(first["items"][0]["index"], 1);
    assert_eq!(first["complete"], false);
    let mut final_output = final_result(vec![
        json!({"key":0,"ok":false,"error":"PI_JUDGE_RESPONSE"}),
        json!({"key":1,"ok":true,"verdict":verdict("peer")}),
    ]);
    final_output.is_error = false;
    let details = final_output.details.as_mut().unwrap();
    details["result"]["retries"] = json!(1);
    details["result"]["minOk"] = json!(1);
    details["result"]["meetsMinimum"] = json!(true);
    details["judgments"] = receipts(3);
    let terminal = job(Some(final_output), "exited");
    let input = json!({"jobId":"fixture","cursor":first["cursor"]});
    let last = parsed_reply(&terminal, input.clone());
    assert_eq!(last["jobFailed"], false);
    assert_eq!(last["complete"], true);
    assert_eq!(last["items"][0]["ok"], false);
    assert_eq!(last["cursor"]["seen"], json!([0, 1]));
    let request = Request::parse(&input).unwrap();
    for (pointer, replacement) in [
        ("/result/details/result/meetsMinimum", json!(false)),
        ("/result/details/result/allOk", json!(true)),
        ("/result/details/result/retries", json!(0)),
        ("/result/details/result/minOk", json!(2)),
    ] {
        let mut invalid = terminal.clone();
        *invalid.pointer_mut(pointer).unwrap() = replacement;
        assert!(Observation::parse(&invalid, &request).is_err(), "{pointer}");
    }
}

#[test]
fn interrupted_and_withheld_evidence_do_not_fabricate_item_failures() {
    event("interruption");
    let first = progress(vec![completed(json!(0), "one"), waiting(json!(1))]);
    let cursor = parsed_reply(&first, json!({"jobId":"fixture"}))["cursor"].clone();
    let mut stopped = first;
    stopped["status"] = json!("killed");
    stopped["result"]["isError"] = json!(true);
    stopped["result"]["details"]["schema"] = json!("pi.host_job.interrupted.v1");
    let result = parsed_reply(&stopped, json!({"jobId":"fixture","cursor":cursor}));
    assert_eq!(result["done"], true);
    assert_eq!(result["complete"], false);
    assert_eq!(result["items"], json!([]));
    let withheld = job(
        Some(ToolOutput {
            content: vec![],
            details: None,
            is_error: true,
        }),
        "failed",
    );
    let result = parsed_reply(&withheld, json!({"jobId":"fixture","cursor":cursor}));
    assert_eq!(result["evidenceAvailable"], false);
    assert_eq!(result["total"], Value::Null);
    assert_eq!(result["cursor"], cursor);
}

struct Host {
    session: String,
    jobs: JobsTool,
    calls: AtomicUsize,
    enabled: AtomicBool,
    projected: AtomicBool,
}
#[async_trait::async_trait]
impl ToolInvocationHost for Host {
    async fn call_tool(&self, call: ToolCall, expected_session: &str) -> PiResult<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if expected_session != self.session
            || !self.enabled.load(Ordering::SeqCst)
            || call.name != "jobs"
        {
            return Err(Error::tool("jobs", "current host refused"));
        }
        let mut output = self.jobs.execute(&call.id, call.arguments, None).await?;
        if self.projected.load(Ordering::SeqCst)
            && let Some(details) = output.details.as_mut()
            && let Some(answer) = details.pointer_mut(
                "/result/details/partialResult/details/items/0/verdict/answers/evidence/value",
            )
        {
            *answer = json!("current-host-projected");
        }
        Ok(output)
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    tool: EvalTool,
    host: Host,
    id: String,
    owner: AgentCx,
    _release: oneshot::Sender<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.owner.set_cancel_requested(true);
    }
}
fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let session = format!("batch-drain-{}", uuid::Uuid::new_v4());
    let scope = crate::jobs::JobSessionScope::fixed(&session);
    let mut tool = EvalTool::new(root.path());
    tool.bind_job_session_scope(scope.clone());
    let mut jobs = JobsTool::new();
    jobs.bind_job_session_scope(scope);
    let owner = AgentCx::for_request();
    let (tx, rx) = oneshot::channel();
    let job = crate::jobs::spawn_owned_task(
        crate::jobs::admit_owned_task(&session).unwrap(),
        "eval judgment_batch",
        owner.clone(),
        Duration::from_secs(20),
        Some(root.path()),
        privacy(),
        async move {
            rx.await
                .map_err(|_| Error::tool("jobs", "fixture dropped"))?;
            Ok(final_result(vec![
                json!({"key":0,"ok":true,"verdict":verdict("finished")}),
            ]))
        },
    )
    .unwrap();
    Fixture {
        _root: root,
        tool,
        host: Host {
            session,
            jobs,
            calls: AtomicUsize::new(0),
            enabled: AtomicBool::new(true),
            projected: AtomicBool::new(false),
        },
        id: job.id,
        owner,
        _release: tx,
    }
}
fn invocation(fixture: &Fixture) -> Invocation<'_> {
    let mut invocation = Invocation::new(None, &json!({}), None)
        .unwrap()
        .with_host(Some(&fixture.host));
    invocation.bind_host_session(&fixture.host.session);
    invocation
}
fn publish(fixture: &Fixture, items: Vec<Value>) {
    crate::jobs::publish_owned_task_progress(
        &fixture.host.session,
        &fixture.id,
        partial(items),
        &privacy(),
    )
    .unwrap();
}
async fn host_called(host: &Host) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.calls.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        AgentCx::for_current_or_request()
            .time()
            .sleep(Duration::from_millis(5))
            .await;
    }
}
async fn cleanup(fixture: &Fixture) {
    crate::jobs::kill_session(&fixture.host.session)
        .await
        .unwrap();
}

#[test]
fn ready_results_use_one_host_call_and_no_model_reservation() {
    let _guard = crate::jobs::process_test_guard();
    event("native-ready");
    runtime().block_on(async {
        let fixture = fixture();
        publish(&fixture, vec![completed(json!(0), "native-body")]);
        let mut call = invocation(&fixture);
        let response: Value = serde_json::from_str(
            &call
                .drain_batch(&fixture.tool, json!({"jobId":fixture.id}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            response["items"][0]["verdict"]["answers"]["evidence"]["value"],
            "native-body"
        );
        assert_eq!(fixture.host.calls.load(Ordering::SeqCst), 1);
        assert!(call.receipts.is_empty());
        assert_eq!(call.tool_receipts.len(), 1);
        cleanup(&fixture).await;
    });
}

#[test]
fn native_wait_rechecks_host_projection_instead_of_returning_registry_bytes() {
    let _guard = crate::jobs::process_test_guard();
    event("native-policy-recheck");
    runtime().block_on(async {
        let fixture = fixture();
        let mut call = invocation(&fixture);
        let reader = call.drain_batch(&fixture.tool, json!({"jobId":fixture.id,"timeoutMs":3000}));
        let producer = async {
            host_called(&fixture.host).await;
            fixture.host.projected.store(true, Ordering::SeqCst);
            publish(&fixture, vec![completed(json!(0), "native-body")]);
        };
        let (reply, ()) = futures::join!(reader, producer);
        let reply = reply.unwrap();
        assert!(reply.contains("current-host-projected"));
        assert!(!reply.contains("native-body"));
        assert_eq!(fixture.host.calls.load(Ordering::SeqCst), 2);
        cleanup(&fixture).await;
    });
}

#[test]
fn revoking_host_access_while_waiting_withholds_new_evidence() {
    let _guard = crate::jobs::process_test_guard();
    event("revocation");
    runtime().block_on(async {
        let fixture = fixture();
        let mut call = invocation(&fixture);
        let reader = call.drain_batch(&fixture.tool, json!({"jobId":fixture.id,"timeoutMs":3000}));
        let producer = async {
            host_called(&fixture.host).await;
            fixture.host.enabled.store(false, Ordering::SeqCst);
            publish(&fixture, vec![completed(json!(0), "withheld-body")]);
        };
        let (reply, ()) = futures::join!(reader, producer);
        assert!(reply.unwrap_err().contains("EVAL_BATCH_HOST"));
        assert_eq!(call.tool_receipts[1]["status"], "failed");
        cleanup(&fixture).await;
    });
}

#[test]
fn timeout_cancellation_and_abandonment_do_not_cancel_or_consume_the_model_job() {
    let _guard = crate::jobs::process_test_guard();
    event("reader-lifetime");
    runtime().block_on(async {
        let fixture = fixture();
        let mut call = invocation(&fixture);
        assert!(
            call.drain_batch(&fixture.tool, json!({"jobId":fixture.id,"timeoutMs":30}))
                .await
                .unwrap_err()
                .contains("TIMEOUT")
        );
        let reader_owner = AgentCx::for_request();
        let mut pending = Box::pin(
            reader_owner.with_current(call.drain_batch(&fixture.tool, json!({"jobId":fixture.id}))),
        );
        assert!(futures::poll!(pending.as_mut()).is_pending());
        reader_owner.set_cancel_requested(true);
        assert!(pending.await.unwrap_err().contains("CANCELLED"));
        let mut abandoned = Box::pin(call.drain_batch(&fixture.tool, json!({"jobId":fixture.id})));
        assert!(futures::poll!(abandoned.as_mut()).is_pending());
        drop(abandoned);
        assert_eq!(
            crate::jobs::wait_async(&fixture.host.session, &fixture.id, Duration::ZERO)
                .await
                .unwrap()
                .status,
            "running"
        );
        publish(&fixture, vec![completed(json!(0), "still-available")]);
        let reply = invocation(&fixture)
            .drain_batch(&fixture.tool, json!({"jobId":fixture.id,"timeoutMs":0}))
            .await
            .unwrap();
        assert!(reply.contains("still-available"));
        cleanup(&fixture).await;
    });
}

#[test]
fn denied_unbound_and_exhausted_call_budgets_never_return_retained_data() {
    let _guard = crate::jobs::process_test_guard();
    event("admission");
    runtime().block_on(async {
        let fixture = fixture();
        publish(&fixture, vec![completed(json!(0), "protected")]);
        let input = json!({"jobId":fixture.id,"timeoutMs":0});
        let mut unbound = Invocation::new(None, &json!({}), None).unwrap();
        assert!(
            unbound
                .drain_batch(&fixture.tool, input.clone())
                .await
                .is_err()
        );
        let mut call = invocation(&fixture);
        call.tool_receipts
            .resize_with(super::super::MAX_TOOL_CALLS, || json!({}));
        assert!(
            call.drain_batch(&fixture.tool, input.clone())
                .await
                .is_err()
        );
        assert_eq!(fixture.host.calls.load(Ordering::SeqCst), 0);
        fixture.host.enabled.store(false, Ordering::SeqCst);
        assert!(
            invocation(&fixture)
                .drain_batch(&fixture.tool, input)
                .await
                .is_err()
        );
        assert_eq!(fixture.host.calls.load(Ordering::SeqCst), 1);
        cleanup(&fixture).await;
    });
}

#[test]
fn public_eval_bridge_delivers_deltas_in_both_kernels_and_retains_cursor_across_cells() {
    let _guard = crate::jobs::process_test_guard();
    event("public-kernels");
    runtime().block_on(async {
        for kernel in ["js","python"] {
            if kernel == "python" && !std::process::Command::new("python3").arg("--version")
                .output().is_ok_and(|output| output.status.success()) { continue; }
            let fixture = fixture();
            publish(&fixture,vec![completed(json!(0),"kernel-result")]);
            let first = if kernel == "js" {
                format!("globalThis.batchPage = JSON.parse(await tool.call('{}',{{jobId:{},timeoutMs:0}})); batchPage.items.length",BRIDGE_NAME,json!(fixture.id))
            } else {
                format!("import json\nbatch_page = json.loads(await tool.call('{}',{{'jobId':{},'timeoutMs':0}}))\nlen(batch_page['items'])",BRIDGE_NAME,json!(fixture.id))
            };
            let protect = |parts: &[&str]| crate::text_completion::redact_inputs(parts,&AuxiliaryPrivacy::default());
            let output = fixture.tool.execute_with_host("first",json!({"kernel":kernel,"code":first}),None,&protect,&fixture.host).await.unwrap();
            assert!(!output.is_error,"{:?}",output.content);
            assert_eq!(output.details.as_ref().unwrap()["toolCalls"]["calls"].as_array().unwrap().len(),1);
            let second = if kernel == "js" {
                format!("JSON.parse(await tool.call('{}',{{jobId:batchPage.jobId,cursor:batchPage.cursor,timeoutMs:0}})).items.length",BRIDGE_NAME)
            } else {
                format!("len(json.loads(await tool.call('{}',{{'jobId':batch_page['jobId'],'cursor':batch_page['cursor'],'timeoutMs':0}}))['items'])",BRIDGE_NAME)
            };
            let output = fixture.tool.execute_with_host("second",json!({"kernel":kernel,"code":second}),None,&protect,&fixture.host).await.unwrap();
            assert!(!output.is_error,"{:?}",output.content);
            assert!(output.content.iter().any(|block| matches!(block,ContentBlock::Text(text) if text.text.trim()=="0")));
            cleanup(&fixture).await;
        }
    });
}

#[test]
fn bounded_serialization_refuses_an_oversized_body_without_advancing_any_cursor() {
    event("output-limit");
    let value = progress(vec![completed(json!(0), &"x".repeat(1024 * 1024))]);
    let request = Request::parse(&json!({"jobId":"fixture"})).unwrap();
    assert!(
        Observation::parse(&value, &request)
            .unwrap()
            .reply(&request)
            .unwrap_err()
            .contains("LIMIT")
    );
    assert_eq!(request.seen, 0);
    assert!(request.manifest.is_none());
}
