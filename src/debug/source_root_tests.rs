//! Native tool calls and the existing DAP protocol peer. These tests verify
//! routing/ownership, not that a real language debugger hit a source line.

use super::*;
use crate::jobs::JobSessionScope;
use crate::tools::{ProcessCleanupMode, ProcessGuard};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Instant;

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
}

fn event(case: &str) {
    eprintln!("{}", json!({"event":"debug_source_root_test","case":case}));
}

fn fixture(root: &Path) -> Option<(DebugTool, String)> {
    let python = adapters::select_adapter(None, Some("debugpy"), &[])
        .and_then(|adapter| adapter.resolve_command());
    let Some(python) = python else {
        assert!(
            std::env::var_os("PI_DEBUG_REQUIRE_PROTOCOL").is_none(),
            "Python is required for native DAP protocol tests"
        );
        event("skipped_python_unavailable");
        return None;
    };
    let script = root.join("adapter.py");
    std::fs::write(&script, include_str!("test_adapter.py")).unwrap();
    for name in ["nested", "second"] {
        std::fs::create_dir(root.join(name)).unwrap();
        std::fs::write(root.join(name).join("program.py"), "value = 1\n".repeat(60)).unwrap();
    }
    // A same-named decoy makes wrong-root routing a successful-but-wrong
    // request on the baseline, rather than merely a missing-file failure.
    std::fs::write(root.join("program.py"), "decoy = 2\n".repeat(60)).unwrap();
    let definitions = [
        ("source-root-fixture", "normal"),
        ("rejected-start", "configuration_error"),
    ]
    .into_iter()
    .map(|(id, mode)| AdapterSpec {
        id: id.into(),
        command_candidates: vec![python.clone()],
        adapter_args: vec![
            "-I".into(),
            "-u".into(),
            script.to_str().unwrap().into(),
            mode.into(),
        ],
        languages: vec!["python"],
        install_hint: "repository protocol peer".into(),
    })
    .collect();
    let mut tool = DebugTool::new(root, None).with_adapters(definitions);
    tool.bind_job_session_scope(JobSessionScope::fixed("source-root-owner"));
    Some((tool, python))
}

async fn call(tool: &DebugTool, input: Value) -> Value {
    let output = tool.execute("source-root", input, None).await.unwrap();
    assert!(!output.is_error);
    output.details.unwrap()
}

fn launch(cwd: Option<&str>) -> Value {
    let mut input =
        json!({"action":"launch","program":"program.py","adapter":"source-root-fixture"});
    if let Some(cwd) = cwd {
        input["cwd"] = json!(cwd);
    }
    input
}

async fn capture(tool: &DebugTool) -> Vec<Value> {
    call(tool, json!({"action":"custom_request","command":"capture"})).await["result"]["requests"]
        .as_array()
        .unwrap()
        .clone()
}

fn source_requests(requests: &[Value]) -> Vec<&Value> {
    requests
        .iter()
        .filter(|entry| entry["command"] == "setBreakpoints")
        .collect()
}

async fn mutate_relative_breakpoints(tool: &DebugTool, expected_file: &Path) {
    let added = call(
        tool,
        json!({"action":"set_breakpoint","file":"program.py","line":20}),
    )
    .await;
    assert_eq!(added["file"], json!(expected_file));
    assert_eq!(added["count"], 2);
    let removed = call(
        tool,
        json!({"action":"remove_breakpoint","file":"./program.py","line":10}),
    )
    .await;
    assert_eq!(removed["file"], json!(expected_file));
    assert_eq!(removed["count"], 1);
    let requests = capture(tool).await;
    let sources = source_requests(&requests);
    assert_eq!(sources.len(), 3);
    for request in &sources {
        assert_eq!(request["arguments"]["source"]["path"], json!(expected_file));
    }
    assert_eq!(sources[0]["arguments"]["breakpoints"], json!([{"line":10}]));
    assert_eq!(
        sources[1]["arguments"]["breakpoints"],
        json!([{"line":10},{"line":20}])
    );
    assert_eq!(sources[2]["arguments"]["breakpoints"], json!([{"line":20}]));
    let cleared = call(
        tool,
        json!({"action":"remove_breakpoint","file":"program.py"}),
    )
    .await;
    assert_eq!(cleared["count"], 0);
}

