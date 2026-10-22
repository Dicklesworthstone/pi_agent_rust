//! Actual QuickJS/Python -> native Eval host bridge; no live child/model claim.

use pi::error::{Error, Result};
use pi::eval::EvalTool;
use pi::model::{ContentBlock, TextContent, ToolCall};
use pi::tools::{Tool, ToolInvocationHost, ToolOutput};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

struct Host {
    calls: Mutex<Vec<Value>>,
    enabled: AtomicBool,
    lose_send: AtomicBool,
}

#[async_trait::async_trait]
impl ToolInvocationHost for Host {
    async fn call_tool(&self, call: ToolCall, expected_session: &str) -> Result<ToolOutput> {
        assert_eq!(expected_session, "agent-recovery-test");
        assert_eq!(call.name, "subagent");
        self.calls.lock().expect("calls").push(call.arguments.clone());
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(Error::tool("subagent", "current host denied"));
        }
        let args = call.arguments;
        let details = match args["operation"].as_str() {
            Some("worker_send") if self.lose_send.load(Ordering::SeqCst) => {
                return Err(Error::tool("subagent", "fixture acknowledgment lost"));
            }
            Some("worker_status" | "worker_wait" | "worker_send" | "worker_start") => {
                let turn = if args["operation"] == "worker_start" {
                    1
                } else if args["operation"] == "worker_send" {
                    4
                } else {
                    args["turnId"].as_u64().unwrap_or(3)
                };
                json!({
                    "schema":"pi.subagent.worker.v1", "workerId":"worker-a",
                    "jobId":"job-worker", "agent":"reviewer", "status":"idle", "turnId":turn,
                    "turn":{"id":turn, "status":"completed", "result":{
                        "content":[{"type":"text", "text":"retained answer"}],
                        "details":{"turn":turn}, "isError":false
                    }}
                })
            }
            Some("pool_status" | "pool_result" | "pool_wait") => {
                let mut value = json!({
                    "schema":"pi.subagent.pool.v1", "poolId":"pool-a", "jobId":"job-pool",
                    "agent":"reviewer", "status":"completed", "sealed":true, "concurrency":2,
                    "remainingMs":100, "admittedItems":1, "queuedItems":0, "runningItems":0,
                    "completedItems":1, "failedItems":0,
                    "items":[{"id":1, "status":"completed", "summary":"accepted",
                        "resultRef":{"poolId":"pool-a", "itemId":1}}]
                });
                if let Some(id) = args.get("itemId") {
                    value["itemId"] = id.clone();
                    value["item"] = json!({"id":id, "status":"completed", "result":{
                        "content":[{"type":"text", "text":"retained pool answer"}],
                        "details":{"item":id}, "isError":false
                    }});
                }
                value
            }
            Some("pool_drain") => {
                let before = args.get("cursor").filter(|value| !value.is_null())
                    .cloned().unwrap_or_else(|| json!({
                        "schema":"pi.subagent.pool.cursor.v1", "poolId":"pool-a", "itemIds":[]
                    }));
                let acknowledged = before["itemIds"] == json!([1]);
                json!({
                    "schema":"pi.subagent.pool.drain.v1", "poolId":"pool-a", "jobId":"job-pool",
                    "agent":"reviewer", "status":"completed", "sealed":true,
                    "admittedItems":1, "settledItems":1, "pendingItems":0,
                    "items":if acknowledged { json!([]) } else { json!([{
                        "id":1, "status":"completed", "result":{
                            "content":[{"type":"text", "text":"retained pool answer"}],
                            "details":{"item":1}, "isError":false
                        }
                    }]) },
                    "cursor":before, "nextCursor":{
                        "schema":"pi.subagent.pool.cursor.v1", "poolId":"pool-a", "itemIds":[1]
                    },
                    "hasMore":false, "done":true, "timedOut":false
                })
            }
            _ => return Err(Error::tool("subagent", "fixture forbids launch or unrequested mutation")),
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("native fixture result"))],
            details: Some(details),
            is_error: false,
        })
    }
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8).build().expect("native test runtime")
}
fn fixture(root: &std::path::Path) -> (EvalTool, Host) {
    let mut tool = EvalTool::new(root);
    tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed("agent-recovery-test"));
    (tool, Host {calls:Mutex::new(Vec::new()), enabled:AtomicBool::new(true), lose_send:AtomicBool::new(false)})
}
fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()), _ => None,
    }).collect()
}
async fn cell(tool: &EvalTool, host: &Host, code: &str, reset: bool) -> ToolOutput {
    let protect = |parts: &[&str]| Ok(parts.iter().map(|part| (*part).to_owned()).collect());
    tool.execute_with_host("recovery", json!({"kernel":"js", "code":code, "reset":reset}),
        None, &protect, host).await.expect("native eval outcome")
}

