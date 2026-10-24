//! Public Eval -> native batch drain -> invocation host, with controlled job
//! snapshots. No provider or jobs-scheduler behavior is simulated as acceptance.

use pi::error::{Error, Result};
use pi::eval::EvalTool;
use pi::model::{ContentBlock, TextContent, ToolCall};
use pi::tools::{Tool, ToolInvocationHost, ToolOutput};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

struct Host {
    snapshot: Mutex<Value>,
    calls: Mutex<Vec<Value>>,
    enabled: AtomicBool,
}

#[async_trait::async_trait]
impl ToolInvocationHost for Host {
    async fn call_tool(&self, call: ToolCall, expected_session: &str) -> Result<ToolOutput> {
        assert_eq!(expected_session, "batch-drain-handles");
        assert_eq!(call.name, "jobs");
        assert_eq!(call.arguments["action"], "wait");
        assert_eq!(call.arguments["timeoutMs"], 0);
        self.calls.lock().expect("calls").push(call.arguments);
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(Error::tool("jobs", "fixture current host denied"));
        }
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                "controlled job observation",
            ))],
            details: Some(self.snapshot.lock().expect("snapshot").clone()),
            is_error: false,
        })
    }
}

fn verdict(index: usize) -> Value {
    json!({"schema":"pi.judgment.v1", "answers":{"safe":false, "source":index},
        "provider":"fixture", "modelId":"model", "usage":{"input":9,"output":3}})
}

fn partial(status: &str) -> Value {
    let interrupted = status != "running";
    json!({"schema":"pi.host_job.v1", "id":"batch-fixture", "command":"eval judgment_batch",
        "status":status, "result":{
            "content":[], "isError":interrupted, "details":{
                "schema":if interrupted {"pi.host_job.interrupted.v1"} else {"pi.host_job.progress.v1"},
                "partialResult":{"content":[], "details":{
                    "schema":"pi.eval.model-job.progress.v1", "operation":"judgment_batch",
                    "complete":false,"total":2,"completed":1,"succeeded":1,"failed":0,"queued":0,"running":1,
                    "items":[{"key":0,"status":"running"},{"key":1,"status":"completed","verdict":verdict(1)}],
                    "judgments":{"schema":"pi.eval.judgments.v1","attempts":[{},{}]}
                }}
            }
        }
    })
}

fn completed() -> Value {
    json!({"schema":"pi.host_job.v1", "id":"batch-fixture", "command":"eval judgment_batch",
        "status":"exited", "result":{"content":[], "details":{
            "schema":"pi.eval.model-job.result.v1", "operation":"judgment_batch",
            "result":{"schema":"pi.eval.judgment-batch.v1","total":2,"succeeded":2,"failed":0,"allOk":true,
                "items":[{"key":0,"ok":true,"verdict":verdict(0)},{"key":1,"ok":true,"verdict":verdict(1)}]},
            "judgments":{"schema":"pi.eval.judgments.v1","attempts":[{},{}]}
        }}}
    )
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("native runtime")
}
fn kernels() -> Vec<&'static str> {
    let mut result = vec!["js"];
    if std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        result.push("python");
    }
    result
}
fn fixture(root: &std::path::Path) -> (EvalTool, Host) {
    let mut tool = EvalTool::new(root);
    tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed("batch-drain-handles"));
    (
        tool,
        Host {
            snapshot: Mutex::new(partial("running")),
            calls: Mutex::new(Vec::new()),
            enabled: AtomicBool::new(true),
        },
    )
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
async fn cell(tool: &EvalTool, host: &Host, kernel: &str, code: &str, reset: bool) -> ToolOutput {
    let protect = |parts: &[&str]| Ok(parts.iter().map(|part| (*part).to_owned()).collect());
    tool.execute_with_host(
        "batch-drain",
        json!({"kernel":kernel,"code":code,"reset":reset}),
        None,
        &protect,
        host,
    )
    .await
    .expect("native eval outcome")
}
const JS_ATTACH: &str = "globalThis.batch = await judgeBatch.attach('batch-fixture');";
const PY_ATTACH: &str = "batch = await judge_batch.attach('batch-fixture')";

#[test]
fn both_kernels_replay_unacknowledged_pages_and_advance_only_an_explicit_cursor() {
    for kernel in kernels() {
        let root = tempfile::tempdir().expect("fixture");
        let (tool, host) = fixture(root.path());
        runtime().block_on(async {
            let setup = if kernel == "js" {JS_ATTACH} else {PY_ATTACH};
            assert!(!cell(&tool,&host,kernel,setup,false).await.is_error);
            let read = if kernel == "js" {
                "globalThis.first = await batch.drain({timeout:0}); const retry = await batch.drain({timeout:0}); if (JSON.stringify(first) !== JSON.stringify(retry)) throw Error('read consumed results'); globalThis.cursor = first.cursor; if(first.items[0].key !== 1 || first.items[0].verdict.answers.safe !== false) throw Error('wrong evidence');"
            } else {
                "first = await batch.drain(timeout=0)\nretry = await batch.drain(timeout=0)\nassert first == retry\ncursor = first['cursor']\nassert first['items'][0]['key'] == 1 and first['items'][0]['verdict']['answers']['safe'] is False"
            };
            let observed = cell(&tool,&host,kernel,read,false).await;
            assert!(!observed.is_error,"{kernel}: {}",text(&observed));
            *host.snapshot.lock().expect("snapshot") = completed();
            let next = if kernel == "js" {
                "const next = await batch.drain({cursor,timeout:0}); if(next.items.length !== 1 || next.items[0].index !== 0 || !next.complete || !next.done) throw Error('wrong delta'); globalThis.cursor = next.cursor; if((await batch.drain({cursor,timeout:0})).items.length) throw Error('acknowledged items repeated');"
            } else {
                "next_page = await batch.drain(cursor=cursor,timeout=0)\nassert len(next_page['items']) == 1 and next_page['items'][0]['index'] == 0\nassert next_page['complete'] and next_page['done']\ncursor = next_page['cursor']\nassert not (await batch.drain(cursor=cursor,timeout=0))['items']"
            };
            let output = cell(&tool,&host,kernel,next,false).await;
            assert!(!output.is_error,"{kernel}: {}",text(&output));
            assert_eq!(host.calls.lock().expect("calls").len(),5);
        });
    }
}

