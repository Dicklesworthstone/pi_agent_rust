//! Language-server registry: per-workspace singletons with lazy spawn and
//! idle shutdown.
//!
//! Servers are configured from a built-in defaults table (rust-analyzer,
//! typescript-language-server, pyright, gopls, clangd) merged with the
//! `lsp.servers` settings map (per-field overrides; new names add servers).
//! Workspace roots are detected by walking up from the target file for the
//! server's root markers (`Cargo.toml`, `package.json`, ...), falling back
//! to the tool working directory. Idle shutdown is a deterministic lazy
//! sweep: every registry access first retires entries idle longer than the
//! configured TTL (bd-cv653.1.1).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::client::{DEFAULT_REQUEST_TIMEOUT, LspClient};
use crate::config::Config;
use crate::error::{Error, Result};

/// Default idle-shutdown TTL (5 minutes).
const DEFAULT_IDLE_SHUTDOWN: Duration = Duration::from_secs(300);

/// One spawnable language-server definition.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    /// Registry key and display name (e.g. `rust-analyzer`).
    pub name: String,
    /// Executable resolved via `PATH` at spawn time.
    pub command: String,
    /// argv after the executable.
    pub args: Vec<String>,
    /// Extra environment (ambient environment is inherited, then overridden).
    pub env: Vec<(String, String)>,
    /// LSP language ids this server handles (e.g. `rust`).
    pub languages: Vec<String>,
    /// File extensions (with leading dot) routed to this server.
    pub extensions: Vec<String>,
    /// Root marker filenames walked up from the target file.
    pub root_markers: Vec<String>,
    /// `initializationOptions` payload for the handshake.
    pub initialization_options: Option<Value>,
    /// Install hint shown when the command is missing.
    pub install_hint: String,
}

/// Built-in server defaults.
#[must_use]
pub fn default_servers() -> Vec<ServerSpec> {
    vec![
        ServerSpec {
            name: "rust-analyzer".to_string(),
            command: "rust-analyzer".to_string(),
            args: vec![],
            env: vec![],
            languages: vec!["rust".to_string()],
            extensions: vec![".rs".to_string()],
            root_markers: vec!["Cargo.toml".to_string(), ".git".to_string()],
            initialization_options: None,
            install_hint: "install with: rustup component add rust-analyzer".to_string(),
        },
        ServerSpec {
            name: "typescript-language-server".to_string(),
            command: "typescript-language-server".to_string(),
            args: vec!["--stdio".to_string()],
            env: vec![],
            languages: vec![
                "javascript".to_string(),
                "javascriptreact".to_string(),
                "typescript".to_string(),
                "typescriptreact".to_string(),
            ],
            extensions: vec![
                ".js".to_string(),
                ".jsx".to_string(),
                ".ts".to_string(),
                ".tsx".to_string(),
                ".mjs".to_string(),
                ".cjs".to_string(),
            ],
            root_markers: vec!["package.json".to_string(), "tsconfig.json".to_string()],
            initialization_options: None,
            install_hint: "install with: npm install -g typescript-language-server typescript"
                .to_string(),
        },
        ServerSpec {
            name: "pyright".to_string(),
            command: "pyright-langserver".to_string(),
            args: vec!["--stdio".to_string()],
            env: vec![],
            languages: vec!["python".to_string()],
            extensions: vec![".py".to_string(), ".pyi".to_string()],
            root_markers: vec![
                "pyproject.toml".to_string(),
                "setup.py".to_string(),
                "requirements.txt".to_string(),
            ],
            initialization_options: None,
            install_hint: "install with: npm install -g pyright".to_string(),
        },
        ServerSpec {
            name: "gopls".to_string(),
            command: "gopls".to_string(),
            args: vec![],
            env: vec![],
            languages: vec!["go".to_string()],
            extensions: vec![".go".to_string()],
            root_markers: vec!["go.mod".to_string(), "go.work".to_string()],
            initialization_options: None,
            install_hint: "install with: go install golang.org/x/tools/gopls@latest".to_string(),
        },
        ServerSpec {
            name: "clangd".to_string(),
            command: "clangd".to_string(),
            args: vec![],
            env: vec![],
            languages: vec!["c".to_string(), "cpp".to_string()],
            extensions: vec![
                ".c".to_string(),
                ".h".to_string(),
                ".cc".to_string(),
                ".cpp".to_string(),
                ".cxx".to_string(),
                ".hpp".to_string(),
            ],
            root_markers: vec![
                "compile_commands.json".to_string(),
                "compile_flags.txt".to_string(),
                ".git".to_string(),
            ],
            initialization_options: None,
            install_hint: "install clangd via your system package manager".to_string(),
        },
    ]
}

