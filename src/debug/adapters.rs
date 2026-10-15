//! Registered debug adapters and target-aware launch arguments (bd-cv653.1.2).
//! Built-in adapters: lldb-dap, GDB, CodeLLDB and debugpy over stdio,
//! Delve over owned TCP.

use std::path::Path;

use serde_json::Value;

/// One debug-adapter definition. Tool arguments select a registered ID, or
/// explicitly name a workspace adapterConfig plus ID; never a shell command.
/// SDK hosts can override definitions explicitly.
#[derive(Debug, Clone)]
pub struct AdapterSpec {
    pub id: String,
    pub command_candidates: Vec<String>,
    pub adapter_args: Vec<String>,
    pub languages: Vec<&'static str>,
    pub install_hint: String,
}

impl AdapterSpec {
    /// Return the exact absolute path discovered now, so changing the child's
    /// working directory cannot resolve a relative PATH entry somewhere else.
    #[must_use]
    pub fn resolve_command(&self) -> Option<String> {
        self.command_candidates.iter().find_map(|candidate| {
            let path = Path::new(candidate);
            if path.is_absolute()
                || candidate.contains('/')
                || (cfg!(windows) && candidate.contains('\\'))
            {
                return resolved_executable(path);
            }
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .find_map(|directory| resolved_executable(&directory.join(candidate)))
            })
        })
    }
}

fn resolved_executable(path: &Path) -> Option<String> {
    let path = std::fs::canonicalize(path).ok()?;
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    // Command uses the actual path, never a lossy replacement spelling.
    path.to_str().map(str::to_owned)
}

/// GDB implements DAP natively when built with Python support. Keep command
/// discovery side-effect-free; the normal initialize exchange establishes
/// protocol availability and reports a failure rather than falling back to MI.
fn gdb_adapter() -> AdapterSpec {
    let mut args = vec![
        "--quiet".to_string(),
        "--nx".to_string(),
        "--interpreter=dap".to_string(),
        "-iex".to_string(),
        "set auto-load off".to_string(),
        "-iex".to_string(),
        "set startup-with-shell off".to_string(),
    ];
    // A byte-counted DAP frame cannot tolerate CRT newline translation.
    // This requires a GDB release supporting --binary-output on Windows.
    if cfg!(windows) {
        args.push("--binary-output".to_string());
    }
    AdapterSpec {
        id: "gdb".to_string(),
        command_candidates: vec!["gdb".to_string(), "gdb.exe".to_string()],
        adapter_args: args,
        languages: vec!["rust", "c", "cpp", "binary"],
        install_hint: "install Python-enabled GDB 16+ with the dap interpreter (17+ on Windows)".to_string(),
    }
}

