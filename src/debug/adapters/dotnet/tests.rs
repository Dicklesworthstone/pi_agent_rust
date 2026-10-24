//! Structural image tests and native-tool protocol workflows. The synthetic
//! images contain recognition headers only; they are NOT executable assemblies.
//! The DAP peer tests do not establish live netcoredbg/.NET interoperability.

use super::*;
use crate::debug::adapters::{
    TargetKind, attach_arguments, classify_target, launch_arguments, select_adapter,
};
use crate::debug::configuration;
use serde_json::{Value, json};
use std::io::Cursor;

fn event(case: &str) {
    eprintln!("{}", json!({"event":"netcoredbg_regression","case":case}));
}

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn image(plus: bool) -> Vec<u8> {
    let mut bytes = vec![0; 0x600];
    bytes[..2].copy_from_slice(b"MZ");
    put32(&mut bytes, 60, 0x80);
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    put16(&mut bytes, 0x84, if plus { 0x8664 } else { 0x14c });
    put16(&mut bytes, 0x86, 1);
    put16(&mut bytes, 0x94, if plus { 0xf0 } else { 0xe0 });
    let optional = 0x98;
    put16(&mut bytes, optional, if plus { 0x20b } else { 0x10b });
    put32(&mut bytes, optional + if plus { 108 } else { 92 }, 16);
    let directory = optional + if plus { 224 } else { 208 };
    put32(&mut bytes, directory, 0x2000);
    put32(&mut bytes, directory + 4, 72);
    let section = optional + if plus { 0xf0 } else { 0xe0 };
    bytes[section..section + 5].copy_from_slice(b".text");
    put32(&mut bytes, section + 8, 0x400);
    put32(&mut bytes, section + 12, 0x2000);
    put32(&mut bytes, section + 16, 0x400);
    put32(&mut bytes, section + 20, 0x200);
    put32(&mut bytes, 0x200, 72);
    put16(&mut bytes, 0x204, 2);
    put16(&mut bytes, 0x206, 5);
    put32(&mut bytes, 0x208, 0x2080);
    put32(&mut bytes, 0x20c, 32);
    bytes[0x280..0x284].copy_from_slice(b"BSJB");
    bytes
}

fn recognized(bytes: &[u8]) -> bool {
    recognize(&mut Cursor::new(bytes), u64::try_from(bytes.len()).unwrap()).is_some()
}

#[test]
fn pe32_and_pe32_plus_are_recognized_by_content_not_extension() {
    event("managed_content");
    let directory = tempfile::tempdir().unwrap();
    for plus in [false, true] {
        let bytes = image(plus);
        assert!(recognized(&bytes));
        for name in [
            "application.dll",
            "application.exe",
            "APPLICATION.DLL",
            "application",
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(classify_target(&path), TargetKind::DotNet);
        }
    }
    let path = directory.path().join("native.dll");
    let mut native = image(false);
    put32(&mut native, 0x98 + 208, 0);
    std::fs::write(&path, native).unwrap();
    assert_eq!(classify_target(&path), TargetKind::NativeBinary);
    std::fs::write(&path, b"a .dll filename is not managed code").unwrap();
    assert_eq!(classify_target(&path), TargetKind::NativeBinary);
    assert!(!is_managed_image(directory.path()));
    assert!(!is_managed_image(&directory.path().join("absent.dll")));
}

#[test]
fn incomplete_headers_and_false_signatures_cannot_select_the_managed_backend() {
    event("truncation_and_signatures");
    for plus in [false, true] {
        let bytes = image(plus);
        for length in 0..bytes.len() {
            assert!(
                !recognized(&bytes[..length]),
                "accepted truncated length {length}"
            );
        }
        for offset in [0, 0x80, 0x98, 0x280] {
            let mut invalid = bytes.clone();
            invalid[offset] ^= 0xff;
            assert!(
                !recognized(&invalid),
                "accepted damaged signature at {offset}"
            );
        }
        let mut invalid = bytes;
        put32(&mut invalid, 0x98 + if plus { 108 } else { 92 }, 14);
        assert!(!recognized(&invalid));
    }
}