#[test]
fn launch_retains_cwd_for_add_remove_and_clear_instead_of_a_same_named_decoy() {
    event("launch_relative_breakpoints");
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let Some((tool, _)) = fixture(&root) else {
        return;
    };
    runtime().block_on(async {
        let mut input = launch(Some("nested"));
        input["initialBreakpoints"] = json!([{"file":"program.py","line":10}]);
        let started = call(&tool, input).await;
        assert_eq!(
            started["threadState"]["sourceRoot"],
            json!(root.join("nested"))
        );
        mutate_relative_breakpoints(&tool, &root.join("nested/program.py")).await;
        call(&tool, json!({"action":"terminate"})).await;
        // The next launch intentionally omits cwd; no old root survives.
        let started = call(&tool, launch(None)).await;
        assert_eq!(started["threadState"]["sourceRoot"], json!(root));
        let added = call(
            &tool,
            json!({"action":"set_breakpoint","file":"program.py","line":30}),
        )
        .await;
        assert_eq!(added["file"], json!(root.join("program.py")));
        assert_eq!(added["count"], 1);
        call(&tool, json!({"action":"terminate"})).await;
    });
}

#[test]
fn pid_attach_retains_absolute_local_cwd_and_keeps_nonterminating_detach() {
    event("pid_attach");
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let Some((tool, _)) = fixture(&root) else {
        return;
    };
    runtime().block_on(async {
        call(
            &tool,
            json!({"action":"attach","pid":4242,"adapter":"source-root-fixture",
            "cwd":root.join("nested"),"initialBreakpoints":[{"file":"program.py","line":10}]}),
        )
        .await;
        mutate_relative_breakpoints(&tool, &root.join("nested/program.py")).await;
        let requests = capture(&tool).await;
        let attach = requests
            .iter()
            .find(|entry| entry["command"] == "attach")
            .unwrap();
        assert_eq!(attach["arguments"]["cwd"], json!(root.join("nested")));
        let detached = call(&tool, json!({"action":"disconnect"})).await;
        assert_eq!(detached["debuggeeTerminationRequested"], false);
        let args: Value =
            serde_json::from_slice(&std::fs::read(root.join("nested/disconnect.json")).unwrap())
                .unwrap();
        assert_eq!(args["terminateDebuggee"], false);
    });
}

#[test]
fn absolute_source_paths_stay_independent_and_missing_relative_files_never_fall_back() {
    event("absolute_and_missing_source");
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let Some((tool, _)) = fixture(&root) else {
        return;
    };
    runtime().block_on(async {
        call(&tool, launch(Some("nested"))).await;
        let relative = call(
            &tool,
            json!({"action":"set_breakpoint","file":"program.py","line":10}),
        )
        .await;
        let absolute = call(
            &tool,
            json!({"action":"set_breakpoint","file":root.join("program.py"),"line":20}),
        )
        .await;
        assert_eq!(relative["file"], json!(root.join("nested/program.py")));
        assert_eq!(absolute["file"], json!(root.join("program.py")));
        assert_eq!(relative["count"], 1);
        assert_eq!(absolute["count"], 1);
        // Move, do not delete, the nested source. A valid decoy still exists.
        std::fs::rename(root.join("nested/program.py"), root.join("nested/moved.py")).unwrap();
        // Deferred source binding is supported. Missing files retain their
        // lexical path under the same root, never the existing root decoy.
        let missing = call(
            &tool,
            json!({"action":"set_breakpoint","file":"program.py","line":30}),
        )
        .await;
        assert_eq!(missing["file"], json!(root.join("nested/program.py")));
        assert_eq!(missing["count"], 2);
        let removed = call(
            &tool,
            json!({"action":"remove_breakpoint","file":"program.py","line":30}),
        )
        .await;
        assert_eq!(removed["file"], json!(root.join("nested/program.py")));
        assert_eq!(removed["count"], 1);
        let requests = capture(&tool).await;
        let sources = source_requests(&requests);
        assert_eq!(sources.len(), 4);
        for source in &sources[2..] {
            assert_eq!(
                source["arguments"]["source"]["path"],
                json!(root.join("nested/program.py"))
            );
        }
        call(&tool, json!({"action":"terminate"})).await;
    });
}

