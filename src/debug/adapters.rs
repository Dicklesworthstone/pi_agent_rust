//! Debug-adapter registry: configured adapters plus built-in defaults with
//! auto-selection by target type (bd-cv653.1.2).
//!
//! Defaults ship for lldb-dap (native binaries), debugpy (Python), and dlv
//! (Go). Settings can override/extend via `debug.adapters.<id>` with
//! command/args and launch/attach argument templates.

use std::path::Path;

use serde_json::Value;

/// One debug-adapter definition.
#[derive(Debug, Clone)]
pub struct AdapterSpec {
    /// Registry id (`lldb-dap`, `debugpy`, `dlv`).
    pub id: String,
    /// Command candidates probed in order (first existing wins).
    pub command_candidates: Vec<String>,
    /// Extra argv for the adapter process itself.
    pub adapter_args: Vec<String>,
    /// Languages this adapter handles (`rust`, `c`, `python`, `go`, ...).
    pub languages: Vec<&'static str>,
    /// Install hint for the missing-binary error.
    pub install_hint: String,
}

impl AdapterSpec {
    /// The first candidate that resolves (PATH or absolute path), else None.
    #[must_use]
    pub fn resolve_command(&self) -> Option<String> {
        self.command_candidates.iter().find_map(|candidate| {
            if candidate.contains('/') {
                return std::path::Path::new(candidate)
                    .exists()
                    .then(|| candidate.clone());
            }
            // PATH probe via a cheap --version spawn is too slow per call;
            // use a filesystem walk of PATH entries.
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths).find_map(|dir| {
                    let full = dir.join(candidate);
                    full.exists().then(|| candidate.clone())
                })
            })
        })
    }
}

/// Built-in adapter defaults.
#[must_use]
pub fn default_adapters() -> Vec<AdapterSpec> {
    let mut lldb_candidates = vec!["lldb-dap".to_string()];
    // LLVM toolchains often install off-PATH.
    for dir in ["/usr/lib", "/usr/local/lib"] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("llvm") {
                    let candidate = entry.path().join("bin/lldb-dap");
                    if candidate.exists() {
                        lldb_candidates.push(candidate.display().to_string());
                    }
                }
            }
        }
    }
    // macOS Xcode command-line tools.
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
        AdapterSpec {
            id: "debugpy".to_string(),
            command_candidates: vec!["python3".to_string()],
            adapter_args: vec!["-m".to_string(), "debugpy.adapter".to_string()],
            languages: vec!["python"],
            install_hint: "install with: pip install debugpy".to_string(),
        },
        AdapterSpec {
            id: "dlv".to_string(),
            command_candidates: vec!["dlv".to_string()],
            adapter_args: vec!["dap".to_string()],
            languages: vec!["go"],
            install_hint: "install with: go install github.com/go-delve/delve/cmd/dlv@latest"
                .to_string(),
        },
    ]
}

/// How the adapter is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    /// A native executable (lldb-dap).
    NativeBinary,
    /// A Python program (debugpy).
    Python,
    /// A Go program (dlv).
    Go,
}

/// Classify a launch target by extension/file shape.
#[must_use]
pub fn classify_target(target: &Path) -> TargetKind {
    match target.extension().and_then(|e| e.to_str()) {
        Some("py") => TargetKind::Python,
        Some("go") => TargetKind::Go,
        _ => TargetKind::NativeBinary,
    }
}

/// Pick the adapter for a target: explicit `adapter` id wins; otherwise the
/// first default whose languages cover the target kind and whose command
/// resolves.
#[must_use]
pub fn select_adapter(
    target: Option<&Path>,
    requested: Option<&str>,
    overrides: &[AdapterSpec],
) -> Option<AdapterSpec> {
    let available: Vec<AdapterSpec> = if overrides.is_empty() {
        default_adapters()
    } else {
        let mut merged = overrides.to_vec();
        let overridden: Vec<String> = overrides.iter().map(|a| a.id.clone()).collect();
        merged.extend(
            default_adapters()
                .into_iter()
                .filter(|a| !overridden.contains(&a.id)),
        );
        merged
    };
    if let Some(id) = requested {
        return available.into_iter().find(|a| a.id == id);
    }
    let kind = target.map_or(TargetKind::NativeBinary, classify_target);
    let language = match kind {
        TargetKind::Python => "python",
        TargetKind::Go => "go",
        TargetKind::NativeBinary => "binary",
    };
    available
        .into_iter()
        .filter(|adapter| adapter.languages.contains(&language))
        .find(|adapter| adapter.resolve_command().is_some())
}

/// Build the DAP `launch` arguments for an adapter + program.
///
/// `stopOnEntry: true` is deliberate: agent-driven debugging needs the
/// launch → set-breakpoints → continue flow to be deterministic — the
/// entry stop guarantees breakpoints land before the program runs.
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
            "mode": "exec",
            "stopOnEntry": true,
        }),
        // debugpy and lldb-dap share the same argument shape; internalConsole
        // avoids the runInTerminal reverse-request negotiation.
        _ => serde_json::json!({
            "program": program.display().to_string(),
            "args": args,
            "cwd": cwd.display().to_string(),
            "console": "internalConsole",
            "stopOnEntry": true,
        }),
    }
}

/// Build the DAP `attach` arguments.
#[must_use]
pub fn attach_arguments(adapter: &AdapterSpec, pid: u32) -> Value {
    match adapter.id.as_str() {
        "debugpy" => serde_json::json!({ "processId": pid }),
        "dlv" => serde_json::json!({ "processId": pid, "mode": "local" }),
        _ => serde_json::json!({ "pid": pid }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_by_extension() {
        assert_eq!(classify_target(Path::new("app.py")), TargetKind::Python);
        assert_eq!(classify_target(Path::new("main.go")), TargetKind::Go);
        assert_eq!(
            classify_target(Path::new("/bin/true")),
            TargetKind::NativeBinary
        );
    }

    #[test]
    fn requested_id_wins_over_auto() {
        let adapters = default_adapters();
        let picked = select_adapter(Some(Path::new("app.py")), Some("lldb-dap"), &[]);
        assert_eq!(picked.expect("found").id, "lldb-dap");
        let _ = adapters;
    }

    #[test]
    fn auto_select_needs_resolvable_command() {
        // A python target picks debugpy when python3 exists (it does on any
        // dev machine); the point is the selection is resolution-aware.
        let picked = select_adapter(Some(Path::new("x.py")), None, &[]);
        if let Some(picked) = picked {
            assert_eq!(picked.id, "debugpy");
        }
    }

    #[test]
    fn launch_args_shape_per_adapter() {
        let lldb = default_adapters()
            .into_iter()
            .find(|a| a.id == "lldb-dap")
            .expect("lldb");
        let args = launch_arguments(
            &lldb,
            Path::new("/tmp/app"),
            &["--flag".to_string()],
            Path::new("/tmp"),
        );
        assert_eq!(args["program"], "/tmp/app");
        assert_eq!(args["args"][0], "--flag");

        let dlv = AdapterSpec {
            id: "dlv".to_string(),
            command_candidates: vec![],
            adapter_args: vec![],
            languages: vec!["go"],
            install_hint: String::new(),
        };
        let args = launch_arguments(&dlv, Path::new("/tmp/app"), &[], Path::new("/tmp"));
        assert_eq!(args["mode"], "exec");

        let args = attach_arguments(&lldb, 4242);
        assert_eq!(args["pid"], 4242);
    }
}
