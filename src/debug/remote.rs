//! Attach to an already-running debugpy DAP endpoint. The same DapSession owns
//! configuration, breakpoints, inspection and disconnect; only startup differs.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{AgentCx, DapSession, DebugInput, DebugTool, ExecState, ToolOutput};
use super::{breakpoints, dap::DapTransport, lock, text_output, tool_err};
use crate::error::Result;

const MAX_MAPPINGS: usize = 16;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AttachOptions {
    pub(super) port: Option<u16>,
    host: Option<String>,
    connect_timeout_ms: Option<u64>,
    path_mappings: Option<Vec<PathMapping>>,
    just_my_code: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PathMapping {
    local_root: String,
    remote_root: String,
}

impl AttachOptions {
    pub(super) fn validate(&self, input: &DebugInput) -> Result<()> {
        let requested = self.port.is_some() || self.host.is_some()
            || self.connect_timeout_ms.is_some() || self.path_mappings.is_some()
            || self.just_my_code.is_some();
        if !requested {
            return Ok(());
        }
        if input.action != "attach" || self.port.is_none()
            || input.pid.is_some() || input.program.is_some() || input.args.is_some()
            || input.go_mode.is_some() || input.stop_on_entry.is_some()
            || input.adapter.as_deref().is_some_and(|adapter| adapter != "debugpy")
        {
            return Err(tool_err("DAP_USAGE", "port attachment requires action=attach and an existing debugpy endpoint; do not combine it with pid, program, args, goMode, stopOnEntry or another adapter"));
        }
        self.endpoint()?;
        if self.connect_timeout_ms.is_some_and(|wait| !(1..=30_000).contains(&wait)) {
            return Err(tool_err("DAP_USAGE", "connectTimeoutMs must be in 1..=30000"));
        }
        if let Some(mappings) = &self.path_mappings {
            if mappings.len() > MAX_MAPPINGS {
                return Err(tool_err("DAP_USAGE", "pathMappings supports at most 16 entries"));
            }
            for mapping in mappings {
                for path in [&mapping.local_root, &mapping.remote_root] {
                    if path.trim().is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
                        return Err(tool_err("DAP_USAGE", "pathMappings roots must be nonempty, NUL-free and at most 4096 bytes"));
                    }
                }
            }
        }
        Ok(())
    }

    fn endpoint(&self) -> Result<SocketAddr> {
        let host = self.host.as_deref().unwrap_or("127.0.0.1");
        let address = if host == "localhost" {
            // Deliberate literal mapping; do not consult DNS or search domains.
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            host.parse::<IpAddr>().map_err(|_| tool_err(
                "DAP_USAGE", "host must be a loopback IP literal or localhost (IPv4)",
            ))?
        };
        let port = self.port.filter(|port| *port != 0)
            .ok_or_else(|| tool_err("DAP_USAGE", "port must be in 1..=65535"))?;
        if !address.is_loopback() {
            return Err(tool_err("DAP_USAGE", "only loopback debugger endpoints are supported; use a trusted local tunnel for remote services"));
        }
        Ok(SocketAddr::new(address, port))
    }

    fn mappings(&self, cwd: &Path) -> Result<Vec<Value>> {
        let Some(mappings) = self.path_mappings.as_ref().filter(|mappings| !mappings.is_empty()) else {
            return Ok(Vec::new());
        };
        let workspace = std::fs::canonicalize(cwd).map_err(|error| {
            tool_err("DAP_PATH_MAPPING", format!("cannot resolve the local workspace: {error}"))
        })?;
        let mut seen = BTreeSet::new();
        let mut resolved = Vec::with_capacity(mappings.len());
        for mapping in mappings {
            let local = std::fs::canonicalize(workspace.join(&mapping.local_root)).map_err(|error| {
                tool_err("DAP_PATH_MAPPING", format!("cannot resolve localRoot: {error}"))
            })?;
            if !local.is_dir() || !local.starts_with(&workspace) {
                return Err(tool_err("DAP_PATH_MAPPING", "localRoot must resolve to a directory inside this tool's workspace"));
            }
            if !seen.insert(local.clone()) {
                return Err(tool_err("DAP_PATH_MAPPING", "duplicate canonical localRoot; use one remote mapping for each local root"));
            }
            let local = local.to_str().filter(|path| path.len() <= MAX_PATH_BYTES)
                .ok_or_else(|| tool_err("DAP_PATH_MAPPING", "resolved localRoot must be UTF-8 and at most 4096 bytes"))?;
            // remoteRoot belongs to the server's OS. Never canonicalize it on
            // this host, fetch remote files, or assume it uses our separators.
            resolved.push(json!({"localRoot":local,"remoteRoot":mapping.remote_root}));
        }
        Ok(resolved)
    }

    fn arguments(&self, endpoint: SocketAddr, mappings: &[Value]) -> Value {
        json!({
            "type":"debugpy", "request":"attach", "name":"Pi loopback attach",
            "connect":{"host":endpoint.ip().to_string(),"port":endpoint.port()},
            "justMyCode":self.just_my_code.unwrap_or(true),
            "subProcess":false, "pathMappings":mappings,
        })
    }
}