/// Extension → LSP language id for `didOpen` (superset of the defaults
/// table, so user-added servers still get sane language ids).
#[must_use]
pub fn language_id_for_extension(extension: &str) -> Option<&'static str> {
    Some(match extension {
        ".rs" => "rust",
        ".py" | ".pyi" => "python",
        ".js" | ".mjs" | ".cjs" => "javascript",
        ".jsx" => "javascriptreact",
        ".ts" => "typescript",
        ".tsx" => "typescriptreact",
        ".go" => "go",
        ".c" | ".h" => "c",
        ".cc" | ".cpp" | ".cxx" | ".hpp" | ".hh" => "cpp",
        ".rb" => "ruby",
        ".java" => "java",
        ".sh" | ".bash" => "shellscript",
        ".json" => "json",
        ".md" => "markdown",
        ".toml" => "toml",
        ".yaml" | ".yml" => "yaml",
        _ => return None,
    })
}

/// Merge settings-layer server entries over the defaults table.
///
/// Same-name entries override per-field (only set fields win); unknown names
/// append new servers and require `command`.
fn merge_servers(config: Option<&Config>) -> Vec<ServerSpec> {
    let mut servers = default_servers();
    let Some(overrides) = config
        .and_then(|c| c.lsp.as_ref())
        .and_then(|l| l.servers.as_ref())
    else {
        return servers;
    };
    for (name, entry) in overrides {
        if let Some(existing) = servers.iter_mut().find(|s| &s.name == name) {
            if let Some(command) = &entry.command {
                existing.command = command.clone();
            }
            if let Some(args) = &entry.args {
                existing.args = args.clone();
            }
            if let Some(env) = &entry.env {
                existing.env = env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
            }
            if let Some(languages) = &entry.languages {
                existing.languages = languages.clone();
            }
            if let Some(extensions) = &entry.extensions {
                existing.extensions = extensions.clone();
            }
            if let Some(markers) = &entry.root_markers {
                existing.root_markers = markers.clone();
            }
            if entry.initialization_options.is_some() {
                existing.initialization_options = entry.initialization_options.clone();
            }
        } else if let Some(command) = &entry.command {
            servers.push(ServerSpec {
                name: name.clone(),
                command: command.clone(),
                args: entry.args.clone().unwrap_or_default(),
                env: entry
                    .env
                    .as_ref()
                    .map(|env| env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default(),
                languages: entry.languages.clone().unwrap_or_default(),
                extensions: entry.extensions.clone().unwrap_or_default(),
                root_markers: entry.root_markers.clone().unwrap_or_default(),
                initialization_options: entry.initialization_options.clone(),
                install_hint: format!("ensure {command:?} is on PATH"),
            });
        }
        // New-name entries without a command are skipped; a file routed to
        // them fails closed with LSP_NO_SERVER.
    }
    servers
}

/// One live server entry.
pub struct ServerEntry {
    /// The connected client.
    pub client: LspClient,
    /// Spec name that produced this server.
    pub spec_name: String,
    /// Workspace root.
    pub root: PathBuf,
    /// Whether the workspace/applyEdit server-request hook was installed.
    pub handler_installed: std::sync::atomic::AtomicBool,
    last_used: Mutex<Instant>,
}

impl ServerEntry {
    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
    }
}