fn select(scope: &JobSessionScope, id: &'static str) {
    scope.bind(Arc::new(move || {
        Box::pin(async move { Some(id.to_owned()) })
    }));
}

#[test]
fn parked_conversations_restore_their_own_source_root_and_breakpoint_set() {
    event("conversation_ownership");
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let Some((mut tool, _)) = fixture(&root) else {
        return;
    };
    let scope = JobSessionScope::fixed("owner-a");
    tool.bind_job_session_scope(scope.clone());
    runtime().block_on(async {
        let mut first = launch(Some("nested"));
        first["initialBreakpoints"] = json!([{"file":"program.py","line":10}]);
        call(&tool, first).await;
        select(&scope, "owner-b");
        call(&tool, launch(Some("second"))).await;
        call(
            &tool,
            json!({"action":"set_breakpoint","file":"program.py","line":40}),
        )
        .await;
        select(&scope, "owner-a");
        let sessions = call(&tool, json!({"action":"sessions"})).await;
        assert_eq!(
            sessions["sessions"][0]["execution"]["sourceRoot"],
            json!(root.join("nested"))
        );
        mutate_relative_breakpoints(&tool, &root.join("nested/program.py")).await;
        call(&tool, json!({"action":"terminate"})).await;
        select(&scope, "owner-b");
        let removed = call(
            &tool,
            json!({"action":"remove_breakpoint","file":"program.py","line":40}),
        )
        .await;
        assert_eq!(removed["file"], json!(root.join("second/program.py")));
        assert_eq!(removed["count"], 0);
        let requests = capture(&tool).await;
        for source in source_requests(&requests) {
            assert_eq!(
                source["arguments"]["source"]["path"],
                json!(root.join("second/program.py"))
            );
        }
        call(&tool, json!({"action":"terminate"})).await;
    });
}

#[test]
fn rejected_startup_cannot_leave_a_source_root_for_the_next_debugger() {
    event("failed_start_isolation");
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let Some((tool, _)) = fixture(&root) else {
        return;
    };
    runtime().block_on(async {
        let mut rejected = launch(Some("nested"));
        rejected["adapter"] = json!("rejected-start");
        assert!(tool.execute("reject", rejected, None).await.is_err());
        assert_eq!(
            call(&tool, json!({"action":"sessions"})).await["sessions"],
            json!([])
        );
        let started = call(&tool, launch(None)).await;
        assert_eq!(started["threadState"]["sourceRoot"], json!(root));
        let added = call(
            &tool,
            json!({"action":"set_breakpoint","file":"program.py","line":20}),
        )
        .await;
        assert_eq!(added["file"], json!(root.join("program.py")));
        call(&tool, json!({"action":"terminate"})).await;
    });
}

#[test]
fn invalid_port_attach_cwd_is_refused_before_connecting() {
    event("remote_cwd_preflight");
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("not-directory"), "file").unwrap();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let tool = DebugTool::new(directory.path(), None);
    runtime().block_on(async {
        for cwd in ["missing", "not-directory"] {
            let error = tool
                .execute(
                    "invalid-cwd",
                    json!({"action":"attach",
                "port":listener.local_addr().unwrap().port(),"cwd":cwd}),
                    None,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("DAP_CWD"), "{error}");
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            assert_eq!(
                call(&tool, json!({"action":"sessions"})).await["sessions"],
                json!([])
            );
        }
    });
}

