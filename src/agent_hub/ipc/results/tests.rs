use super::*;
use serde_json::json;

fn event(case: &str) {
    eprintln!("{}", json!({"suite":"agent_result_channel","case":case,"phase":"start"}));
}

fn document(reference: &str, content: &str) -> Document {
    Document {
        reference: reference.to_string(), content: content.to_string(),
        content_type: "text/plain".to_string(), metadata: json!({"scope":"parentChannel"}),
    }
}

#[test]
fn result_envelopes_cannot_smuggle_messaging_or_sender_fields() {
    event("closed-envelope");
    let valid = json!({"version":1,"token":"a".repeat(64),"result":{"reference":"peer-1/value"}});
    assert!(serde_json::from_value::<super::super::Incoming>(valid.clone()).is_ok());
    for (field, value) in [("request",json!({"action":"send"})), ("sender",json!("parent"))] {
        let mut candidate = valid.clone();
        candidate[field] = value;
        assert!(serde_json::from_value::<super::super::Incoming>(candidate).is_err());
    }
    let mut candidate = valid;
    candidate["result"]["path"] = json!("/etc/passwd");
    assert!(serde_json::from_value::<super::super::Incoming>(candidate).is_err());
}

#[test]
fn wrong_credentials_fail_before_lookup_without_echoing_the_secret() {
    event("result-auth");
    let owner = AgentCx::for_testing_with_io();
    for (version, supplied) in [(1, "b".repeat(64)), (2, "a".repeat(64))] {
        let expected = "a".repeat(64);
        let request = WireRequest {
            version, token: supplied.clone(), result: ReadRequest { reference:"does-not-exist".into() },
        };
        let error = dispatch_authenticated("unregistered", &expected, request, &owner).unwrap_err().to_string();
        assert!(error.contains("PI_HUB_IPC_AUTH"));
        assert!(!error.contains(&expected) && !error.contains(&supplied));
    }
}

#[test]
fn a_historical_pid_or_terminal_status_cannot_authorize_result_access() {
    event("reader-lifetime");
    let temp = tempfile::tempdir().unwrap();
    let mut hub = AgentHubRegistry::default();
    hub.set_dir_for_tests(temp.path().to_path_buf());
    let entry = hub.register("result-reader", "lifetime fixture").unwrap();
    assert!(live_identity(&hub, &entry.id).is_err());
    hub.mark_running(&entry.id, 42);
    let identity = live_identity(&hub, &entry.id).unwrap();
    assert_eq!(identity, (entry.transcript_path, 42));
    hub.mark_process_reaped(&entry.id);
    assert_eq!(hub.get(&entry.id).unwrap().pid, Some(42));
    assert!(live_identity(&hub, &entry.id).is_err());
    let entry = hub.register("result-reader", "killed fixture").unwrap();
    hub.mark_running(&entry.id, 43);
    hub.mark_killed(&entry.id);
    assert!(live_identity(&hub, &entry.id).is_err());
}

#[test]
fn document_limit_counts_json_escaping_and_preserves_complete_payloads() {
    event("result-limit");
    let mut doc = document("peer-1/value", "");
    let overhead = serde_json::to_vec(&doc).unwrap().len();
    doc.content = "x".repeat(MAX_DOCUMENT_BYTES - overhead);
    assert!(enforce_document_limit(&doc).is_ok());
    doc.content.push('x');
    assert!(enforce_document_limit(&doc).is_err());
    doc.content = "\n".repeat(MAX_DOCUMENT_BYTES / 2);
    assert!(enforce_document_limit(&doc).is_err());
    assert!(reference_valid(&"r".repeat(MAX_REFERENCE_BYTES + 1)).is_err());
}

pub(super) fn serve(response: Vec<u8>) -> (SocketAddr, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < until => {
                    std::thread::sleep(IDLE_POLL);
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut input = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let count = socket.read(&mut chunk).unwrap();
            assert!(count != 0);
            input.extend_from_slice(&chunk[..count]);
            assert!(input.len() <= MAX_REQUEST_BYTES);
            if input.ends_with(b"\n") { break; }
        }
        let request: WireRequest = serde_json::from_slice(&input).unwrap();
        assert_eq!(request.result.reference, "peer-1/value");
        // Fragment the real socket response. A closed client on an intentional
        // refusal may stop the fixture writer; it does not require a retry.
        for chunk in response.chunks(47) {
            if socket.write_all(chunk).is_err() { break; }
        }
    });
    (address, handle)
}

