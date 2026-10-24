//! Explicit, bounded workspace DAP configuration. Merely finding dap.json is
//! never permission to execute its commands: the caller must name both the
//! file and the adapter on a local launch/attach operation.

use super::adapters::{self, AdapterSpec};
use super::tool_err;
use crate::error::Result;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Component, Path};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_ADAPTERS: usize = 64;
const MAX_ARGUMENTS: usize = 128;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    adapters: BTreeMap<String, Definition>,
}

// Use OMP's documented field names, but refuse unsupported transports rather
// than accidentally treating a TCP/socket server as a stdio DAP stream.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Definition {
    command: Option<String>,
    args: Option<Vec<String>>,
    #[serde(default)]
    launch_defaults: Map<String, Value>,
    #[serde(default)]
    attach_defaults: Map<String, Value>,
    connect_mode: Option<String>,
    accepts_directory_program: Option<bool>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    file_types: Vec<String>,
    #[serde(default)]
    root_markers: Vec<String>,
}

pub(super) struct Selection {
    pub adapter: AdapterSpec,
    pub accepts_directory_program: bool,
    launch_defaults: Map<String, Value>,
    attach_defaults: Map<String, Value>,
}

impl Selection {
    fn registered(adapter: AdapterSpec) -> Self {
        Self {
            accepts_directory_program: adapter.id == "dlv",
            adapter,
            launch_defaults: Map::new(),
            attach_defaults: Map::new(),
        }
    }

    /// Defaults can add adapter-specific settings, but never overwrite the
    /// program, arguments, workspace, process identity or host-owned terminal.
    pub fn apply_defaults(&self, action: &str, arguments: &mut Value) {
        let defaults = if action == "launch" {
            &self.launch_defaults
        } else {
            &self.attach_defaults
        };
        if let Some(arguments) = arguments.as_object_mut() {
            for (key, value) in defaults {
                arguments
                    .entry(key.clone())
                    .or_insert_with(|| value.clone());
            }
        }
    }
}