#[must_use]
pub fn default_adapters() -> Vec<AdapterSpec> {
    let mut lldb_candidates = vec!["lldb-dap".to_string()];
    for dir in ["/usr/lib", "/usr/local/lib"] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("llvm") {
                    let candidate = entry.path().join("bin/lldb-dap");
                    if candidate.is_file() {
                        lldb_candidates.push(candidate.display().to_string());
                    }
                }
            }
        }
    }
    lldb_candidates.push("/usr/bin/lldb-dap".to_string());
    lldb_candidates.push("/Library/Developer/CommandLineTools/usr/bin/lldb-dap".to_string());
    vec![
        AdapterSpec {
            id: "lldb-dap".to_string(),
            command_candidates: lldb_candidates,
            adapter_args: vec![],
            languages: vec!["rust", "c", "cpp", "binary"],
            install_hint: "install the LLVM toolchain (lldb-dap ships with lldb)".to_string(),
        },
        // Preserve LLDB preference for existing callers; offer a real native
        // backend when only GDB is installed. Explicit adapter="gdb" wins.
        gdb_adapter(),
        AdapterSpec {
            id: "codelldb".to_string(),
            command_candidates: vec!["codelldb".to_string(), "codelldb.exe".to_string()],
            // CodeLLDB 1.12.3 uses framed stdio when neither --port nor
            // --connect is supplied. Reuse the existing owned transport.
            adapter_args: vec![],
            languages: vec!["rust", "c", "cpp", "binary"],
            install_hint: "install CodeLLDB 1.12.3 with native stdio support; put its adapter on PATH or select its command with adapterConfig".to_string(),
        },
        AdapterSpec {
            id: "debugpy".to_string(),
            command_candidates: vec!["python3".to_string()],
            adapter_args: vec!["-m".to_string(), "debugpy.adapter".to_string()],
            languages: vec!["python"],
            install_hint: "install with: pip install debugpy".to_string(),
        },
        AdapterSpec {
            id: "dlv".to_string(),
            command_candidates: vec!["dlv".to_string(), "dlv.exe".to_string()],
            adapter_args: vec!["dap".to_string()],
            languages: vec!["go"],
            install_hint: "install with: go install github.com/go-delve/delve/cmd/dlv@latest"
                .to_string(),
        },
        AdapterSpec {
            id: "netcoredbg".to_string(),
            command_candidates: vec!["netcoredbg".to_string(), "netcoredbg.exe".to_string()],
            adapter_args: vec!["--interpreter=vscode".to_string()],
            languages: vec!["csharp", "fsharp"],
            install_hint: "install netcoredbg and place it on PATH".to_string(),
        },
        AdapterSpec {
            id: "kotlin-debug-adapter".to_string(),
            command_candidates: vec!["kotlin-debug-adapter".to_string(), "kotlin-debug-adapter.exe".to_string()],
            adapter_args: vec![],
            languages: vec!["kotlin"],
            install_hint: "install kotlin-debug-adapter and place it on PATH".to_string(),
        },
        AdapterSpec {
            id: "rdbg".to_string(),
            command_candidates: vec!["rdbg".to_string(), "rdbg.exe".to_string()],
            adapter_args: vec!["--open".to_string(), "--command".to_string(), "--".to_string()],
            languages: vec!["ruby"],
            install_hint: "install the Ruby debug gem (rdbg) and place it on PATH".to_string(),
        },
        AdapterSpec {
            id: "php-debug-adapter".to_string(),
            command_candidates: vec!["php-debug-adapter".to_string(), "php-debug-adapter.exe".to_string()],
            adapter_args: vec![],
            languages: vec!["php"],
            install_hint: "install php-debug-adapter and place it on PATH".to_string(),
        },
        AdapterSpec {
            id: "bash-debug-adapter".to_string(),
            command_candidates: vec!["bash-debug-adapter".to_string(), "bash-debug-adapter.exe".to_string()],
            adapter_args: vec![],
            languages: vec!["bash", "shell"],
            install_hint: "install bash-debug-adapter and bashdb, then place them on PATH".to_string(),
        },
        AdapterSpec {
            id: "dart-debug-adapter".to_string(),
            command_candidates: vec!["dart".to_string(), "dart.exe".to_string()],
            adapter_args: vec!["debug_adapter".to_string()],
            languages: vec!["dart"],
            install_hint: "install the Dart SDK and place dart on PATH".to_string(),
        },
        AdapterSpec {
            id: "flutter-debug-adapter".to_string(),
            command_candidates: vec!["dart".to_string(), "dart.exe".to_string()],
            adapter_args: vec!["debug_adapter".to_string(), "--flutter-sdk-path".to_string(), "".to_string()],
            languages: vec!["flutter"],
            install_hint: "install the Dart and Flutter SDKs; configure the Flutter SDK path when required".to_string(),
        },
        AdapterSpec {
            id: "elixir-ls-debugger".to_string(),
            command_candidates: vec!["elixir-ls-debugger".to_string(), "elixir-ls-debugger.bat".to_string()],
            adapter_args: vec![],
            languages: vec!["elixir"],
            install_hint: "install ElixirLS and place elixir-ls-debugger on PATH".to_string(),
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    NativeBinary,
    Python,
    Go,
    DotNet,
    Kotlin,
    Ruby,
    Php,
    Shell,
    Dart,
    Elixir,
}

/// Local package directories are meaningful Go targets, not native binaries.
/// A compiled Go binary still needs adapter="dlv" because its language cannot
/// be inferred reliably from its filename.
#[must_use]
pub fn classify_target(target: &Path) -> TargetKind {
    if target.is_dir()
        && (target.join("go.mod").is_file()
            || std::fs::read_dir(target).is_ok_and(|entries| {
                entries
                    .take(4096)
                    .filter_map(std::result::Result::ok)
                    .any(|entry| {
                        entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "go")
                            && entry.path().is_file()
                    })
            }))
    {
        return TargetKind::Go;
    }
    match target.extension().and_then(|extension| extension.to_str()) {
        Some("py") => TargetKind::Python,
        Some("go") => TargetKind::Go,
        Some("cs" | "csx" | "fs" | "fsx") => TargetKind::DotNet,
        Some("kt" | "kts") => TargetKind::Kotlin,
        Some("rb" | "rake" | "gemspec") => TargetKind::Ruby,
        Some("php" | "phtml") => TargetKind::Php,
        Some("sh" | "bash") => TargetKind::Shell,
        Some("dart") => TargetKind::Dart,
        Some("ex" | "exs" | "heex" | "eex") => TargetKind::Elixir,
        _ => TargetKind::NativeBinary,
    }
}

