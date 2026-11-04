//! Opt-in media tools (bd-cv653.2.7).
//!
//! Vision, image generation/editing, and speech synthesis use native bounded
//! provider requests. `read_media` attaches local video/audio for Gemini-family
//! providers. The original public tool paths are re-exported here.

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

mod artifact;
mod generation;
mod speech;
mod transport;
pub(crate) mod video;
mod vision;
pub use generation::GenerateImageTool;
pub use speech::TtsTool;
pub use vision::InspectImageTool;

#[cfg(test)]
pub(crate) use transport::tests::peer as test_peer;
#[cfg(test)]
pub(crate) use transport::tests::peer_responses as test_peer_responses;

/// Session-owned credentials shared by native media tools and registry snapshots.
/// Resolution stays private to the native adapters and happens only at invocation.
#[derive(Clone, Default)]
pub struct MediaAuthScope {
    state: Arc<RwLock<MediaAuthState>>,
}

#[derive(Default)]
enum MediaAuthState {
    #[default]
    Unclaimed,
    Owned {
        owner: Arc<()>,
        auth: Option<Arc<crate::auth::AuthStorage>>,
    },
    Conflicted,
}

impl MediaAuthState {
    fn claim(&mut self, incoming: &Arc<()>) {
        match self {
            Self::Unclaimed => {
                *self = Self::Owned {
                    owner: Arc::clone(incoming),
                    auth: None,
                };
            }
            Self::Owned { owner, .. } if Arc::ptr_eq(owner, incoming) => {}
            // Keep this permanent, even after either AgentSession is dropped.
            // Old registry snapshots must never acquire a different session's key.
            _ => *self = Self::Conflicted,
        }
    }
}

impl MediaAuthScope {
    pub(crate) fn claim_owner(&self, owner: &Arc<()>) {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .claim(owner);
    }

    pub(crate) fn set(&self, owner: &Arc<()>, auth: crate::auth::AuthStorage) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.claim(owner);
        if let MediaAuthState::Owned { auth: stored, .. } = &mut *state {
            *stored = Some(Arc::new(auth));
        }
    }

    pub(crate) fn snapshot(
        &self,
    ) -> std::result::Result<Option<Arc<crate::auth::AuthStorage>>, &'static str> {
        match &*self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            MediaAuthState::Unclaimed => Ok(None),
            MediaAuthState::Owned { auth, .. } => Ok(auth.clone()),
            MediaAuthState::Conflicted => Err(
                "media credentials refused: this tool registry is shared by multiple AgentSessions; create a separate tool registry for each session",
            ),
        }
    }
}

pub const MAX_IMAGE_FILE_SIZE_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_TTS_TEXT_CHARS: usize = 4096;
/// Default decoded-byte cap for an inline video/audio block (`media.maxBytes`).
pub const DEFAULT_MEDIA_MAX_BYTES: u64 = 5 * 1024 * 1024;

// Deterministic test fixtures, reachable only in explicit mock mode or tests.
// Native adapters never substitute these for a missing provider response.
const MIN_VALID_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

// Header-only WAV retained for deterministic mocks and rejection regressions.
const MIN_VALID_WAV: &[u8] = &[
    0x52, 0x49, 0x46, 0x46, 0x24, 0x00, 0x00, 0x00, 0x57, 0x41, 0x56, 0x45, 0x66, 0x6D, 0x74, 0x20,
    0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x44, 0xAC, 0x00, 0x00, 0x88, 0x58, 0x01, 0x00,
    0x02, 0x00, 0x10, 0x00, 0x64, 0x61, 0x74, 0x61, 0x00, 0x00, 0x00, 0x00,
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MediaSettings {
    /// Default activation for `inspect_image`, `generate_image`, and `tts`.
    /// Individual switches override this after settings scopes are merged.
    #[serde(alias = "enableMedia")]
    pub enable_media: Option<bool>,
    #[serde(alias = "enableInspectImage")]
    pub enable_inspect_image: Option<bool>,
    #[serde(alias = "enableGenerateImage")]
    pub enable_generate_image: Option<bool>,
    #[serde(alias = "enableTts")]
    pub enable_tts: Option<bool>,
    /// Enable local inline video/audio attachments.
    #[serde(alias = "enableReadMedia")]
    pub enable_read_media: Option<bool>,
    /// Maximum decoded bytes in one inline `read_media` block.
    #[serde(alias = "maxBytes")]
    pub max_bytes: Option<u64>,
    #[serde(alias = "visionModel")]
    pub vision_model: Option<String>,
    #[serde(alias = "visionProvider")]
    pub vision_provider: Option<String>,
    #[serde(alias = "imageGenProvider")]
    pub image_gen_provider: Option<String>,
    #[serde(alias = "imageGenModel")]
    pub image_gen_model: Option<String>,
    #[serde(alias = "ttsVoice")]
    pub tts_voice: Option<String>,
    #[serde(alias = "ttsProvider")]
    pub tts_provider: Option<String>,
}

/// Map a `read_media` extension to the MIME spelling expected by Gemini.
pub fn media_mime_type_for_extension(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mov" => Some("video/mov"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "m4a" => Some("audio/m4a"),
        "ogg" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        _ => None,
    }
}