pub(super) fn validate_request(
    path: Option<&str>,
    adapter: Option<&str>,
    action: &str,
    remote: bool,
) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if !matches!(action, "launch" | "attach") || remote {
        return Err(tool_err(
            "DAP_USAGE",
            "adapterConfig is local launch/pid-attach only",
        ));
    }
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\0')
        || !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
        || adapter.is_none_or(|id| !valid_id(id))
    {
        return Err(tool_err(
            "DAP_USAGE",
            "adapterConfig requires a workspace-relative path without parent traversal and an explicit adapter ID",
        ));
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[allow(clippy::too_many_lines)] // registered and file-configured selection share one validation path
pub(super) fn select(
    cwd: &Path,
    target: Option<&Path>,
    requested: Option<&str>,
    config: Option<&str>,
    registered: &[AdapterSpec],
) -> Result<Selection> {
    let Some(config) = config else {
        let adapter = adapters::select_adapter(target, requested, registered).ok_or_else(|| {
            tool_err(
                "DAP_ADAPTER_MISSING",
                "no matching registered debug adapter is available",
            )
        })?;
        adapters::dotnet::require_built_target(&adapter, target)?;
        return Ok(Selection::registered(adapter));
    };
    // Defense in depth for direct callers, before any filesystem access.
    validate_request(Some(config), requested, "launch", false)?;
    let id = requested.expect("validated explicit adapter ID");
    let root = std::fs::canonicalize(cwd)
        .map_err(|_| tool_err("DAP_CONFIG", "cannot resolve the debugger workspace"))?;
    let path = std::fs::canonicalize(root.join(config))
        .map_err(|_| tool_err("DAP_CONFIG", "cannot resolve adapterConfig"))?;
    if !path.starts_with(&root) {
        return Err(tool_err(
            "DAP_CONFIG",
            "adapterConfig resolves outside the debugger workspace",
        ));
    }
    let metadata = std::fs::metadata(&path)
        .map_err(|_| tool_err("DAP_CONFIG", "cannot inspect adapterConfig"))?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(tool_err(
            "DAP_CONFIG",
            "adapterConfig must be a regular file of at most 64 KiB",
        ));
    }
    let file = std::fs::File::open(&path)
        .map_err(|_| tool_err("DAP_CONFIG", "cannot open adapterConfig"))?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(tool_err(
            "DAP_CONFIG",
            "adapterConfig is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| tool_err("DAP_CONFIG", "cannot read adapterConfig"))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(tool_err("DAP_CONFIG", "adapterConfig exceeds 64 KiB"));
    }
    let mut document: Document = serde_json::from_slice(&bytes)
        // Do not echo configuration text or serde's offending value: it may
        // contain credentials in arguments, env or adapter-specific defaults.
        .map_err(|_| {
            tool_err(
                "DAP_CONFIG",
                "invalid dap.json: expected an adapters object with supported typed fields",
            )
        })?;
    if document.adapters.is_empty()
        || document.adapters.len() > MAX_ADAPTERS
        || document.adapters.keys().any(|id| !valid_id(id))
    {
        return Err(tool_err(
            "DAP_CONFIG",
            "dap.json requires 1..=64 adapters with valid IDs",
        ));
    }
    let definition = document.adapters.remove(id).ok_or_else(|| {
        tool_err(
            "DAP_CONFIG",
            "selected adapter is not defined in adapterConfig",
        )
    })?;
    validate_definition(id, &definition)?;
    let inherited = adapters::select_adapter(None, Some(id), registered);
    let mut adapter = inherited.unwrap_or_else(|| AdapterSpec {
        id: id.to_owned(),
        command_candidates: Vec::new(),
        adapter_args: Vec::new(),
        languages: Vec::new(),
        install_hint: "check the explicitly selected dap.json adapter command".into(),
    });
    if let Some(command) = definition.command {
        let command = if Path::new(&command).is_relative()
            && (command.contains('/') || (cfg!(windows) && command.contains('\\')))
        {
            root.join(command)
                .to_str()
                .ok_or_else(|| {
                    tool_err(
                        "DAP_CONFIG",
                        "configured command path must be representable as UTF-8",
                    )
                })?
                .to_owned()
        } else {
            command
        };
        adapter.command_candidates = vec![command];
    }
    if let Some(args) = definition.args {
        adapter.adapter_args = args;
    }
    if adapter.command_candidates.is_empty() {
        return Err(tool_err("DAP_CONFIG", "a new adapter requires command"));
    }
    adapters::dotnet::require_built_target(&adapter, target)?;
    let accepts_directory_program = definition
        .accepts_directory_program
        .unwrap_or(adapter.id == "dlv");
    tracing::info!(target: "debug", reason_code = "dap_config_selected", adapter_id = id,
        "Explicit workspace adapter configuration selected");
    Ok(Selection {
        adapter,
        accepts_directory_program,
        launch_defaults: definition.launch_defaults,
        attach_defaults: definition.attach_defaults,
    })
}

fn validate_definition(id: &str, definition: &Definition) -> Result<()> {
    if definition
        .command
        .as_ref()
        .is_some_and(|value| value.trim().is_empty() || value.len() > 4096 || value.contains('\0'))
        || definition.args.as_ref().is_some_and(|args| {
            args.len() > MAX_ARGUMENTS
                || args
                    .iter()
                    .any(|arg| arg.len() > 16 * 1024 || arg.contains('\0'))
        })
        || [
            &definition.languages,
            &definition.file_types,
            &definition.root_markers,
        ]
        .iter()
        .any(|values| {
            values.len() > 128
                || values
                    .iter()
                    .any(|value| value.is_empty() || value.len() > 4096 || value.contains('\0'))
        })
    {
        return Err(tool_err(
            "DAP_CONFIG",
            "invalid or oversized adapter command, arguments or metadata",
        ));
    }
    if definition
        .connect_mode
        .as_deref()
        .is_some_and(|mode| mode != if id == "dlv" { "tcp" } else { "stdio" })
    {
        return Err(tool_err(
            "DAP_UNSUPPORTED",
            "configured adapters use stdio; only the built-in Delve profile supports owned TCP",
        ));
    }
    // OMP's older CodeLLDB definition supplies --port 0. Merely declaring
    // connectMode=stdio does not undo that argv choice: fail before spawn,
    // rather than hanging the native reader on a TCP server's stdout.
    if id == "codelldb"
        && definition.args.as_ref().is_some_and(|args| {
            args.iter().any(|arg| {
                arg == "--"
                    || matches!(
                        arg.split('=').next().unwrap_or(arg),
                        "--port" | "--connect" | "--multi-session"
                    )
            })
        })
    {
        return Err(tool_err(
            "DAP_UNSUPPORTED",
            "CodeLLDB uses native stdio; remove port, connect and multi-session arguments from adapterConfig",
        ));
    }
    for defaults in [&definition.launch_defaults, &definition.attach_defaults] {
        if defaults.len() > 128
            || defaults.keys().any(|key| {
                matches!(
                    key.as_str(),
                    "program"
                        | "args"
                        | "cwd"
                        | "pid"
                        | "processId"
                        | "mode"
                        | "output"
                        | "stopOnEntry"
                        | "stopAtBeginningOfMainSubprogram"
                        | "stopAtEntry"
                        | "console"
                        | "runInTerminal"
                        | "redirectOutput"
                        | "subProcess"
                        | "request"
                        | "type"
                )
            })
        {
            return Err(tool_err(
                "DAP_CONFIG",
                "adapter defaults cannot replace caller identity, execution mode, launch artifacts or terminal ownership",
            ));
        }
    }
    Ok(())
}

/// Keep the caller's stop policy independent of adapter-specific wire names.
pub(super) fn set_stop_on_entry(adapter: &str, arguments: &mut Value, stop: Option<bool>) {
    if let Some(stop) = stop {
        if adapter == "gdb" && arguments.get("program").is_some() {
            // Main-subprogram entry, not the ELF loader's first instruction.
            // Explicit false must clear BOTH flags; never leave a hidden stop.
            arguments["stopOnEntry"] = Value::Bool(false);
            arguments["stopAtBeginningOfMainSubprogram"] = Value::Bool(stop);
        } else if adapter == "netcoredbg" && arguments.get("program").is_some() {
            // netcoredbg does not read stopOnEntry. In particular, false must
            // replace the native true default instead of adding an ignored key.
            if let Some(object) = arguments.as_object_mut() {
                object.remove("stopOnEntry");
            }
            arguments["stopAtEntry"] = Value::Bool(stop);
        } else {
            arguments["stopOnEntry"] = Value::Bool(stop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
    fn write_config(root: &Path, value: Value) {
        std::fs::write(root.join("dap.json"), serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn selected(root: &Path, id: &str) -> Result<Selection> {
        select(root, None, Some(id), Some("dap.json"), &[])
    }

    fn logged(case: &str) {
        eprintln!("{}", json!({"event":"dap_config_test","case":case}));
    }

    #[test]
    fn explicit_selection_inherits_registered_arguments_and_merges_only_defaults() {
        logged("inherit_and_merge");
        let directory = tempfile::tempdir().unwrap();
        write_config(
            directory.path(),
            json!({"adapters":{"debugpy":{
                "command":"./venv/bin/python", "launchDefaults":{"justMyCode":false},
                "attachDefaults":{"logToFile":false}
            }}}),
        );
        let selection = selected(directory.path(), "debugpy").unwrap();
        assert_eq!(selection.adapter.adapter_args, ["-m", "debugpy.adapter"]);
        assert_eq!(
            Path::new(&selection.adapter.command_candidates[0]),
            directory
                .path()
                .canonicalize()
                .unwrap()
                .join("venv/bin/python")
        );
        let mut launch = json!({"program":"app.py","args":["a b"],"cwd":"workspace"});
        selection.apply_defaults("launch", &mut launch);
        assert_eq!(launch["justMyCode"], false);
        assert!(launch.get("logToFile").is_none());
        assert_eq!(launch["args"], json!(["a b"]));
        let mut attach = json!({"processId":42});
        selection.apply_defaults("attach", &mut attach);
        assert_eq!(attach, json!({"processId":42,"logToFile":false}));
    }

    #[test]
    fn custom_adapters_are_literal_and_never_selected_implicitly() {
        logged("explicit_literal");
        let directory = tempfile::tempdir().unwrap();
        write_config(
            directory.path(),
            json!({"adapters":{"custom":{
                "command":"/opt/debug adapter", "args":["a b","$(no-shell)",""],
                "acceptsDirectoryProgram":true,"connectMode":"stdio"
            }}}),
        );
        let selection = selected(directory.path(), "custom").unwrap();
        assert_eq!(selection.adapter.command_candidates, ["/opt/debug adapter"]);
        assert_eq!(selection.adapter.adapter_args, ["a b", "$(no-shell)", ""]);
        assert!(selection.accepts_directory_program);
        assert!(select(directory.path(), None, Some("custom"), None, &[]).is_err());
        assert!(select(directory.path(), None, None, Some("dap.json"), &[]).is_err());
        assert!(selected(directory.path(), "absent").is_err());
    }

    #[test]
    fn wrong_scope_paths_and_unsupported_transports_fail_before_execution() {
        logged("scope_and_transport");
        for (path, id, action, remote) in [
            ("../dap.json", Some("custom"), "launch", false),
            ("/dap.json", Some("custom"), "launch", false),
            ("dap.json", None, "launch", false),
            ("dap.json", Some("custom"), "threads", false),
            ("dap.json", Some("custom"), "attach", true),
            ("dap.json", Some("bad/id"), "launch", false),
        ] {
            assert!(validate_request(Some(path), id, action, remote).is_err());
        }
        let directory = tempfile::tempdir().unwrap();
        for mode in ["tcp", "socket", "invalid"] {
            write_config(
                directory.path(),
                json!({"adapters":{"custom":{"command":"adapter","connectMode":mode}}}),
            );
            assert!(
                selected(directory.path(), "custom")
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("DAP_UNSUPPORTED")
            );
        }
    }

    #[test]
    fn configuration_cannot_redirect_host_owned_startup_fields() {
        logged("owned_fields");
        let directory = tempfile::tempdir().unwrap();
        for key in [
            "program",
            "cwd",
            "args",
            "pid",
            "processId",
            "mode",
            "output",
            "stopOnEntry",
            "stopAtBeginningOfMainSubprogram",
            "stopAtEntry",
            "console",
            "runInTerminal",
            "redirectOutput",
            "subProcess",
            "request",
            "type",
        ] {
            for field in ["launchDefaults", "attachDefaults"] {
                let mut definition = json!({"command":"adapter"});
                let mut forbidden = Map::new();
                forbidden.insert(key.to_owned(), json!("forbidden"));
                definition[field] = Value::Object(forbidden);
                write_config(directory.path(), json!({"adapters":{"custom":definition}}));
                assert!(
                    selected(directory.path(), "custom").is_err(),
                    "accepted {field}.{key}"
                );
            }
        }
    }

    #[test]
    fn invalid_and_oversized_configuration_never_falls_back_or_echoes_secrets() {
        logged("invalid_and_bounded");
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("dap.json"),
            vec![b' '; usize::try_from(MAX_CONFIG_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(selected(directory.path(), "debugpy").is_err());
        write_config(
            directory.path(),
            json!({"adapters":{"debugpy":{"args":"SECRET-CONFIG-CANARY"}}}),
        );
        let error = selected(directory.path(), "debugpy")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("DAP_CONFIG"));
        assert!(!error.contains("SECRET-CONFIG-CANARY"));
        write_config(directory.path(), json!({"adapters":{"custom":{}}}));
        assert!(selected(directory.path(), "custom").is_err());
        write_config(
            directory.path(),
            json!({"adapters":{"custom":{"command":"adapter","args":["bad\u{0000}arg"]}}}),
        );
        assert!(selected(directory.path(), "custom").is_err());
    }

    #[test]
    fn codelldb_rejects_tcp_mode_arguments_even_when_config_claims_stdio() {
        logged("codelldb_transport_arguments");
        let directory = tempfile::tempdir().unwrap();
        for argument in [
            "--port",
            "--port=0",
            "--connect",
            "--connect=5000",
            "--multi-session",
            "--multi-session=true",
            "--",
        ] {
            write_config(
                directory.path(),
                json!({"adapters":{"codelldb":{
                    "command":"adapter", "args":[argument], "connectMode":"stdio"
                }}}),
            );
            let error = selected(directory.path(), "codelldb").err().unwrap();
            assert!(error.to_string().contains("DAP_UNSUPPORTED"));
        }
    }

    #[test]
    fn codelldb_command_only_override_keeps_native_stdio_and_custom_ids_are_not_restricted() {
        logged("codelldb_profile_inheritance");
        let directory = tempfile::tempdir().unwrap();
        write_config(
            directory.path(),
            json!({"adapters":{"codelldb":{"command":"./adapter"}}}),
        );
        assert!(
            selected(directory.path(), "codelldb")
                .unwrap()
                .adapter
                .adapter_args
                .is_empty()
        );
        write_config(
            directory.path(),
            json!({"adapters":{"codelldb":{
                "command":"adapter", "args":["--liblldb","/opt/LLDB library"]
            }}}),
        );
        assert_eq!(
            selected(directory.path(), "codelldb")
                .unwrap()
                .adapter
                .adapter_args,
            ["--liblldb", "/opt/LLDB library"]
        );
        write_config(
            directory.path(),
            json!({"adapters":{"custom":{"command":"adapter","args":["--port","0"]}}}),
        );
        assert_eq!(
            selected(directory.path(), "custom")
                .unwrap()
                .adapter
                .adapter_args,
            ["--port", "0"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn configuration_symlink_cannot_escape_workspace() {
        logged("symlink_boundary");
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_config(
            outside.path(),
            json!({"adapters":{"custom":{"command":"adapter"}}}),
        );
        std::os::unix::fs::symlink(
            outside.path().join("dap.json"),
            directory.path().join("dap.json"),
        )
        .unwrap();
        assert!(selected(directory.path(), "custom").is_err());
    }
}