fn remote_peer(root: &Path, python: &str) -> (ProcessGuard, u16) {
    let script = root.join("server.py");
    std::fs::write(&script, concat!(
        "import importlib.util, json, socket, sys\nfrom pathlib import Path\n",
        "peer_path = sys.argv[1]\nsys.argv = [peer_path, 'normal']\n",
        "spec = importlib.util.spec_from_file_location('peer', peer_path)\n",
        "peer = importlib.util.module_from_spec(spec)\nspec.loader.exec_module(peer)\n",
        "with socket.socket() as listener:\n",
        "    listener.bind(('127.0.0.1', 0))\n    listener.listen(1)\n    listener.settimeout(10)\n",
        "    Path('endpoint.json').write_text(json.dumps(listener.getsockname()))\n",
        "    with listener.accept()[0] as connection:\n",
        "        connection.settimeout(30)\n",
        "        with connection.makefile('rwb') as stream:\n",
        "            peer.INPUT = peer.OUTPUT = stream\n            peer.main()\n",
    )).unwrap();
    let mut command = Command::new(python);
    command
        .args(["-I", "-u"])
        .arg(&script)
        .arg(root.join("adapter.py"))
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let owner = AgentCx::for_request();
    let mut process = ProcessGuard::new(
        owner.process().spawn_checked(&mut command).unwrap(),
        ProcessCleanupMode::ProcessGroupTree,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(bytes) = std::fs::read(root.join("endpoint.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(port) = value[1].as_u64().and_then(|port| u16::try_from(port).ok())
        {
            return (process, port);
        }
        assert!(
            process.try_wait_child().unwrap().is_none(),
            "protocol peer exited before ready"
        );
        assert!(
            Instant::now() < deadline,
            "protocol peer readiness deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn port_attach_uses_local_source_cwd_without_rewriting_remote_cwd_or_mapping_roots() {
    event("port_attach_source_root");
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let Some((tool, python)) = fixture(&root) else {
        return;
    };
    let (_peer, port) = remote_peer(&root, &python);
    runtime().block_on(async {
        let started = call(
            &tool,
            json!({"action":"attach","port":port,"cwd":"nested",
            "pathMappings":[{"localRoot":"nested","remoteRoot":"/service/src"}],
            "initialBreakpoints":[{"file":"program.py","line":10}]}),
        )
        .await;
        assert_eq!(started["adapterProcessOwned"], false);
        assert_eq!(
            started["threadState"]["sourceRoot"],
            json!(root.join("nested"))
        );
        mutate_relative_breakpoints(&tool, &root.join("nested/program.py")).await;
        let requests = capture(&tool).await;
        let attach = requests
            .iter()
            .find(|entry| entry["command"] == "attach")
            .unwrap();
        assert!(attach["arguments"].get("cwd").is_none());
        assert_eq!(
            attach["arguments"]["pathMappings"],
            json!([
                {"localRoot":root.join("nested"),"remoteRoot":"/service/src"}
            ])
        );
        call(&tool, json!({"action":"disconnect"})).await;
        let args: Value =
            serde_json::from_slice(&std::fs::read(root.join("disconnect.json")).unwrap()).unwrap();
        assert_eq!(args["terminateDebuggee"], false);
    });
}

#[cfg(unix)]
#[test]
fn canonical_source_root_does_not_follow_a_retargeted_cwd_symlink() {
    event("canonical_root");
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let Some((tool, _)) = fixture(&root) else {
        return;
    };
    std::os::unix::fs::symlink(root.join("nested"), root.join("current")).unwrap();
    runtime().block_on(async {
        call(&tool, launch(Some("current"))).await;
        std::fs::rename(root.join("current"), root.join("old-link")).unwrap();
        std::os::unix::fs::symlink(root.join("second"), root.join("current")).unwrap();
        let added = call(
            &tool,
            json!({"action":"set_breakpoint","file":"program.py","line":10}),
        )
        .await;
        assert_eq!(added["file"], json!(root.join("nested/program.py")));
        let sessions = call(&tool, json!({"action":"sessions"})).await;
        assert_eq!(
            sessions["sessions"][0]["execution"]["sourceRoot"],
            json!(root.join("nested"))
        );
        call(&tool, json!({"action":"terminate"})).await;
    });
}