fn request_response(response: WireResponse) -> Result<ResolvedDoc> {
    let mut response = serde_json::to_vec(&response).unwrap();
    response.push(b'\n');
    let (address, handle) = serve(response);
    let owner = AgentCx::for_testing_with_io();
    let result = resolve_at(&owner, address, "a".repeat(64), "peer-1/value", read_deadline(&owner).unwrap());
    handle.join().unwrap();
    result
}

#[test]
fn client_reassembles_fragmented_utf8_and_keeps_parent_failure_taxonomy() {
    event("fragmented-result");
    let value = "alpha\n雪🙂\nbeta".repeat(64);
    let resolved = request_response(WireResponse::Ok(document("peer-1/value", &value))).unwrap();
    assert_eq!(resolved.content, value);
    assert_eq!(resolved.scheme, "agent");
    assert_eq!(resolved.metadata["scope"], "parentChannel");
    let error = request_response(WireResponse::Error("PI_URL_AGENT_OUTPUT_REJECTED: failed child".into())).unwrap_err();
    assert!(error.to_string().contains("PI_URL_AGENT_OUTPUT_REJECTED"));
}

#[test]
fn mismatched_oversized_and_incomplete_responses_are_not_documents() {
    event("bad-result-response");
    let error = request_response(WireResponse::Ok(document("different-peer/value", "wrong result"))).unwrap_err();
    assert!(error.to_string().contains("PI_HUB_IPC_PROTOCOL"));
    let error = request_response(WireResponse::Ok(document("peer-1/value", &"x".repeat(MAX_COMPLETE_BYTES)))).unwrap_err();
    assert!(error.to_string().contains("PI_HUB_IPC_RESULT_LIMIT"));
    let (address, handle) = serve(b"{\"status\":\"ok\"".to_vec());
    let owner = AgentCx::for_testing_with_io();
    let error = resolve_at(&owner, address, "a".repeat(64), "peer-1/value", read_deadline(&owner).unwrap()).unwrap_err();
    handle.join().unwrap();
    assert!(error.to_string().contains("PI_HUB_IPC_CLOSED"));
}

#[test]
fn cancelled_expired_and_nonlocal_clients_never_connect() {
    event("client-admission");
    let owner = AgentCx::for_testing_with_io();
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let error = resolve_at(&owner, address, "a".repeat(64), "peer-1/value", Instant::now()).unwrap_err();
    assert!(error.to_string().contains("PI_HUB_IPC_TIMEOUT"));
    owner.set_cancel_requested(true);
    let error = resolve_at(&owner, address, "a".repeat(64), "peer-1/value", Instant::now() + Duration::from_secs(1)).unwrap_err();
    assert!(error.to_string().contains("PI_HUB_IPC_CANCELLED"));
    assert!(listener.accept().is_err());
    let owner = AgentCx::for_testing_with_io();
    let error = resolve_at(&owner, "192.0.2.1:80".parse().unwrap(), "a".repeat(64), "peer-1/value", read_deadline(&owner).unwrap()).unwrap_err();
    assert!(error.to_string().contains("PI_HUB_IPC_CONFIG"));
}