#[must_use]
pub fn select_adapter(
    target: Option<&Path>,
    requested: Option<&str>,
    overrides: &[AdapterSpec],
) -> Option<AdapterSpec> {
    let available = if overrides.is_empty() {
        default_adapters()
    } else {
        let mut merged = overrides.to_vec();
        let overridden: Vec<_> = overrides
            .iter()
            .map(|adapter| adapter.id.as_str())
            .collect();
        merged.extend(
            default_adapters()
                .into_iter()
                .filter(|adapter| !overridden.contains(&adapter.id.as_str())),
        );
        merged
    };
    if let Some(id) = requested {
        return available.into_iter().find(|adapter| adapter.id == id);
    }
    let language = match target.map_or(TargetKind::NativeBinary, classify_target) {
        TargetKind::Python => "python",
        TargetKind::Go => "go",
        TargetKind::DotNet => "csharp",
        TargetKind::Kotlin => "kotlin",
        TargetKind::Ruby => "ruby",
        TargetKind::Php => "php",
        TargetKind::Shell => "shell",
        TargetKind::Dart => "dart",
        TargetKind::Elixir => "elixir",
        TargetKind::NativeBinary => "binary",
    };
    available
        .into_iter()
        .filter(|adapter| adapter.languages.contains(&language))
        .find(|adapter| adapter.resolve_command().is_some())
}

#[must_use]
pub fn go_launch_mode(program: &Path) -> &'static str {
    if program
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with("_test.go"))
    {
        "test"
    } else if program.is_dir()
        || program
            .extension()
            .is_some_and(|extension| extension == "go")
    {
        "debug"
    } else {
        "exec"
    }
}

#[must_use]
pub fn launch_arguments(
    adapter: &AdapterSpec,
    program: &Path,
    args: &[String],
    cwd: &Path,
) -> Value {
    match adapter.id.as_str() {
        "dlv" => serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
            "cwd": cwd.display().to_string(),
            "mode": go_launch_mode(program),
            "stopOnEntry": true,
        }),
        "gdb" => serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
            "cwd": cwd.display().to_string(),
            // GDB's stopOnEntry means the dynamic loader's first instruction;
            // the main-subprogram stop is useful for source-level agent work.
            "stopOnEntry": false,
            "stopAtBeginningOfMainSubprogram": true,
        }),
        "netcoredbg" => serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
            "cwd": cwd.display().to_string(),
            "stopAtEntry": true,
        }),
        "rdbg" => serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
            "cwd": cwd.display().to_string(),
            "type": "rdbg",
        }),
        "bash-debug-adapter" => serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
            "cwd": cwd.display().to_string(),
            "type": "bashdb",
            "pathBashdb": "bashdb",
            "pathBash": "bash",
        }),
        "elixir-ls-debugger" => serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
            "cwd": cwd.display().to_string(),
            "type": "mix_task",
            "task": "run",
            "stopOnEntry": true,
        }),
        "codelldb" => serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
            "cwd": cwd.display().to_string(),
            // Native CodeLLDB defaults to a terminal. This client has not
            // granted its runInTerminal path, so keep output on DAP events.
            "terminal": "console",
            "stopOnEntry": true,
        }),
        _ => serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
            "cwd": cwd.display().to_string(),
            "console": "internalConsole",
            "stopOnEntry": true,
        }),
    }
}