#[test]
fn reset_reconstructs_workers_and_pools_without_launch_or_replayed_tasks() {
    let root = tempfile::tempdir().expect("tempdir");
    let (tool, host) = fixture(root.path());
    runtime().block_on(async {
        let first = cell(&tool, &host, concat!(
            "globalThis.w = await agent.attachWorker('worker-a');",
            "globalThis.p = await workpool.attach('pool-a');"
        ), false).await;
        assert!(!first.is_error, "{}", text(&first));
        let second = cell(&tool, &host, concat!(
            "if (typeof w !== 'undefined') throw new Error('state retained across reset');",
            "globalThis.w = await agent.attachWorker('worker-a',{turnId:1});",
            "globalThis.p = await workpool.attach('pool-a');",
            "const t = await w.turn(2); const values = await wait([t,p.items[0]],{timeout:0});",
            "if (values[0].details.turn !== 2 || values[1].details.item !== 1) throw new Error('wrong result');"
        ), true).await;
        assert!(!second.is_error, "{}", text(&second));
        let calls = host.calls.lock().expect("calls");
        assert_eq!(calls.len(), 7);
        assert!(calls.iter().all(|call| matches!(call["operation"].as_str(),
            Some("worker_status" | "worker_wait" | "pool_status" | "pool_wait"))));
        assert_eq!(second.details.expect("details")["toolCalls"]["calls"].as_array().expect("receipts").len(), 5);
    });
}

#[test]
fn a_caught_send_error_still_fences_next_cell_submissions_and_retains_native_receipt() {
    let root = tempfile::tempdir().expect("tempdir");
    let (tool, host) = fixture(root.path());
    host.lose_send.store(true, Ordering::SeqCst);
    runtime().block_on(async {
        let first = cell(&tool, &host, concat!(
            "globalThis.w = await agent.attachWorker('worker-a',{allowSend:true});",
            "try { await w.send('possibly admitted'); } catch (error) {}",
            "if (!w.unknownAdmission || w.sendAllowed) throw new Error('lost admission not recorded');"
        ), false).await;
        assert!(!first.is_error, "{}", text(&first));
        assert_eq!(first.details.expect("details")["toolCalls"]["calls"][1]["status"], "failed");
        let retry = cell(&tool, &host, "await w.send('never replay')", false).await;
        assert!(retry.is_error);
        assert!(text(&retry).contains("RECOVERY_REQUIRED"));
        assert_eq!(host.calls.lock().expect("calls").len(), 2);
        let observed = cell(&tool, &host, "await w.output()", false).await;
        assert!(!observed.is_error, "{}", text(&observed));
    });
}

#[test]
fn recovered_handles_do_not_retain_an_earlier_host_permission() {
    let root = tempfile::tempdir().expect("tempdir");
    let (tool, host) = fixture(root.path());
    runtime().block_on(async {
        let first = cell(&tool, &host, "globalThis.p = await workpool.attach('pool-a');", false).await;
        assert!(!first.is_error, "{}", text(&first));
        host.enabled.store(false, Ordering::SeqCst);
        let denied = cell(&tool, &host, "await p.items[0].output()", false).await;
        assert!(denied.is_error);
        assert!(text(&denied).contains("current host denied"));
        assert_eq!(denied.details.expect("details")["toolCalls"]["calls"][0]["status"], "failed");
    });
}

#[test]
fn standalone_eval_cannot_acquire_subagent_authority_by_attaching() {
    let root = tempfile::tempdir().expect("tempdir");
    let tool = EvalTool::new(root.path());
    runtime().block_on(async {
        for code in ["await agent.attachWorker('worker-a')", "await workpool.attach('pool-a')"] {
            let output = tool.execute("standalone", json!({"kernel":"js", "code":code}), None)
                .await.expect("native eval outcome");
            assert!(output.is_error);
            assert!(text(&output).contains("EVAL_BRIDGE_DENIED"));
        }
    });
}

fn python_runtime() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|name| {
        std::process::Command::new(name)
            .args(["-c", "import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)"])
            .output().is_ok_and(|output| output.status.success())
    })
}

async fn python_cell(tool: &EvalTool, host: &Host, code: &str, reset: bool) -> ToolOutput {
    let protect = |parts: &[&str]| Ok(parts.iter().map(|part| (*part).to_owned()).collect());
    tool.execute_with_host("python-recovery", json!({"kernel":"python", "code":code, "reset":reset}),
        None, &protect, host).await.expect("native Python outcome")
}