/// Spawned by the native delegation fixture below; never changes this test
/// process's global environment. Its ordinary ReadTool reaches the parent via
/// exactly the inherited channel installed by ChildRunner.
#[test]
#[ignore = "invoked as an owned native reader subprocess by native_chain_reads_accepted_sibling_result"]
fn native_reader_process() {
    use crate::model::ContentBlock;
    use crate::tools::{ReadTool, Tool};
    let Some(root) = std::env::var_os("PI_RESULT_READER_FIXTURE") else { return; };
    let seed = std::env::var("PI_RESULT_READER_SEED").unwrap();
    let root = PathBuf::from(root);
    let roster = crate::url_router::resolve("agent://", &root).unwrap();
    assert_eq!(roster.metadata["scope"], "parentChannel");
    let roster: Value = serde_json::from_str(&roster.content).unwrap();
    let id = roster["agents"].as_array().unwrap().iter()
        .filter_map(|entry| entry["id"].as_str())
        .find(|id| id.starts_with(&format!("{seed}-"))).unwrap();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let tool = ReadTool::new(&root);
    let output = runtime.block_on(tool.execute("peer-result", json!({
        "path":format!("agent://{id}/answer"),"offset":2,"limit":1
    }), None)).unwrap();
    assert!(!output.is_error);
    let text = output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()), _ => None,
    }).collect::<Vec<_>>().join("\n");
    assert!(text.contains("beta") && !text.contains("alpha") && !text.contains("gamma"));
    let missing = crate::url_router::resolve(&format!("agent://{id}/missing"), &root).unwrap_err();
    assert!(missing.to_string().contains("PI_URL_AGENT_FIELD_NOT_FOUND"));
    let full = crate::url_router::resolve(&format!("agent://{id}/answer"), &root).unwrap();
    assert_eq!(full.content, std::fs::read_to_string(root.join("expected-answer.txt")).unwrap());
    let event = json!({"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop",
        "content":[{"type":"text","text":"accepted peer result read through native channel"}]}]});
    std::fs::write(root.join("reader-result.jsonl"), format!("{event}\n")).unwrap();
}

#[cfg(unix)]
#[test]
fn native_chain_reads_accepted_sibling_result() {
    run_native_chain("alpha\nbeta\ngamma");
}

#[cfg(unix)]
#[test]
fn native_chain_reads_large_sibling_result_without_reopening_or_truncation() {
    let answer = format!("alpha\nbeta\ngamma\n{}", "雪🙂x".repeat(20_000));
    assert!(answer.len() > MAX_COMPLETE_BYTES);
    run_native_chain(&answer);
}

#[cfg(unix)]
fn run_native_chain(answer: &str) {
    use crate::subagents::SubagentTool;
    use crate::tools::Tool;
    use std::os::unix::fs::PermissionsExt;
    event("native-peer-read");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    let global = dir.path().join("global");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(global.join("agents")).unwrap();
    std::fs::write(root.join("expected-answer.txt"), answer).unwrap();
    let terminal = json!({"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop",
        "content":[{"type":"text","text":json!({"answer":answer}).to_string()}]}]});
    std::fs::write(root.join("seed-result.jsonl"), format!("{terminal}\n")).unwrap();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let seed = format!("seed{suffix}");
    let reader = format!("reader{suffix}");
    for name in [&seed, &reader] {
        std::fs::write(global.join(format!("agents/{name}.md")), format!(
            "---\nname: {name}\ndescription: native result fixture\ntools: read,hub\n---\nReturn fixture output.\n"
        )).unwrap();
    }
    let quote = |value: &std::path::Path| format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"));
    let executable = std::env::current_exe().unwrap();
    let child = dir.path().join("child.sh");
    // Harness chatter goes to a fixture file, never native stdout. Only a
    // successful real ReadTool subprocess writes the final agent_end frame.
    let script = format!(r#"#!/bin/sh
set -eu
for arg do task="$arg"; done
case "$task" in
  'Task: SEED')
    cat seed-result.jsonl
    ;;
  *)
    PI_RESULT_READER_FIXTURE={root} PI_RESULT_READER_SEED='{seed}' {executable} --exact agent_hub::ipc::results::tests::native_reader_process --ignored --nocapture > reader-harness.log 2>&1
    cat reader-result.jsonl
    ;;
esac
"#, root=quote(&root), executable=quote(&executable));
    std::fs::write(&child, script).unwrap();
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
    let tool = SubagentTool::with_paths(root.clone(), global, child).with_timeout(Duration::from_secs(15));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let output = runtime.block_on(tool.execute("native-peer-chain", json!({"scope":"user","chain":[
        {"agent":seed,"task":"SEED","outputSchema":{"type":"object"},"schemaMode":"strict"},
        {"agent":reader,"task":"READ"}
    ]}), None)).unwrap();
    assert!(!output.is_error, "{output:?}; harness: {}", std::fs::read_to_string(root.join("reader-harness.log")).unwrap_or_default());
    assert!(std::fs::read_to_string(root.join("reader-result.jsonl")).unwrap().contains("accepted peer result read"));
}