#[must_use]
pub fn attach_arguments(adapter: &AdapterSpec, pid: u32) -> Value {
    match adapter.id.as_str() {
        "debugpy" => serde_json::json!({ "processId": pid }),
        "dlv" => serde_json::json!({ "processId": pid, "mode": "local" }),
        _ => serde_json::json!({ "pid": pid }),
    }
}

#[cfg(test)]
#[path = "adapter_configuration_tests.rs"]
mod configuration_tests;
#[cfg(test)]
#[path = "codelldb_tests.rs"]
mod codelldb_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_by_extension() {
        assert_eq!(classify_target(Path::new("app.py")), TargetKind::Python);
        assert_eq!(classify_target(Path::new("main.go")), TargetKind::Go);
        assert_eq!(classify_target(Path::new("app.cs")), TargetKind::DotNet);
        assert_eq!(classify_target(Path::new("build.kts")), TargetKind::Kotlin);
        assert_eq!(classify_target(Path::new("task.rake")), TargetKind::Ruby);
        assert_eq!(classify_target(Path::new("index.php")), TargetKind::Php);
        assert_eq!(classify_target(Path::new("setup.sh")), TargetKind::Shell);
        assert_eq!(classify_target(Path::new("main.dart")), TargetKind::Dart);
        assert_eq!(classify_target(Path::new("app.exs")), TargetKind::Elixir);
        assert_eq!(
            classify_target(Path::new("/bin/true")),
            TargetKind::NativeBinary
        );
    }

    #[test]
    fn omp_stdio_catalog_profiles_are_registered_with_expected_argv() {
        let adapters = default_adapters();
        let by_id = |id: &str| adapters.iter().find(|adapter| adapter.id == id).unwrap();
        assert_eq!(by_id("netcoredbg").adapter_args, ["--interpreter=vscode"]);
        assert_eq!(by_id("rdbg").adapter_args, ["--open", "--command", "--"]);
        assert_eq!(by_id("dart-debug-adapter").adapter_args, ["debug_adapter"]);
        assert_eq!(
            by_id("flutter-debug-adapter").adapter_args,
            ["debug_adapter", "--flutter-sdk-path", ""]
        );
        for id in [
            "kotlin-debug-adapter",
            "php-debug-adapter",
            "bash-debug-adapter",
            "elixir-ls-debugger",
        ] {
            assert!(by_id(id).adapter_args.is_empty(), "{id}");
        }
    }

    #[test]
    fn new_language_targets_select_matching_available_profile() {
        let executable = std::env::current_exe().unwrap().display().to_string();
        let cases = [
            ("app.cs", "netcoredbg", "csharp"),
            ("build.kts", "kotlin-debug-adapter", "kotlin"),
            ("app.rb", "rdbg", "ruby"),
            ("index.php", "php-debug-adapter", "php"),
            ("setup.sh", "bash-debug-adapter", "shell"),
            ("main.dart", "dart-debug-adapter", "dart"),
            ("app.exs", "elixir-ls-debugger", "elixir"),
        ];
        for (target, id, language) in cases {
            let override_adapter = AdapterSpec {
                id: id.to_string(),
                command_candidates: vec![executable.clone()],
                adapter_args: vec![],
                languages: vec![language],
                install_hint: "test".to_string(),
            };
            assert_eq!(
                select_adapter(Some(Path::new(target)), None, &[override_adapter])
                    .unwrap()
                    .id,
                id
            );
        }
    }

    #[test]
    fn requested_id_wins_over_auto() {
        let picked = select_adapter(Some(Path::new("app.py")), Some("lldb-dap"), &[]);
        assert_eq!(picked.expect("found").id, "lldb-dap");
    }

    #[test]
    fn auto_select_needs_resolvable_command() {
        if let Some(picked) = select_adapter(Some(Path::new("x.py")), None, &[]) {
            assert_eq!(picked.id, "debugpy");
        }
    }

    #[test]
    fn launch_args_shape_per_adapter() {
        let adapters = default_adapters();
        let lldb = adapters
            .iter()
            .find(|adapter| adapter.id == "lldb-dap")
            .unwrap();
        let args = launch_arguments(
            lldb,
            Path::new("/tmp/app"),
            &["--flag".to_string()],
            Path::new("/tmp"),
        );
        assert_eq!(args["program"], "/tmp/app");
        assert_eq!(args["args"][0], "--flag");
        let dlv = adapters.iter().find(|adapter| adapter.id == "dlv").unwrap();
        assert_eq!(
            launch_arguments(dlv, Path::new("/tmp/app"), &[], Path::new("/tmp"))["mode"],
            "exec"
        );
        assert_eq!(attach_arguments(lldb, 4242)["pid"], 4242);
        assert_eq!(
            attach_arguments(dlv, 4242),
            serde_json::json!({"processId":4242,"mode":"local"})
        );
    }

    #[test]
    fn gdb_profile_is_native_dap_and_suppresses_implicit_startup_code() {
        let adapter = select_adapter(None, Some("gdb"), &[]).unwrap();
        assert!(adapter.adapter_args.iter().any(|arg| arg == "--interpreter=dap"));
        assert!(adapter.adapter_args.iter().any(|arg| arg == "--nx"));
        for command in ["set auto-load off", "set startup-with-shell off"] {
            assert!(adapter.adapter_args.windows(2).any(|args| args[0] == "-iex" && args[1] == command));
        }
        assert_eq!(adapter.adapter_args.iter().any(|arg| arg == "--binary-output"), cfg!(windows));
        let launch = launch_arguments(&adapter, Path::new("app"), &["a b".into(), "$(literal)".into()], Path::new("."));
        assert_eq!(launch["args"], serde_json::json!(["a b", "$(literal)"]));
        assert_eq!(launch["stopAtBeginningOfMainSubprogram"], true);
        assert_eq!(launch["stopOnEntry"], false);
        assert!(launch.get("console").is_none());
        assert_eq!(attach_arguments(&adapter, 42), serde_json::json!({"pid":42}));
    }

    #[test]
    fn go_sources_tests_packages_and_binaries_have_distinct_modes() {
        assert_eq!(go_launch_mode(Path::new("main.go")), "debug");
        assert_eq!(go_launch_mode(Path::new("main_test.go")), "test");
        assert_eq!(go_launch_mode(Path::new("server")), "exec");
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(go_launch_mode(directory.path()), "debug");
        assert_eq!(classify_target(directory.path()), TargetKind::NativeBinary);
        std::fs::write(directory.path().join("main.go"), "package main\n").unwrap();
        assert_eq!(classify_target(directory.path()), TargetKind::Go);
    }

    #[cfg(unix)]
    #[test]
    fn executable_resolution_preserves_identity_and_rejects_nonexecutables() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("adapter");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolved_executable(&program).is_none());
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = directory.path().join("link");
        symlink(&program, &link).unwrap();
        assert_eq!(
            resolved_executable(&link).unwrap(),
            std::fs::canonicalize(&program).unwrap().to_str().unwrap()
        );
        assert!(resolved_executable(directory.path()).is_none());
    }
}