#[test]
fn python_bootstrap_restores_existing_workers_and_pools_after_reset() {
    let Some(interpreter) = python_runtime() else {
        eprintln!("SKIP python recovery integration: Python >= 3.10 unavailable");
        return;
    };
    let root = tempfile::tempdir().expect("tempdir");
    let (_, host) = fixture(root.path());
    let mut tool = EvalTool::new_with_python(root.path(), interpreter).expect("explicit Python");
    tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed("agent-recovery-test"));
    runtime().block_on(async {
        let initial = python_cell(&tool, &host,
            "worker = await agent.attach_worker('worker-a')\npool = await workpool.attach('pool-a')", false).await;
        assert!(!initial.is_error, "{}", text(&initial));
        let recovered = python_cell(&tool, &host, concat!(
            "assert 'worker' not in globals()\n",
            "worker = await agent.attach_worker('worker-a', turn_id=1)\n",
            "pool = await workpool.attach('pool-a')\n",
            "earlier = await worker.turn(2)\n",
            "results = await wait([earlier, pool.items[0]], timeout=0)\n",
            "assert results[0]['details']['turn'] == 2\n",
            "assert results[1]['details']['item'] == 1\n",
            "assert worker.recovered and not worker.send_allowed and pool.recovered and pool.closed\n"
        ), true).await;
        assert!(!recovered.is_error, "{}", text(&recovered));
        assert_eq!(host.calls.lock().expect("calls").len(), 7);
        host.enabled.store(false, Ordering::SeqCst);
        let denied = python_cell(&tool, &host, "await pool.items[0].output()", false).await;
        assert!(denied.is_error);
        assert!(text(&denied).contains("current host denied"));
        assert_eq!(denied.details.expect("details")["toolCalls"]["calls"][0]["status"], "failed");
    });
}

#[test]
fn python_fresh_workers_fence_lost_send_replies_and_keep_existing_turns_readable() {
    let Some(interpreter) = python_runtime() else {
        eprintln!("SKIP python recovery integration: Python >= 3.10 unavailable");
        return;
    };
    let root = tempfile::tempdir().expect("tempdir");
    let (_, host) = fixture(root.path());
    let mut tool = EvalTool::new_with_python(root.path(), interpreter).expect("explicit Python");
    tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed("agent-recovery-test"));
    host.lose_send.store(true, Ordering::SeqCst);
    runtime().block_on(async {
        let first = python_cell(&tool, &host, concat!(
            "worker = await agent('seed', agent='reviewer', keep_alive=True)\n",
            "assert not worker.recovered and worker.send_allowed\n",
            "try:\n    await worker.send('possibly admitted')\n",
            "except Exception:\n    pass\n",
            "assert worker.unknown_admission and not worker.send_allowed\n"
        ), false).await;
        assert!(!first.is_error, "{}", text(&first));
        assert_eq!(first.details.expect("details")["toolCalls"]["calls"][1]["status"], "failed");
        let second = python_cell(&tool, &host, "await worker.send('do not replay')", false).await;
        assert!(second.is_error);
        assert!(text(&second).contains("RECOVERY_REQUIRED"));
        assert_eq!(host.calls.lock().expect("calls").len(), 2);
        let known = python_cell(&tool, &host, "assert (await worker.output())['details']['turn'] == 1", false).await;
        assert!(!known.is_error, "{}", text(&known));
    });
}

#[test]
fn recovered_pool_keeps_native_incremental_collection_in_both_kernels() {
    let scripts = [
        ("js", concat!(
            "globalThis.p = await workpool.attach('pool-a');",
            "const first = await p.drain({timeout:0});",
            "const repeated = await p.drain({timeout:0});",
            "const acknowledged = await p.drain({cursor:first.nextCursor,timeout:0});",
            "if (!p.closed || first.items[0].result.details.item !== 1 || repeated.items.length !== 1 ",
            "|| acknowledged.items.length !== 0 || !acknowledged.done) throw new Error('recovery drain failed');"
        )),
        ("python", concat!(
            "pool = await workpool.attach('pool-a')\n",
            "first = await pool.drain(timeout=0)\n",
            "repeated = await pool.drain(timeout=0)\n",
            "acknowledged = await pool.drain(cursor=first['nextCursor'], timeout=0)\n",
            "assert pool.closed and first['items'][0]['result']['details']['item'] == 1\n",
            "assert len(repeated['items']) == 1 and not acknowledged['items'] and acknowledged['done']\n"
        )),
    ];
    for (kernel, code) in scripts {
        let root = tempfile::tempdir().expect("tempdir");
        let (mut tool, host) = fixture(root.path());
        if kernel == "python" {
            let Some(interpreter) = python_runtime() else {
                eprintln!("SKIP Python recovery drain: Python >= 3.10 unavailable");
                continue;
            };
            tool = EvalTool::new_with_python(root.path(), interpreter).expect("explicit Python");
            tool.bind_job_session_scope(pi::jobs::JobSessionScope::fixed("agent-recovery-test"));
        }
        let protect = |parts: &[&str]| Ok(parts.iter().map(|part| (*part).to_owned()).collect());
        let output = runtime().block_on(tool.execute_with_host(
            "recovered-drain", json!({"kernel":kernel,"code":code}), None, &protect, &host,
        )).expect("native kernel outcome");
        assert!(!output.is_error, "{kernel}: {}", text(&output));
        let calls = host.calls.lock().expect("calls");
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0]["operation"], "pool_status");
        assert!(calls[1..].iter().all(|call| call["operation"] == "pool_drain"));
        assert!(calls[1].get("cursor").is_none());
        assert!(calls[2].get("cursor").is_none());
        assert_eq!(calls[3]["cursor"]["itemIds"], json!([1]));
    }
}