/// Status snapshot for one server (tool `status` action).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub name: String,
    pub root: PathBuf,
    pub alive: bool,
    pub idle_secs: u64,
    pub open_documents: usize,
    pub dropped_notifications: u64,
    pub server_name: Option<String>,
}

/// The per-tool-instance registry.
pub struct LspRegistry {
    cwd: PathBuf,
    servers: Vec<ServerSpec>,
    request_timeout: Duration,
    idle_shutdown: Duration,
    entries: Mutex<HashMap<String, Arc<ServerEntry>>>,
    spawn_lane: asupersync::sync::Mutex<()>,
}

impl LspRegistry {
    /// Build a registry for `cwd` with settings merged from `config`.
    #[must_use]
    pub fn new(cwd: &Path, config: Option<&Config>) -> Self {
        let lsp = config.and_then(|c| c.lsp.as_ref());
        Self {
            cwd: cwd.to_path_buf(),
            servers: merge_servers(config),
            request_timeout: lsp
                .and_then(|l| l.request_timeout_secs)
                .map_or(DEFAULT_REQUEST_TIMEOUT, Duration::from_secs),
            idle_shutdown: lsp
                .and_then(|l| l.idle_shutdown_secs)
                .map_or(DEFAULT_IDLE_SHUTDOWN, Duration::from_secs),
            entries: Mutex::new(HashMap::new()),
            spawn_lane: asupersync::sync::Mutex::new(()),
        }
    }

    /// Configured request timeout.
    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<ServerEntry>>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The server spec responsible for `path`, by extension.
    #[must_use]
    pub fn spec_for_file(&self, path: &Path) -> Option<&ServerSpec> {
        let extension = path.extension()?.to_str()?;
        let dotted = format!(".{}", extension.to_ascii_lowercase());
        self.servers
            .iter()
            .find(|spec| spec.extensions.iter().any(|ext| ext == &dotted))
    }

    /// Detect the workspace root for `file`: nearest ancestor containing a
    /// root marker; falls back to the tool working directory.
    #[must_use]
    pub fn workspace_root(&self, file: &Path, spec: &ServerSpec) -> PathBuf {
        spec.workspace_root_for(&self.cwd, file)
    }

    /// Lazy idle sweep: retire entries idle past the TTL. Runs on every
    /// registry access so shutdown is deterministic without a background
    /// task.
    fn sweep_idle(&self) {
        let stale: Vec<(String, Arc<ServerEntry>)> = {
            let mut entries = self.lock_entries();
            let stale_keys: Vec<String> = entries
                .iter()
                .filter(|(_, entry)| entry.idle_for() > self.idle_shutdown || !entry.client.is_alive())
                .map(|(key, _)| key.clone())
                .collect();
            stale_keys
                .into_iter()
                .filter_map(|key| entries.remove(&key).map(|entry| (key, entry)))
                .collect()
        };
        for (_, entry) in stale {
            entry.client.kill();
        }
    }