#[test]
fn reset_reinstalls_the_handle_method_and_accepts_an_externally_saved_cursor() {
    for kernel in kernels() {
        let root = tempfile::tempdir().expect("fixture");
        let (tool, host) = fixture(root.path());
        runtime().block_on(async {
            let setup = if kernel == "js" {
                format!("{JS_ATTACH} globalThis.cursor=(await batch.drain({{timeout:0}})).cursor; display(cursor);")
            } else {
                format!("{PY_ATTACH}\ncursor=(await batch.drain(timeout=0))['cursor']\ndisplay(cursor)")
            };
            let result = cell(&tool,&host,kernel,&setup,false).await;
            assert!(!result.is_error,"{}",text(&result));
            // Fixed SHA-256 vector for UTF-8 [0,1], independent of the native
            // encoder and guest variables. Keep persisted cursors interoperable.
            let cursor = json!({"schema":"pi.eval.batch-cursor.v1","jobId":"batch-fixture",
                "manifest":"463f2998327eb3a694145e6014444480b2235be84aa6cfd57871cc64f1cd816c","seen":[1]});
            *host.snapshot.lock().expect("snapshot") = completed();
            let code = if kernel == "js" {
                format!("if(typeof batch !== 'undefined') throw Error('reset retained variables'); {JS_ATTACH} const next=await batch.drain({{cursor:{cursor},timeout:0}}); if(next.items.length!==1 || next.items[0].index!==0) throw Error('cursor ignored');")
            } else {
                format!("assert 'batch' not in globals()\n{PY_ATTACH}\nimport json\nsaved=json.loads({:?})\nnext_page=await batch.drain(cursor=saved,timeout=0)\nassert len(next_page['items'])==1 and next_page['items'][0]['index']==0",cursor.to_string())
            };
            let output = cell(&tool,&host,kernel,&code,true).await;
            assert!(!output.is_error,"{kernel}: {}",text(&output));
            assert_eq!(host.calls.lock().expect("calls").len(),4);
        });
    }
}

#[test]
fn current_policy_refusal_is_recorded_and_a_retained_handle_cannot_bypass_it() {
    for kernel in kernels() {
        let root = tempfile::tempdir().expect("fixture");
        let (tool, host) = fixture(root.path());
        runtime().block_on(async {
            assert!(
                !cell(
                    &tool,
                    &host,
                    kernel,
                    if kernel == "js" { JS_ATTACH } else { PY_ATTACH },
                    false
                )
                .await
                .is_error
            );
            host.enabled.store(false, Ordering::SeqCst);
            let code = if kernel == "js" {
                "await batch.drain({timeout:0});"
            } else {
                "await batch.drain(timeout=0)"
            };
            let output = cell(&tool, &host, kernel, code, false).await;
            assert!(output.is_error);
            assert!(
                text(&output).contains("EVAL_BATCH_HOST"),
                "{}",
                text(&output)
            );
            assert_eq!(host.calls.lock().expect("calls").len(), 2);
            assert_eq!(
                output.details.expect("details")["toolCalls"]["calls"][0]["status"],
                "failed"
            );
        });
    }
}

#[test]
fn interrupted_evidence_remains_incomplete_and_invalid_guest_cursors_do_not_dispatch() {
    for kernel in kernels() {
        let root = tempfile::tempdir().expect("fixture");
        let (tool, host) = fixture(root.path());
        runtime().block_on(async {
            assert!(!cell(&tool,&host,kernel,if kernel=="js" {JS_ATTACH} else {PY_ATTACH},false).await.is_error);
            let invalid=if kernel=="js" {"await batch.drain({cursor:{schema:'bad'},timeout:0});"}
                else {"await batch.drain(cursor={'schema':'bad'},timeout=0)"};
            let refused=cell(&tool,&host,kernel,invalid,false).await;
            assert!(refused.is_error);assert!(text(&refused).contains("EVAL_BATCH_INPUT"));
            assert_eq!(host.calls.lock().expect("calls").len(),1);
            *host.snapshot.lock().expect("snapshot")=partial("killed");
            let code=if kernel=="js" {"const p=await batch.drain({timeout:0}); if(!p.done || !p.jobFailed || p.complete || p.items.length!==1) throw Error('false completion');"}
                else {"p=await batch.drain(timeout=0)\nassert p['done'] and p['jobFailed'] and not p['complete'] and len(p['items'])==1"};
            let output=cell(&tool,&host,kernel,code,false).await;
            assert!(!output.is_error,"{kernel}: {}",text(&output));
            assert_eq!(host.calls.lock().expect("calls").len(),2);
        });
    }
}