pub const READ_MEDIA_EXTENSIONS: &[&str] =
    &["mp4", "webm", "mov", "mp3", "wav", "m4a", "ogg", "flac"];

/// Load local video/audio as inline media for Gemini-family providers.
pub struct ReadMediaTool {
    cwd: PathBuf,
    max_bytes: u64,
}

impl ReadMediaTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            max_bytes: DEFAULT_MEDIA_MAX_BYTES,
        }
    }

    #[must_use]
    pub const fn with_max_bytes(mut self, max_bytes: Option<u64>) -> Self {
        if let Some(max_bytes) = max_bytes {
            self.max_bytes = max_bytes;
        }
        self
    }

    #[must_use]
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    fn resolve_path(&self, rel_or_abs: &str) -> PathBuf {
        self.cwd.join(rel_or_abs)
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for ReadMediaTool {
    fn name(&self) -> &str {
        "read_media"
    }

    fn label(&self) -> &str {
        "Read Media"
    }

    fn description(&self) -> &str {
        "Attach a local video or audio file (mp4, webm, mov, mp3, wav, m4a, ogg, flac) to the \
         conversation as inline media. Only Gemini-family models can watch/listen to it; other \
         providers see a text placeholder. Files above the configured size cap are rejected."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to a local video (mp4, webm, mov) or audio (mp3, wav, m4a, ogg, flac) file"
                }
            }
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let path_str = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::tool("read_media", "missing required path parameter"))?;
        let target_path = self.resolve_path(path_str);
        if !target_path.is_file() {
            return Err(Error::tool(
                "read_media",
                format!("media file not found: {}", target_path.display()),
            ));
        }
        let ext = target_path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_lowercase();
        let Some(mime_type) = media_mime_type_for_extension(&ext) else {
            return Err(Error::tool(
                "read_media",
                format!(
                    "unsupported media extension: .{ext} (supported: {})",
                    READ_MEDIA_EXTENSIONS.join(", ")
                ),
            ));
        };
        let metadata = std::fs::metadata(&target_path).map_err(|error| {
            Error::tool("read_media", format!("cannot stat media file: {error}"))
        })?;
        let size = metadata.len();
        if size > self.max_bytes {
            return Err(Error::tool(
                "read_media",
                format!(
                    "media file is {} ({size} bytes), above the {} cap ({} bytes); raise media.maxBytes or trim the file",
                    crate::model::format_media_size(size),
                    crate::model::format_media_size(self.max_bytes),
                    self.max_bytes
                ),
            ));
        }
        if size == 0 {
            return Err(Error::tool("read_media", "media file is empty"));
        }
        // Enforce the cap while reading, not merely after allocating the whole
        // file. The advisory metadata check above cannot prevent concurrent growth.
        let bytes = transport::read_capped(&target_path, "read_media", self.max_bytes)?;
        let size = bytes.len() as u64;
        let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
        let name = target_path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(crate::model::sanitize_media_name);
        let note = format!(
            "Read media file {} [{mime_type}, {}]",
            name.as_deref().unwrap_or(path_str),
            crate::model::format_media_size(size)
        );
        Ok(ToolOutput {
            content: vec![
                ContentBlock::Text(TextContent::new(note)),
                ContentBlock::Media(crate::model::MediaContent {
                    data,
                    mime_type: mime_type.to_string(),
                    name,
                }),
            ],
            details: Some(json!({
                "path": target_path.display().to_string(),
                "mime_type": mime_type,
                "size_bytes": size,
                "max_bytes": self.max_bytes
            })),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod auth_scope_tests {
    use super::*;
    use crate::auth::{AuthCredential, AuthStorage};

    fn stored_auth(path: &Path, key: &str) -> AuthStorage {
        let mut auth = AuthStorage::empty_at(path.to_path_buf());
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: key.to_string(),
            },
        );
        auth
    }

    #[test]
    fn same_media_owner_can_replace_and_clear_auth_without_changing_other_scopes() {
        let directory = tempfile::tempdir().expect("auth directory");
        let path = directory.path().join("auth.json");
        let scope = MediaAuthScope::default();
        let owner = Arc::new(());
        scope.claim_owner(&owner);
        assert!(scope.snapshot().expect("claimed scope").is_none());
        scope.set(&owner, stored_auth(&path, "first-key"));
        let captured = scope.snapshot().unwrap().unwrap();
        let earlier_view = scope.clone();

        let separate = MediaAuthScope::default();
        separate.set(&Arc::new(()), stored_auth(&path, "separate-key"));
        scope.set(&owner, stored_auth(&path, "replacement-key"));
        assert_eq!(
            earlier_view
                .snapshot()
                .unwrap()
                .unwrap()
                .api_key("openai")
                .as_deref(),
            Some("replacement-key")
        );
        assert_eq!(captured.api_key("openai").as_deref(), Some("first-key"));

        scope.set(&owner, AuthStorage::empty_at(path));
        assert!(
            earlier_view
                .snapshot()
                .unwrap()
                .unwrap()
                .api_key("openai")
                .is_none()
        );
        assert_eq!(
            separate
                .snapshot()
                .unwrap()
                .unwrap()
                .api_key("openai")
                .as_deref(),
            Some("separate-key"),
            "even identical auth paths do not share the session's credential snapshot"
        );
    }

    #[test]
    fn media_scope_never_reassigns_credentials_after_the_original_owner_drops() {
        let directory = tempfile::tempdir().expect("auth directory");
        let path = directory.path().join("auth.json");
        let scope = MediaAuthScope::default();
        let original = Arc::new(());
        scope.set(&original, stored_auth(&path, "first-key"));
        let earlier_view = scope.clone();
        drop(original);

        let second = Arc::new(());
        scope.claim_owner(&second);
        assert!(
            scope
                .snapshot()
                .unwrap_err()
                .contains("multiple AgentSessions")
        );
        scope.set(&second, stored_auth(&path, "must-not-replace"));
        assert!(earlier_view.snapshot().is_err());
        drop(second);

        scope.set(&Arc::new(()), AuthStorage::empty_at(path));
        assert!(
            scope.snapshot().is_err(),
            "ownership conflict must remain permanent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn conflicting_media_scope_refuses_commands_but_preserves_explicit_builder_keys() {
        let directory = tempfile::tempdir().expect("command workspace");
        let mut auth = stored_auth(
            &directory.path().join("auth.json"),
            "$CMD:printf invoked > credential-invoked; printf command-key",
        );
        auth.set_command_working_directory(directory.path())
            .unwrap();
        let scope = MediaAuthScope::default();
        scope.set(&Arc::new(()), auth);
        scope.claim_owner(&Arc::new(()));
        let mut transport =
            transport::Transport::default().with_base_url("http://127.0.0.1:1/v1/".to_string());
        transport.bind_auth_scope(scope);
        let owner = crate::agent_cx::AgentCx::for_testing_with_io();
        futures::executor::block_on(owner.with_current(async {
            let error = transport
                .api("tts", "openai", None, std::time::Duration::from_secs(1))
                .await
                .err()
                .expect("shared session credentials must be refused");
            assert!(error.to_string().contains("multiple AgentSessions"));
            assert!(
                transport
                    .api(
                        "tts",
                        "openai",
                        Some("builder-key"),
                        std::time::Duration::from_secs(1),
                    )
                    .await
                    .is_ok()
            );
            let empty_error = transport
                .api(
                    "tts",
                    "openai",
                    Some(" "),
                    std::time::Duration::from_secs(1),
                )
                .await
                .err()
                .expect("explicit empty key must still deny fallback");
            assert!(empty_error.to_string().contains("missing API key"));
            assert!(!empty_error.to_string().contains("multiple AgentSessions"));
        }));
        assert!(!directory.path().join("credential-invoked").exists());
    }
}