    /// Resolve (spawn if needed) the server for `path`.
    ///
    /// # Errors
    ///
    /// - `[LSP_NO_SERVER]` when no configured server claims the extension.
    /// - `[LSP_SERVER_MISSING]` when the command cannot be spawned (includes
    ///   the spec's install hint).
    /// - Transport/handshake errors from [`LspClient::connect`].
    pub async fn client_for(&self, path: &Path) -> Result<Arc<ServerEntry>> {
        self.sweep_idle();
        let spec = self.spec_for_file(path).ok_or_else(|| {
            let extension = path
                .extension()
                .and_then(|e| e.to_str())
                .map_or_else(|| "<none>".to_string(), |e| format!(".{e}"));
            Error::tool(
                "lsp",
                format!(
                    "[LSP_NO_SERVER] no language server configured for extension {extension:?}; \
                     add an entry under lsp.servers in settings.json"
                ),
            )
        })?;
        let root = spec.workspace_root_for(&self.cwd, path);
        let key = format!("{}\n{}", spec.name, root.display());
        if let Some(entry) = self.lock_entries().get(&key) {
            if entry.client.is_alive() {
                entry.touch();
                return Ok(Arc::clone(entry));
            }
        }
        // Serialize spawns so concurrent first-use cannot double-spawn.
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let _spawn_guard = self
            .spawn_lane
            .lock(cx.cx())
            .await
            .map_err(|_| Error::tool("lsp", "[LSP_CANCELLED] cancelled by ambient context"))?;
        // Re-check after acquiring the lane.
        if let Some(entry) = self.lock_entries().get(&key) {
            if entry.client.is_alive() {
                entry.touch();
                return Ok(Arc::clone(entry));
            }
        }
        let client = LspClient::connect(
            &spec.command,
            &spec.args,
            &spec.env,
            &root,
            spec.initialization_options.as_ref(),
            self.request_timeout,
        )
        .await
        .map_err(|err| {
            let message = err.to_string();
            if message.contains("[LSP_SERVER_MISSING]") {
                Error::tool(
                    "lsp",
                    format!("{message}\nhint: {}", spec.install_hint),
                )
            } else {
                err
            }
        })?;
        let entry = Arc::new(ServerEntry {
            client,
            spec_name: spec.name.clone(),
            root,
            handler_installed: std::sync::atomic::AtomicBool::new(false),
            last_used: Mutex::new(Instant::now()),
        });
        self.lock_entries()
            .insert(key, Arc::clone(&entry));
        Ok(entry)
    }

    /// Status of every live entry.
    #[must_use]
    pub fn status(&self) -> Vec<ServerStatus> {
        self.lock_entries()
            .values()
            .map(|entry| ServerStatus {
                name: entry.spec_name.clone(),
                root: entry.root.clone(),
                alive: entry.client.is_alive(),
                idle_secs: entry.idle_for().as_secs(),
                open_documents: entry.client.open_document_count(),
                dropped_notifications: entry.client.dropped_notifications(),
                server_name: entry.client.capabilities().server_name,
            })
            .collect()
    }

    /// Look up a live entry by spec name + root (diagnostics glob surface).
    #[must_use]
    pub fn entry_for_root(&self, name: &str, root: &Path) -> Option<Arc<ServerEntry>> {
        let key = format!("{name}\n{}", root.display());
        self.lock_entries().get(&key).cloned()
    }

    /// All configured server specs (defaults merged with settings).
    #[must_use]
    pub fn configured_servers(&self) -> &[ServerSpec] {
        &self.servers
    }

    /// Kill servers. With `path`, only the server that would serve it;
    /// otherwise every entry.
    pub async fn kill_matching(&self, path: Option<&Path>) -> usize {
        let doomed: Vec<Arc<ServerEntry>> = {
            let mut entries = self.lock_entries();
            match path {
                Some(path) => {
                    let keys: Vec<String> = entries
                        .keys()
                        .filter(|key| {
                            self.spec_for_file(path)
                                .is_some_and(|spec| key.starts_with(&spec.name))
                        })
                        .cloned()
                        .collect();
                    keys.into_iter()
                        .filter_map(|key| entries.remove(&key))
                        .collect()
                }
                None => entries.drain().map(|(_, entry)| entry).collect(),
            }
        };
        let count = doomed.len();
        for entry in doomed {
            entry.client.stop().await;
        }
        count
    }
}

impl ServerSpec {
    fn workspace_root_for(&self, cwd: &Path, file: &Path) -> PathBuf {
        let start = if file.is_dir() {
            file
        } else {
            file.parent().unwrap_or(cwd)
        };
        for ancestor in start.ancestors() {
            for marker in &self.root_markers {
                if ancestor.join(marker).exists() {
                    return ancestor.to_path_buf();
                }
            }
        }
        cwd.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LspServerSettings, LspSettings};