#[test]
fn header_counts_and_unbacked_or_ambiguous_rvas_fail_closed() {
    event("bounded_rva_mapping");
    let original = image(false);
    for (offset, value) in [
        (60, u32::MAX),
        (0x98 + 208, u32::MAX),
        (0x98 + 212, 71),
        (0x200, 71),
        (0x200, 73),
        (0x208, 0x2400),
        (0x20c, u32::MAX),
        (0x178 + 20, u32::MAX),
    ] {
        let mut invalid = original.clone();
        put32(&mut invalid, offset, value);
        assert!(!recognized(&invalid), "accepted invalid field at {offset}");
    }
    for count in [0, 97, u16::MAX] {
        let mut invalid = original.clone();
        put16(&mut invalid, 0x86, count);
        assert!(!recognized(&invalid));
    }
    let mut invalid = original.clone();
    put16(&mut invalid, 0x94, 4097);
    assert!(!recognized(&invalid));
    let mut ambiguous = original.clone();
    put16(&mut ambiguous, 0x86, 2);
    ambiguous.copy_within(0x178..0x178 + 40, 0x178 + 40);
    assert!(!recognized(&ambiguous));
    let mut virtual_only = original;
    put32(&mut virtual_only, 0x178 + 16, 72);
    assert!(
        !recognized(&virtual_only),
        "zero-filled virtual memory is not stored metadata"
    );
}

#[test]
fn recognition_reads_only_fixed_headers_even_with_a_huge_declared_file() {
    struct Counted {
        source: Cursor<Vec<u8>>,
        bytes: usize,
        reads: usize,
    }
    impl Read for Counted {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            let count = self.source.read(output)?;
            self.bytes += count;
            Ok(count)
        }
    }
    impl Seek for Counted {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.source.seek(position)
        }
    }
    event("read_budget");
    let mut input = Counted {
        source: Cursor::new(image(true)),
        bytes: 0,
        reads: 0,
    };
    assert!(recognize(&mut input, u64::MAX).is_some());
    assert_eq!(input.bytes, 64 + 24 + 240 + 40 + 72 + 4);
    assert_eq!(input.reads, 6);
}

#[test]
fn managed_routing_preserves_explicit_ids_and_does_not_need_discovery_processes() {
    event("selection");
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("application.dll");
    std::fs::write(&target, image(false)).unwrap();
    let executable = std::env::current_exe()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let adapters = vec![
        AdapterSpec {
            id: "native-fixture".into(),
            command_candidates: vec![executable.clone()],
            adapter_args: vec![],
            languages: vec!["binary"],
            install_hint: String::new(),
        },
        AdapterSpec {
            id: "netcoredbg".into(),
            command_candidates: vec![executable],
            adapter_args: vec![],
            languages: vec!["csharp", "fsharp"],
            install_hint: String::new(),
        },
    ];
    assert_eq!(
        select_adapter(Some(&target), None, &adapters).unwrap().id,
        "netcoredbg"
    );
    assert_eq!(
        select_adapter(Some(&target), Some("native-fixture"), &adapters)
            .unwrap()
            .id,
        "native-fixture"
    );
    assert!(select_adapter(Some(&target), Some("absent"), &adapters).is_none());
}

#[test]
fn netcoredbg_uses_its_actual_pid_and_entry_stop_contract() {
    event("wire_fields");
    let adapter = select_adapter(None, Some("netcoredbg"), &[]).unwrap();
    assert_eq!(attach_arguments(&adapter, 314), json!({"processId":314}));
    for stop in [None, Some(true), Some(false)] {
        let mut arguments: Value =
            launch_arguments(&adapter, Path::new("app.dll"), &[], Path::new("."));
        configuration::set_stop_on_entry("netcoredbg", &mut arguments, stop);
        assert_eq!(arguments["stopAtEntry"], stop.unwrap_or(true));
        assert!(arguments.get("stopOnEntry").is_none());
    }
}

#[test]
fn source_build_guard_does_not_reject_apphosts_or_change_other_backends() {
    event("build_preflight");
    let adapter = select_adapter(None, Some("netcoredbg"), &[]).unwrap();
    for name in [
        "app.cs",
        "app.fsx",
        "project.csproj",
        "project.FSPROJ",
        "app.sln",
        "app.slnx",
    ] {
        assert!(
            require_built_target(&adapter, Some(Path::new(name)))
                .unwrap_err()
                .to_string()
                .contains("DAP_BUILD_REQUIRED")
        );
    }
    for name in ["application.dll", "apphost.exe", "apphost"] {
        assert!(require_built_target(&adapter, Some(Path::new(name))).is_ok());
    }
    assert!(require_built_target(&adapter, None).is_ok());
    let other = select_adapter(None, Some("lldb-dap"), &[]).unwrap();
    assert!(require_built_target(&other, Some(Path::new("app.cs"))).is_ok());
}