impl DebugTool {
    /// Called only under normal ownership/admission, after capacity validation.
    pub(super) async fn run_remote_start(&self, input: &DebugInput) -> Result<ToolOutput> {
        input.remote.validate(input)?;
        let owner = AgentCx::for_current_or_request();
        owner.checkpoint().map_err(|_| tool_err("DAP_CANCELLED", "debugger attachment cancelled"))?;
        if !owner.capabilities().io || !owner.capabilities().time {
            return Err(tool_err("DAP_PERMISSION", "debugger attachment requires I/O and timer capabilities"));
        }
        // Complete local preflight before opening the user's selected endpoint.
        // cwd selects local source identity, not the remote process directory.
        // Configuration mapping roots remain anchored to the tool workspace.
        let source_root = self.resolve_working_directory(input.cwd.as_deref())?;
        let endpoint = input.remote.endpoint()?;
        let mappings = input.remote.mappings(&self.cwd)?;
        let initial = breakpoints::initial(&source_root, input.initial_breakpoints.as_deref().unwrap_or(&[]))?;
        let connect_ms = input.remote.connect_timeout_ms.unwrap_or(5_000);
        let startup_ms = input.startup_timeout_ms.unwrap_or(120_000);
        let transport = DapTransport::connect_loopback(endpoint, Duration::from_millis(connect_ms)).await?;
        let session = DapSession::begin(transport).await?.with_source_root(source_root);
        session.start(
            "attach", input.remote.arguments(endpoint, &mappings), &initial,
            input.exception_filters.as_deref(), Duration::from_millis(startup_ms),
        ).await?;
        owner.checkpoint().map_err(|_| tool_err("DAP_CANCELLED", "debugger attachment cancelled"))?;
        let state = session.state();
        let payload = json!({
            "action":"attach", "adapter":"debugpy", "transport":"tcp",
            "adapterProcessOwned":false, "endpoint":endpoint.to_string(),
            "connectTimeoutMs":connect_ms, "startupTimeoutMs":startup_ms,
            "pathMappings":mappings, "justMyCode":input.remote.just_my_code.unwrap_or(true),
            "state":match &state {
                ExecState::Stopped { .. } => "stopped",
                ExecState::Running => "running",
                ExecState::Exited => "exited",
            },
            "execution":state, "threadState":session.execution_snapshot(),
            "capabilities":session.capabilities(),
        });
        // An identity change after admission leaves this session with its
        // originating conversation; execute's existing publication check wins.
        *lock(&self.session) = Some(Arc::new(session));
        Ok(text_output(payload.to_string(), payload))
    }
}