    #[test]
    fn defaults_cover_rust() {
        let servers = default_servers();
        let rust = servers
            .iter()
            .find(|s| s.name == "rust-analyzer")
            .expect("rust-analyzer default");
        assert!(rust.extensions.contains(&".rs".to_string()));
        assert!(rust.root_markers.contains(&"Cargo.toml".to_string()));
    }

    #[test]
    fn merge_overrides_named_server_fields() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "rust-analyzer".to_string(),
            LspServerSettings {
                command: Some("/custom/rust-analyzer".to_string()),
                ..Default::default()
            },
        );
        let config = Config {
            lsp: Some(LspSettings {
                servers: Some(overrides),
                ..Default::default()
            }),
            ..Config::default()
        };
        let servers = merge_servers(Some(&config));
        let rust = servers
            .iter()
            .find(|s| s.name == "rust-analyzer")
            .expect("rust-analyzer");
        assert_eq!(rust.command, "/custom/rust-analyzer");
        // Untouched fields survive.
        assert!(rust.extensions.contains(&".rs".to_string()));
    }

    #[test]
    fn merge_adds_new_server_with_command() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "solargraph".to_string(),
            LspServerSettings {
                command: Some("solargraph".to_string()),
                args: Some(vec!["stdio".to_string()]),
                extensions: Some(vec![".rb".to_string()]),
                root_markers: Some(vec!["Gemfile".to_string()]),
                ..Default::default()
            },
        );
        let config = Config {
            lsp: Some(LspSettings {
                servers: Some(overrides),
                ..Default::default()
            }),
            ..Config::default()
        };
        let servers = merge_servers(Some(&config));
        let ruby = servers
            .iter()
            .find(|s| s.name == "solargraph")
            .expect("solargraph added");
        assert_eq!(ruby.args, vec!["stdio".to_string()]);
        assert!(ruby.extensions.contains(&".rb".to_string()));
    }

    #[test]
    fn merge_skips_commandless_new_server() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mystery".to_string(),
            LspServerSettings::default(),
        );
        let config = Config {
            lsp: Some(LspSettings {
                servers: Some(overrides),
                ..Default::default()
            }),
            ..Config::default()
        };
        let servers = merge_servers(Some(&config));
        assert!(!servers.iter().any(|s| s.name == "mystery"));
        // Defaults intact.
        assert!(servers.iter().any(|s| s.name == "rust-analyzer"));
    }

    #[test]
    fn root_detection_walks_up_to_marker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("manifest");
        let nested = root.join("src/nested/deep");
        std::fs::create_dir_all(&nested).expect("dirs");
        let file = nested.join("lib.rs");
        std::fs::write(&file, "fn x() {}\n").expect("file");

        let registry = LspRegistry::new(root, None);
        let spec = registry.spec_for_file(&file).expect("spec");
        assert_eq!(spec.name, "rust-analyzer");
        assert_eq!(registry.workspace_root(&file, spec), root);
    }

    #[test]
    fn root_detection_falls_back_to_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        let other = tempfile::tempdir().expect("other");
        let file = other.path().join("lonely.rs");
        std::fs::write(&file, "fn x() {}\n").expect("file");
        let registry = LspRegistry::new(temp.path(), None);
        let spec = registry.spec_for_file(&file).expect("spec");
        assert_eq!(registry.workspace_root(&file, spec), temp.path());
    }

    #[test]
    fn language_ids_cover_defaults() {
        assert_eq!(language_id_for_extension(".rs"), Some("rust"));
        assert_eq!(language_id_for_extension(".tsx"), Some("typescriptreact"));
        assert_eq!(language_id_for_extension(".unknown"), None);
    }
}