#[cfg(unix)]
mod native {
    use super::*;
    use crate::debug::DebugTool;
    use crate::tools::Tool;

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::new()
            .enable_parking(false)
            .worker_threads(1)
            .blocking_threads(1, 8)
            .build()
            .unwrap()
    }

    fn call(
        tool: &DebugTool,
        runtime: &asupersync::runtime::Runtime,
        input: Value,
    ) -> Result<Value> {
        event(input["action"].as_str().unwrap_or("invalid"));
        let output = runtime.block_on(tool.execute("netcoredbg-protocol", input, None))?;
        assert!(!output.is_error);
        Ok(output.details.unwrap())
    }

    fn fixture(root: &Path) -> Option<AdapterSpec> {
        let python = select_adapter(None, Some("debugpy"), &[])
            .unwrap()
            .resolve_command();
        let Some(python) = python else {
            assert!(
                std::env::var_os("PI_DEBUG_REQUIRE_PROTOCOL").is_none(),
                "python3 is required for .NET DAP protocol tests"
            );
            event("skipped_python_missing");
            return None;
        };
        let path = root.join("adapter.py");
        std::fs::write(
            &path,
            format!(
                "from pathlib import Path\nPath('started').write_text('protocol-peer')\n{}",
                include_str!("../../test_adapter.py")
            ),
        )
        .unwrap();
        std::fs::write(root.join("Application.dll"), image(false)).unwrap();
        std::fs::write(root.join("Program.cs"), "class Program {}\n").unwrap();
        Some(AdapterSpec {
            id: "netcoredbg".into(),
            command_candidates: vec![python],
            adapter_args: vec![
                "-I".into(),
                "-u".into(),
                path.to_str().unwrap().to_owned(),
                "normal".into(),
            ],
            languages: vec!["csharp", "fsharp"],
            install_hint: "deterministic peer, not netcoredbg".into(),
        })
    }

    fn captured(tool: &DebugTool, runtime: &asupersync::runtime::Runtime) -> Vec<Value> {
        call(
            tool,
            runtime,
            json!({"action":"custom_request","command":"capture"}),
        )
        .unwrap()["result"]["requests"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn request<'a>(wire: &'a [Value], command: &str) -> &'a Value {
        wire.iter()
            .find(|entry| entry["command"] == command)
            .unwrap()
    }

    #[test]
    fn public_managed_launch_selects_backend_configures_then_controls_the_actual_session() {
        for stop in [None, Some(true), Some(false)] {
            let root = tempfile::tempdir().unwrap();
            let Some(adapter) = fixture(root.path()) else {
                return;
            };
            let tool = DebugTool::new(root.path(), None).with_adapters(vec![adapter]);
            let runtime = runtime();
            let mut input = json!({"action":"launch","program":"Application.dll", "args":["a b", "$(literal)", ""],
                "initialBreakpoints":[{"file":"Program.cs","line":10}]});
            if let Some(stop) = stop {
                input["stopOnEntry"] = json!(stop);
            }
            let started = call(&tool, &runtime, input).unwrap();
            assert_eq!(started["adapter"], "netcoredbg");
            assert_eq!(started["transport"], "stdio");
            let stack = call(&tool, &runtime, json!({"action":"stack_trace"})).unwrap();
            let frame = stack["frames"][0]["id"].as_u64().unwrap();
            assert_eq!(
                call(
                    &tool,
                    &runtime,
                    json!({"action":"evaluate","expression":"count","frameId":frame})
                )
                .unwrap()["result"],
                "42"
            );
            call(&tool, &runtime, json!({"action":"step_over"})).unwrap();
            assert!(call(&tool, &runtime, json!({"action":"scopes","frameId":frame})).is_err());
            let wire = captured(&tool, &runtime);
            let args = &request(&wire, "launch")["arguments"];
            assert_eq!(
                args["program"],
                json!(root.path().join("Application.dll").canonicalize().unwrap())
            );
            assert_eq!(args["args"], json!(["a b", "$(literal)", ""]));
            assert_eq!(args["stopAtEntry"], stop.unwrap_or(true));
            assert!(args.get("stopOnEntry").is_none());
            let position = |name: &str| {
                wire.iter()
                    .position(|entry| entry["command"] == name)
                    .unwrap()
            };
            assert!(position("launch") < position("setBreakpoints"));
            assert!(position("setBreakpoints") < position("configurationDone"));
            // Launched targets use terminate; nonterminating disconnect is
            // correctly reserved for targets attached from outside Pi.
            call(&tool, &runtime, json!({"action":"terminate"})).unwrap();
            assert_eq!(
                call(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"],
                json!([])
            );
        }
    }

    #[test]
    fn public_netcoredbg_attach_sends_process_id_and_preserves_detach_policy() {
        let root = tempfile::tempdir().unwrap();
        let Some(adapter) = fixture(root.path()) else {
            return;
        };
        let tool = DebugTool::new(root.path(), None).with_adapters(vec![adapter]);
        let runtime = runtime();
        call(
            &tool,
            &runtime,
            json!({"action":"attach","adapter":"netcoredbg","pid":314}),
        )
        .unwrap();
        let wire = captured(&tool, &runtime);
        assert_eq!(
            request(&wire, "attach")["arguments"],
            json!({"processId":314})
        );
        call(&tool, &runtime, json!({"action":"disconnect"})).unwrap();
        let detached: Value =
            serde_json::from_slice(&std::fs::read(root.path().join("disconnect.json")).unwrap())
                .unwrap();
        assert_eq!(detached["terminateDebuggee"], false);
    }

    #[test]
    fn explicit_config_preserves_native_profile_and_rejects_hidden_entry_stops() {
        let root = tempfile::tempdir().unwrap();
        let Some(adapter) = fixture(root.path()) else {
            return;
        };
        let tool = DebugTool::new(root.path(), None).with_adapters(vec![adapter]);
        let runtime = runtime();
        std::fs::write(
            root.path().join("dap.json"),
            serde_json::to_vec(&json!({"adapters":{"netcoredbg":{
                "launchDefaults":{"justMyCode":false,"stopAtEntry":true}
            }}}))
            .unwrap(),
        )
        .unwrap();
        let input = json!({"action":"launch","program":"Application.dll","adapter":"netcoredbg","adapterConfig":"dap.json","stopOnEntry":false});
        let error = call(&tool, &runtime, input.clone()).unwrap_err();
        assert!(error.to_string().contains("DAP_CONFIG"));
        assert!(!root.path().join("started").exists());
        std::fs::write(
            root.path().join("dap.json"),
            serde_json::to_vec(&json!({"adapters":{"netcoredbg":{
                "launchDefaults":{"justMyCode":false}
            }}}))
            .unwrap(),
        )
        .unwrap();
        call(&tool, &runtime, input).unwrap();
        let wire = captured(&tool, &runtime);
        assert_eq!(request(&wire, "launch")["arguments"]["justMyCode"], false);
        assert_eq!(request(&wire, "launch")["arguments"]["stopAtEntry"], false);
        call(&tool, &runtime, json!({"action":"terminate"})).unwrap();
    }

    #[test]
    fn unbuilt_projects_and_sources_fail_before_any_adapter_is_started() {
        let root = tempfile::tempdir().unwrap();
        let Some(adapter) = fixture(root.path()) else {
            return;
        };
        let tool = DebugTool::new(root.path(), None).with_adapters(vec![adapter]);
        let runtime = runtime();
        std::fs::write(
            root.path().join("dap.json"),
            r#"{"adapters":{"netcoredbg":{}}}"#,
        )
        .unwrap();
        for target in [
            "Program.cs",
            "Program.fs",
            "App.csproj",
            "App.FSPROJ",
            "App.sln",
        ] {
            std::fs::write(root.path().join(target), "unbuilt source or project").unwrap();
            for configured in [false, true] {
                let mut input = json!({"action":"launch","program":target,"adapter":"netcoredbg"});
                if configured {
                    input["adapterConfig"] = json!("dap.json");
                }
                let error = call(&tool, &runtime, input).unwrap_err();
                assert!(error.to_string().contains("DAP_BUILD_REQUIRED"), "{error}");
                assert!(!root.path().join("started").exists());
                assert_eq!(
                    call(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"],
                    json!([])
                );
            }
        }
    }
}