pub(super) fn schema(mut schema: Value) -> Value {
    let properties = &mut schema["properties"];
    properties["port"] = json!({"type":"integer","minimum":1,"maximum":65535,"description":"attach only: existing loopback debugpy DAP port, instead of pid; does not spawn a local adapter"});
    properties["host"] = json!({"type":"string","default":"127.0.0.1","description":"Port attachment only: loopback IP or localhost (IPv4); use ::1 explicitly for IPv6. No DNS lookup."});
    properties["connectTimeoutMs"] = json!({"type":"integer","minimum":1,"maximum":30000,"default":5000,"description":"TCP connection budget, separate from initialize and startupTimeoutMs"});
    properties["justMyCode"] = json!({"type":"boolean","default":true,"description":"Port attachment only: debugpy user-code filtering"});
    properties["pathMappings"] = json!({
        "type":"array","maxItems":MAX_MAPPINGS,
        "description":"Port attachment only: maps workspace source directories to the server's source directories. Keep source copies synchronized.",
        "items":{"type":"object","required":["localRoot","remoteRoot"],"additionalProperties":false,
            "properties":{
                "localRoot":{"type":"string","minLength":1,"maxLength":MAX_PATH_BYTES},
                "remoteRoot":{"type":"string","minLength":1,"maxLength":MAX_PATH_BYTES},
            }},
    });
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: Value) -> DebugInput {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn port_attach_validates_before_any_endpoint_or_file_access() {
        for value in [
            json!({"action":"attach","port":0}),
            json!({"action":"attach","port":1,"pid":123}),
            json!({"action":"launch","port":1,"program":"app.py"}),
            json!({"action":"attach","host":"localhost"}),
            json!({"action":"attach","port":1,"adapter":"dlv"}),
            json!({"action":"attach","port":1,"stopOnEntry":false}),
            json!({"action":"attach","port":1,"connectTimeoutMs":0}),
            json!({"action":"attach","port":1,"connectTimeoutMs":30001}),
            json!({"action":"attach","port":1,"host":"192.0.2.1"}),
            json!({"action":"attach","port":1,"host":"example.invalid"}),
            json!({"action":"threads","pathMappings":[]}),
        ] {
            assert!(input(value.clone()).validate().is_err(), "accepted {value}");
        }
        for host in ["127.0.0.1", "localhost", "::1"] {
            let request = input(json!({"action":"attach","port":5678,"host":host}));
            request.validate().unwrap();
            assert!(request.remote.endpoint().unwrap().ip().is_loopback());
        }
        input(json!({"action":"attach","pid":1})).validate().unwrap();
        input(json!({"action":"sessions"})).validate().unwrap();
        assert!(serde_json::from_value::<DebugInput>(json!({"action":"attach","port":65536})).is_err());
    }

    #[test]
    fn mappings_are_workspace_local_and_do_not_reinterpret_remote_paths() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("source")).unwrap();
        let request = input(json!({"action":"attach","port":5678,"pathMappings":[
            {"localRoot":"source","remoteRoot":"C:\\service\\source"}
        ]}));
        request.validate().unwrap();
        let mappings = request.remote.mappings(workspace.path()).unwrap();
        assert_eq!(mappings[0]["remoteRoot"], "C:\\service\\source");
        assert_eq!(mappings[0]["localRoot"], workspace.path().join("source").canonicalize().unwrap().to_str().unwrap());
        let args = request.remote.arguments(request.remote.endpoint().unwrap(), &mappings);
        assert_eq!(args["subProcess"], false);
        assert_eq!(args["justMyCode"], true);
        assert!(args.get("processId").is_none());
        let outside = tempfile::tempdir().unwrap();
        let request = input(json!({"action":"attach","port":5678,"pathMappings":[
            {"localRoot":outside.path(),"remoteRoot":"."}
        ]}));
        assert!(request.remote.mappings(workspace.path()).is_err());
        let duplicate = input(json!({"action":"attach","port":5678,"pathMappings":[
            {"localRoot":"source","remoteRoot":"/a"},{"localRoot":"./source","remoteRoot":"/b"}
        ]}));
        assert!(duplicate.remote.mappings(workspace.path()).is_err());
    }

    #[test]
    fn malformed_or_oversized_mapping_is_not_silently_ignored() {
        assert!(serde_json::from_value::<DebugInput>(json!({"action":"attach","port":1,"pathMappings":[
            {"localRoot":".","remoteRoot":".","typo":true}
        ]})).is_err());
        for root in ["".to_string(), "x\0y".to_string(), "x".repeat(MAX_PATH_BYTES + 1)] {
            assert!(input(json!({"action":"attach","port":1,"pathMappings":[
                {"localRoot":".","remoteRoot":root}
            ]})).validate().is_err());
        }
        let mappings = vec![json!({"localRoot":".","remoteRoot":"."}); MAX_MAPPINGS + 1];
        assert!(input(json!({"action":"attach","port":1,"pathMappings":mappings})).validate().is_err());
    }

    /// Opt-in native acceptance: this talks through the actual Rust tool to a
    /// real Python debugger. The external test process is owned by the harness,
    /// not by the connected DapTransport; its guard cleans up after failures.
    #[test]
    #[ignore = "requires a real Python/debugpy installation; opt in through DSR"]
    fn live_debugpy_maps_breakpoints_inspects_changes_and_preserves_the_target() {
        use crate::tools::{ProcessCleanupMode, ProcessGuard, Tool, ToolRegistry};
        use std::process::{Command, Stdio};
        use std::time::Instant;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let local = root.join("local");
        let remote = root.join("remote");
        std::fs::create_dir(&local).unwrap();
        std::fs::create_dir(&remote).unwrap();
        let source = concat!(
            "import debugpy\nimport json\nimport sys\nimport time\nfrom pathlib import Path\n",
            "control = Path(sys.argv[1])\ndebugpy.configure(subProcess=False)\n",
            "address = debugpy.listen(('127.0.0.1', 0))\n",
            "(control / 'ready.json').write_text(json.dumps(address))\n",
            "debugpy.wait_for_client()\nanswer = 41\nanswer += 1\n",
            "(control / 'ran.json').write_text(str(answer))\n",
            "end = time.monotonic() + 20\n",
            "while not (control / 'finish').exists() and time.monotonic() < end:\n",
            "    time.sleep(0.01)\n",
        );
        std::fs::write(local.join("program.py"), source).unwrap();
        std::fs::write(remote.join("program.py"), source).unwrap();
        let python = std::env::var_os("PI_DEBUG_TEST_PYTHON").unwrap_or_else(|| "python3".into());
        let log = std::fs::File::create(root.join("debugpy.log")).unwrap();
        let mut command = Command::new(python);
        command.args(["-I", "-u"]).arg(remote.join("program.py")).arg(&root)
            .stdin(Stdio::null()).stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
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
        let started = Instant::now();
        let endpoint = loop {
            if let Ok(bytes) = std::fs::read(root.join("ready.json"))
                && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            {
                break value;
            }
            assert!(process.try_wait_child().unwrap().is_none(), "debugpy startup failed: {}",
                std::fs::read_to_string(root.join("debugpy.log")).unwrap_or_default());
            assert!(started.elapsed() < Duration::from_secs(15), "debugpy startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        };
        let registry = ToolRegistry::from_tools(vec![Box::new(
            DebugTool::new(&local, None).with_adapters(Vec::new()),
        )]);
        let tool = registry.get("debug").unwrap();
        async fn call(tool: &dyn Tool, input: Value) -> Value {
            let output = tool.execute("live-attach", input, None).await.unwrap();
            assert!(!output.is_error, "{output:?}");
            output.details.unwrap()
        }
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .blocking_threads(1, 8).build().unwrap();
        runtime.block_on(owner.with_current(async {
            let attached = call(tool, json!({"action":"attach","port":endpoint[1],
                "connectTimeoutMs":3000,"startupTimeoutMs":10000,
                "pathMappings":[{"localRoot":".","remoteRoot":remote}],
                "initialBreakpoints":[{"file":"program.py","line":12}]})).await;
            assert_eq!(attached["adapterProcessOwned"], false);
            let wait = Instant::now();
            loop {
                let sessions = call(tool, json!({"action":"sessions"})).await;
                if sessions["sessions"][0]["state"]["state"] == "stopped" { break; }
                assert!(wait.elapsed() < Duration::from_secs(5), "did not hit mapped breakpoint");
                owner.time().sleep(Duration::from_millis(10)).await;
            }
            assert!(!root.join("ran.json").exists(), "debuggee was not actually suspended");
            let stack = call(tool, json!({"action":"stack_trace"})).await;
            let frame = &stack["frames"][0];
            assert_eq!(frame["line"], 12);
            assert_eq!(Path::new(frame["source"]["path"].as_str().unwrap()).canonicalize().unwrap(),
                local.join("program.py"));
            let value = call(tool, json!({"action":"evaluate","frameId":frame["id"],"expression":"answer"})).await;
            assert_eq!(value["result"], "41");
            let scopes = call(tool, json!({"action":"scopes","frameId":frame["id"]})).await;
            let reference = &scopes["scopes"].as_array().unwrap().iter()
                .find(|scope| scope["name"] == "Locals").unwrap()["variablesReference"];
            let values = call(tool, json!({"action":"variables","variablesReference":reference})).await;
            assert!(values["variables"].as_array().unwrap().iter()
                .any(|value| value["name"] == "answer" && value["value"] == "41"));
            let changed = call(tool, json!({"action":"set_expression","frameId":frame["id"],
                "expression":"answer","value":"99"})).await;
            assert_eq!(changed["value"], "99");
            call(tool, json!({"action":"disconnect"})).await;
            assert_eq!(call(tool, json!({"action":"sessions"})).await["sessions"], json!([]));
        }));
        let wait = Instant::now();
        while std::fs::read_to_string(root.join("ran.json")).ok().as_deref() != Some("100") {
            assert!(process.try_wait_child().unwrap().is_none());
            assert!(wait.elapsed() < Duration::from_secs(5), "detached target did not resume");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(process.try_wait_child().unwrap().is_none(), "disconnect killed the external target");
        std::fs::write(root.join("finish"), "done").unwrap();
        let wait = Instant::now();
        loop {
            if let Some(status) = process.try_wait_child().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(wait.elapsed() < Duration::from_secs(10), "target did not exit under its own control");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
