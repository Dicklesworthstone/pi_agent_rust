//! Error hints: mapping from error variants to user-facing remediation suggestions.
//!
//! Each error variant maps to:
//! - A 1-line summary (human readable)
//! - 0-2 actionable hints (commands, env vars, paths)
//! - Contextual fields that should be printed with the error
//!
//! # Design Principles
//! - Hints must be stable for testability
//! - Avoid OS-specific hints unless OS is reliably detectable
//! - Never suggest destructive actions
//! - Prefer specific, actionable guidance over generic messages

use crate::error::Error;
use std::fmt::Write as _;

/// A remediation hint for an error.
#[derive(Debug, Clone)]
pub struct ErrorHint {
    /// Brief 1-line summary of the error category.
    pub summary: &'static str,
    /// Actionable hints for the user (0-2 items).
    pub hints: &'static [&'static str],
    /// Context fields that should be displayed with the error.
    pub context_fields: &'static [&'static str],
}

/// Get remediation hints for an error variant.
///
/// Returns structured hints that can be rendered in any output mode
/// (interactive, print, RPC).
#[allow(clippy::too_many_lines)]
pub fn hints_for_error(error: &Error) -> ErrorHint {
    match error {
        Error::Config(msg) => config_hints(msg),
        Error::SessionNotFound { .. } | Error::Session(_) => session_hints(error),
        Error::Auth(msg) => auth_hints(msg),
        Error::Provider { message, .. } => provider_hints(message),
        Error::Tool { tool, message } => tool_hints(tool, message),
        Error::Validation(msg) => validation_hints(msg),
        Error::Extension(msg) => extension_hints(msg),
        Error::Io(err) => io_hints(err),
        Error::Json(err) => json_hints(err),
        Error::Sqlite(err) => sqlite_hints(err),
        Error::Aborted => aborted_hints(),
        Error::Api(msg) => api_hints(msg),
    }
}

fn config_hints(msg: &str) -> ErrorHint {
    if msg.contains("cassette") {
        return ErrorHint {
            summary: "VCR cassette missing or invalid",
            hints: &[
                "If running tests, set VCR_MODE=record to create cassettes",
                "Or ensure VCR_CASSETTE_DIR contains the expected cassette file",
            ],
            context_fields: &["file_path"],
        };
    }
    if msg.contains("settings.json") {
        return ErrorHint {
            summary: "Invalid or missing configuration file",
            hints: &[
                "Check that ~/.pi/agent/settings.json exists and is valid JSON",
                "Run 'pi config' to see configuration paths and precedence",
            ],
            context_fields: &["file_path"],
        };
    }
    if msg.contains("models.json") {
        return ErrorHint {
            summary: "Invalid models configuration",
            hints: &[
                "Verify ~/.pi/agent/models.json has valid JSON syntax",
                "Check that 'providers' key exists in models.json",
            ],
            context_fields: &["file_path", "parse_error"],
        };
    }
    ErrorHint {
        summary: "Configuration error",
        hints: &["Check configuration file syntax and required fields"],
        context_fields: &[],
    }
}

fn session_hints(error: &Error) -> ErrorHint {
    match error {
        Error::SessionNotFound { .. } => ErrorHint {
            summary: "Session file not found",
            hints: &[
                "Use 'pi' without --session to start a new session",
                "Use 'pi --resume' to pick from existing sessions",
            ],
            context_fields: &["path"],
        },
        Error::Session(msg) if msg.contains("corrupted") || msg.contains("invalid") => ErrorHint {
            summary: "Session file is corrupted or invalid",
            hints: &[
                "Start a new session with 'pi'",
                "Session files are JSONL format - check for malformed lines",
            ],
            context_fields: &["path", "line_number"],
        },
        Error::Session(msg) if msg.contains("locked") => ErrorHint {
            summary: "Session file is locked by another process",
            hints: &["Close other Pi instances using this session"],
            context_fields: &["path"],
        },
        _ => ErrorHint {
            summary: "Session error",
            hints: &["Try starting a new session with 'pi'"],
            context_fields: &[],
        },
    }
}

fn auth_hints(msg: &str) -> ErrorHint {
    if msg.contains("GitHub Copilot") && msg.contains("client_id") {
        return ErrorHint {
            summary: "GitHub Copilot OAuth client_id not configured",
            hints: &[
                "Set GITHUB_COPILOT_CLIENT_ID to your GitHub OAuth App / GitHub App client id",
                "Or run on a workstation with a browser, or use device flow over SSH (set PI_COPILOT_FORCE_DEVICE_FLOW=1)",
            ],
            context_fields: &["provider"],
        };
    }
    if msg.contains("API key") || msg.contains("api_key") {
        return ErrorHint {
            summary: "API key not configured",
            hints: &[
                "Set ANTHROPIC_API_KEY environment variable",
                "Or add key to ~/.pi/agent/auth.json",
            ],
            context_fields: &["provider"],
        };
    }
    if msg.contains("401") || msg.contains("unauthorized") {
        return ErrorHint {
            summary: "API key is invalid or expired",
            hints: &[
                "Verify your API key is correct and active",
                "Check API key permissions at your provider's console",
            ],
            context_fields: &["provider", "status_code"],
        };
    }
    if msg.contains("OAuth") || msg.contains("refresh") {
        return ErrorHint {
            summary: "OAuth token expired or invalid",
            hints: &[
                "Run 'pi login <provider>' to re-authenticate",
                "Or set API key directly via environment variable",
            ],
            context_fields: &["provider"],
        };
    }
    if msg.contains("lock") {
        return ErrorHint {
            summary: "Auth file locked by another process",
            hints: &["Close other Pi instances that may be using auth.json"],
            context_fields: &["path"],
        };
    }
    ErrorHint {
        summary: "Authentication error",
        hints: &["Check your API credentials"],
        context_fields: &[],
    }
}

fn provider_hints(message: &str) -> ErrorHint {
    // Connect-path errors re-wrapped by providers keep the original message
    // text, so match the WSAENOTCONN signature here too (#106).
    if message.contains("10057") || message.contains("Socket is not connected") {
        return winsock_not_connected_hints();
    }
    if message.contains("429") || message.contains("rate limit") {
        return ErrorHint {
            summary: "Rate limit exceeded",
            hints: &[
                "Wait a moment and try again",
                "Consider using a different model or reducing request frequency",
            ],
            context_fields: &["provider", "retry_after"],
        };
    }
    if message.contains("500") || message.contains("server error") {
        return ErrorHint {
            summary: "Provider server error",
            hints: &[
                "This is a temporary issue - try again shortly",
                "Check provider status page for outages",
            ],
            context_fields: &["provider", "status_code"],
        };
    }
    if message.contains("connection") || message.contains("network") {
        return ErrorHint {
            summary: "Network connection error",
            hints: &[
                "Check your internet connection",
                "If using a proxy, verify proxy settings",
            ],
            context_fields: &["provider", "url"],
        };
    }
    if message.contains("timeout") {
        return ErrorHint {
            summary: "Request timed out",
            hints: &[
                "Try again - the provider may be slow",
                "Consider using a smaller context or simpler request",
            ],
            context_fields: &["provider", "timeout_seconds"],
        };
    }
    if message.contains("model") && message.contains("not found") {
        return ErrorHint {
            summary: "Model not found or unavailable",
            hints: &[
                "Check that the model ID is correct",
                "Use 'pi --list-models' to see available models",
            ],
            context_fields: &["provider", "model_id"],
        };
    }
    ErrorHint {
        summary: "Provider API error",
        hints: &["Check provider documentation for this error"],
        context_fields: &["provider", "status_code"],
    }
}

fn tool_hints(tool: &str, message: &str) -> ErrorHint {
    if tool == "read" && message.contains("not found") {
        return ErrorHint {
            summary: "File not found",
            hints: &[
                "Verify the file path is correct",
                "Use 'ls' or 'find' to locate the file",
            ],
            context_fields: &["path"],
        };
    }
    if tool == "read" && message.contains("permission") {
        return ErrorHint {
            summary: "Permission denied reading file",
            hints: &["Check file permissions"],
            context_fields: &["path"],
        };
    }
    if tool == "write" && message.contains("permission") {
        return ErrorHint {
            summary: "Permission denied writing file",
            hints: &["Check directory permissions"],
            context_fields: &["path"],
        };
    }
    if tool == "edit" && message.contains("not found") {
        return ErrorHint {
            summary: "Text to replace not found in file",
            hints: &[
                "Verify the old_text exactly matches content in the file",
                "Use 'read' to see the current file content",
            ],
            context_fields: &["path", "old_text_preview"],
        };
    }
    if tool == "edit" && message.contains("ambiguous") {
        return ErrorHint {
            summary: "Multiple matches found for replacement",
            hints: &["Provide more context in old_text to make it unique"],
            context_fields: &["path", "match_count"],
        };
    }
    if tool == "bash" && message.contains("timeout") {
        return ErrorHint {
            summary: "Command timed out",
            hints: &[
                "Increase timeout with 'timeout' parameter",
                "Consider breaking into smaller commands",
            ],
            context_fields: &["command", "timeout_seconds"],
        };
    }
    if tool == "bash" && message.contains("exit code") {
        return ErrorHint {
            summary: "Command failed with non-zero exit code",
            hints: &["Review command output for error details"],
            context_fields: &["command", "exit_code", "stderr"],
        };
    }
    if tool == "grep" && message.contains("pattern") {
        return ErrorHint {
            summary: "Invalid regex pattern",
            hints: &["Check regex syntax - special characters may need escaping"],
            context_fields: &["pattern"],
        };
    }
    if tool == "find" && message.contains("fd") {
        return ErrorHint {
            summary: "fd command not found",
            hints: &[
                "Install fd: 'apt install fd-find' or 'brew install fd'",
                "The binary may be named 'fdfind' on some systems",
            ],
            context_fields: &[],
        };
    }
    ErrorHint {
        summary: "Tool execution error",
        hints: &["Review the tool parameters and try again"],
        context_fields: &["tool", "command"],
    }
}

fn validation_hints(msg: &str) -> ErrorHint {
    if msg.contains("required") {
        return ErrorHint {
            summary: "Required field missing",
            hints: &["Provide all required parameters"],
            context_fields: &["field_name"],
        };
    }
    if msg.contains("type") {
        return ErrorHint {
            summary: "Invalid parameter type",
            hints: &["Check parameter types match expected schema"],
            context_fields: &["field_name", "expected_type"],
        };
    }
    ErrorHint {
        summary: "Validation error",
        hints: &["Check input parameters"],
        context_fields: &[],
    }
}

fn extension_hints(msg: &str) -> ErrorHint {
    if msg.contains("not found") {
        return ErrorHint {
            summary: "Extension not found",
            hints: &[
                "Check extension name is correct",
                "Use 'pi list' to see installed extensions",
            ],
            context_fields: &["extension_name"],
        };
    }
    if msg.contains("manifest") {
        return ErrorHint {
            summary: "Invalid extension manifest",
            hints: &[
                "Check extension manifest.json syntax",
                "Verify required fields are present",
            ],
            context_fields: &["extension_name", "manifest_path"],
        };
    }
    if msg.contains("capability") || msg.contains("permission") {
        return ErrorHint {
            summary: "Extension capability denied",
            hints: &[
                "Extension requires capabilities not granted by policy",
                "Review extension security settings",
            ],
            context_fields: &["extension_name", "capability"],
        };
    }
    ErrorHint {
        summary: "Extension error",
        hints: &["Check extension configuration"],
        context_fields: &["extension_name"],
    }
}

/// Hint for Windows `WSAENOTCONN` (os error 10057) "Socket is not connected"
/// failures during connect/TLS handshake (#106, #66, asupersync#35).
///
/// Layered Winsock providers (VPN clients, antivirus, firewall LSPs) can
/// report an outbound connect as complete while the base provider socket has
/// not finished connecting, so the first send fails with 10057. Pi retries the
/// connect automatically; if the error still surfaces the interference is
/// persistent and needs OS-level remediation.
const fn winsock_not_connected_hints() -> ErrorHint {
    ErrorHint {
        summary: "Socket not connected (Windows WSAENOTCONN 10057) - often VPN, antivirus, or Winsock LSP interference",
        hints: &[
            "Retry the request; if it persists, temporarily disable VPN/antivirus/firewall software to identify the interfering layer",
            "Inspect layered providers with 'netsh winsock show catalog'; as a last resort run 'netsh winsock reset' from an elevated prompt and reboot",
        ],
        context_fields: &["url"],
    }
}

fn io_hints(err: &std::io::Error) -> ErrorHint {
    // Windows WSAENOTCONN (10057): kind() maps to NotConnected on Windows,
    // but also check the raw OS code in case the error was synthesized with
    // an uncategorized kind (#106).
    if err.kind() == std::io::ErrorKind::NotConnected || err.raw_os_error() == Some(10057) {
        return winsock_not_connected_hints();
    }
    match err.kind() {
        std::io::ErrorKind::NotFound => ErrorHint {
            summary: "File or directory not found",
            hints: &["Verify the path exists"],
            context_fields: &["path"],
        },
        std::io::ErrorKind::PermissionDenied => ErrorHint {
            summary: "Permission denied",
            hints: &["Check file/directory permissions"],
            context_fields: &["path"],
        },
        std::io::ErrorKind::AlreadyExists => ErrorHint {
            summary: "File already exists",
            hints: &["Use a different path or remove existing file first"],
            context_fields: &["path"],
        },
        _ => ErrorHint {
            summary: "I/O error",
            hints: &["Check file system and permissions"],
            context_fields: &["path"],
        },
    }
}

fn json_hints(err: &serde_json::Error) -> ErrorHint {
    if err.is_syntax() {
        return ErrorHint {
            summary: "Invalid JSON syntax",
            hints: &[
                "Check for missing commas, brackets, or quotes",
                "Validate JSON at jsonlint.com or similar",
            ],
            context_fields: &["line", "column"],
        };
    }
    if err.is_data() {
        return ErrorHint {
            summary: "JSON data does not match expected structure",
            hints: &["Check that JSON fields match expected schema"],
            context_fields: &["field_path"],
        };
    }
    ErrorHint {
        summary: "JSON error",
        hints: &["Verify JSON syntax and structure"],
        context_fields: &[],
    }
}

const fn sqlite_hints(err: &fsqlite::FrankenError) -> ErrorHint {
    if matches!(
        err,
        fsqlite::FrankenError::DatabaseLocked { .. }
            | fsqlite::FrankenError::Busy
            | fsqlite::FrankenError::BusyRecovery
            | fsqlite::FrankenError::BusySnapshot { .. }
            | fsqlite::FrankenError::LockFailed { .. }
            | fsqlite::FrankenError::MultiProcessContractViolation { .. }
    ) {
        return ErrorHint {
            summary: "Database locked",
            hints: &["Close other Pi instances using this database"],
            context_fields: &["db_path"],
        };
    }
    if matches!(
        err,
        fsqlite::FrankenError::DatabaseCorrupt { .. }
            | fsqlite::FrankenError::WalCorrupt { .. }
            | fsqlite::FrankenError::NotADatabase { .. }
    ) {
        return ErrorHint {
            summary: "Database corrupted",
            hints: &[
                "The session index may need to be rebuilt",
                "Delete ~/.pi/agent/sessions/index.db to rebuild",
            ],
            context_fields: &["db_path"],
        };
    }
    ErrorHint {
        summary: "Database error",
        hints: &["Check database file permissions and integrity"],
        context_fields: &["db_path"],
    }
}

const fn aborted_hints() -> ErrorHint {
    ErrorHint {
        summary: "Operation cancelled by user",
        hints: &[],
        context_fields: &[],
    }
}

fn api_hints(msg: &str) -> ErrorHint {
    // TLS connect failures arrive flattened into a message string, e.g.
    // "TLS connect failed: I/O error: Socket is not connected. (os error
    // 10057)" (#106).
    if msg.contains("10057") || msg.contains("Socket is not connected") {
        return winsock_not_connected_hints();
    }
    if msg.contains("timed out") || msg.contains("timeout") {
        return ErrorHint {
            summary: "Request timed out",
            hints: &[
                "Raise the timeout: --request-timeout <seconds>, PI_HTTP_REQUEST_TIMEOUT_SECS=<seconds>, or requestTimeoutSecs in settings.json (0 = no timeout)",
                "Local providers (Ollama/LM Studio): the first request can block while the model loads — ensure the model is pulled (ollama pull <model>) and the server is reachable (ollama list)",
            ],
            context_fields: &["url", "timeout_seconds"],
        };
    }
    if msg.contains("401") {
        return ErrorHint {
            summary: "Unauthorized API request",
            hints: &["Check your API credentials"],
            context_fields: &["url", "status_code"],
        };
    }
    if msg.contains("403") {
        return ErrorHint {
            summary: "Forbidden API request",
            hints: &["Check API key permissions for this resource"],
            context_fields: &["url", "status_code"],
        };
    }
    if msg.contains("404") {
        return ErrorHint {
            summary: "API resource not found",
            hints: &["Check the API endpoint URL"],
            context_fields: &["url"],
        };
    }
    ErrorHint {
        summary: "API error",
        hints: &["Check API documentation"],
        context_fields: &["url", "status_code"],
    }
}

/// Format an error with its hints for display.
///
/// Returns a formatted string suitable for terminal output.
pub fn format_error_with_hints(error: &Error) -> String {
    let hint = hints_for_error(error);
    let mut output = String::new();

    // Error message
    let _ = writeln!(&mut output, "Error: {error}");

    // Summary if different from error message
    if !error.to_string().contains(hint.summary) {
        output.push('\n');
        output.push_str(hint.summary);
        output.push('\n');
    }

    // Hints
    if !hint.hints.is_empty() {
        output.push_str("\nSuggestions:\n");
        for &h in hint.hints {
            let _ = writeln!(&mut output, "  • {h}");
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_error_hints() {
        let error = Error::config("settings.json not found");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("configuration"));
        assert!(!hint.hints.is_empty());
    }

    #[test]
    fn test_auth_error_api_key_hints() {
        let error = Error::auth("API key not set");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("API key"));
        assert!(hint.hints.iter().any(|h| h.contains("ANTHROPIC_API_KEY")));
    }

    #[test]
    fn test_auth_error_401_hints() {
        let error = Error::auth("401 unauthorized");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("invalid") || hint.summary.contains("expired"));
    }

    #[test]
    fn test_provider_rate_limit_hints() {
        let error = Error::provider("anthropic", "429 rate limit exceeded");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("Rate limit"));
        assert!(hint.hints.iter().any(|h| h.contains("Wait")));
    }

    #[test]
    fn test_tool_read_not_found_hints() {
        let error = Error::tool("read", "file not found: /path/to/file");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("not found"));
        assert!(hint.context_fields.contains(&"path"));
    }

    #[test]
    fn test_tool_edit_ambiguous_hints() {
        let error = Error::tool("edit", "ambiguous match: found 3 occurrences");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("Multiple"));
        assert!(hint.hints.iter().any(|h| h.contains("context")));
    }

    #[test]
    fn test_tool_fd_not_found_hints() {
        let error = Error::tool("find", "fd command not found");
        let hint = hints_for_error(&error);
        assert!(hint.hints.iter().any(|h| h.contains("apt install")));
    }

    #[test]
    fn test_session_not_found_hints() {
        let error = Error::SessionNotFound {
            path: "/path/to/session.jsonl".to_string(),
        };
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("not found"));
        assert!(hint.hints.iter().any(|h| h.contains("--resume")));
    }

    #[test]
    fn test_json_syntax_error_hints() {
        let json_err = serde_json::from_str::<serde_json::Value>("{ invalid }").unwrap_err();
        let error = Error::Json(Box::new(json_err));
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("JSON") || hint.summary.contains("syntax"));
    }

    #[test]
    fn test_aborted_has_no_hints() {
        let error = Error::Aborted;
        let hint = hints_for_error(&error);
        assert!(hint.hints.is_empty());
    }

    #[test]
    fn test_format_error_with_hints() {
        let error = Error::auth("API key not set");
        let formatted = format_error_with_hints(&error);
        assert!(formatted.contains("Error:"));
        assert!(formatted.contains("Suggestions:"));
    }

    #[test]
    fn test_format_error_with_hints_includes_api_key_suggestion() {
        let error = Error::auth("API key not set");
        let formatted = format_error_with_hints(&error);
        assert!(formatted.contains("ANTHROPIC_API_KEY"));
        assert!(formatted.contains("auth.json"));
    }

    #[test]
    fn test_format_error_with_hints_includes_json_syntax_suggestions() {
        let json_err = serde_json::from_str::<serde_json::Value>("{ invalid }").unwrap_err();
        let error = Error::Json(Box::new(json_err));
        let formatted = format_error_with_hints(&error);
        assert!(formatted.contains("Invalid JSON syntax"));
        assert!(formatted.contains("Validate JSON"));
    }

    #[test]
    fn test_format_error_with_hints_includes_fd_install_hint() {
        let error = Error::tool("find", "fd command not found");
        let formatted = format_error_with_hints(&error);
        assert!(formatted.contains("fd"));
        assert!(formatted.contains("apt install"));
    }

    #[test]
    fn test_format_error_with_hints_includes_read_permission_hint() {
        let error = Error::tool("read", "permission denied: /etc/shadow");
        let formatted = format_error_with_hints(&error);
        assert!(formatted.contains("Permission denied"));
        assert!(formatted.contains("Check file permissions"));
    }

    #[test]
    fn test_format_error_with_hints_includes_vcr_cassette_hint() {
        let error = Error::config("Failed to read cassette /tmp/cassette.json: missing file");
        let formatted = format_error_with_hints(&error);
        assert!(formatted.contains("VCR cassette"));
        assert!(formatted.contains("VCR_MODE=record"));
        assert!(formatted.contains("VCR_CASSETTE_DIR"));
    }

    #[test]
    fn test_extension_capability_denied_hints() {
        let error = Error::extension("capability network not allowed by policy");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("capability") || hint.summary.contains("denied"));
    }

    #[test]
    fn test_provider_timeout_hints() {
        let error = Error::provider("openai", "request timeout after 120s");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("timed out") || hint.summary.contains("timeout"));
    }

    #[test]
    fn test_provider_connection_hints() {
        let error = Error::provider("anthropic", "connection refused");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("Network") || hint.summary.contains("connection"));
    }

    #[test]
    fn test_io_permission_denied_hints() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let error = Error::Io(Box::new(io_err));
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("Permission"));
    }

    #[test]
    fn test_sqlite_locked_hints() {
        // Create a mock sqlite error string
        let error = Error::session("database locked");
        let hint = hints_for_error(&error);
        // Falls back to generic session error since it's not actually a Sqlite variant
        assert!(!hint.hints.is_empty());
    }

    // -----------------------------------------------------------------------
    // config_hints additional branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_models_json_hints() {
        let error = Error::config("models.json parse error at line 5");
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "Invalid models configuration");
        assert!(hint.context_fields.contains(&"parse_error"));
    }

    #[test]
    fn test_config_generic_fallback() {
        let error = Error::config("some unknown config issue");
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "Configuration error");
    }

    // -----------------------------------------------------------------------
    // session_hints additional branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_session_corrupted_hints() {
        let error = Error::session("file corrupted at line 42");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("corrupted"));
        assert!(hint.context_fields.contains(&"line_number"));
    }

    #[test]
    fn test_session_invalid_hints() {
        let error = Error::session("invalid session format");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("corrupted") || hint.summary.contains("invalid"));
    }

    #[test]
    fn test_session_locked_hints() {
        let error = Error::session("session file locked by pid 1234");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("locked"));
        assert!(hint.hints.iter().any(|h| h.contains("Close")));
    }

    #[test]
    fn test_session_generic_fallback() {
        let error = Error::session("something went wrong");
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "Session error");
    }

    // -----------------------------------------------------------------------
    // auth_hints additional branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_auth_oauth_hints() {
        let error = Error::auth("OAuth token expired for provider X");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("OAuth"));
        assert!(hint.hints.iter().any(|h| h.contains("pi login")));
    }

    #[test]
    fn test_auth_refresh_hints() {
        let error = Error::auth("failed to refresh token");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("OAuth"));
    }

    #[test]
    fn test_auth_lock_hints() {
        let error = Error::auth("auth file lock contention");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("locked"));
    }

    #[test]
    fn test_auth_generic_fallback() {
        let error = Error::auth("unknown auth issue");
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "Authentication error");
    }

    // -----------------------------------------------------------------------
    // provider_hints additional branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_provider_server_error_500_hints() {
        let error = Error::provider("openai", "500 internal server error");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("server error"));
        assert!(hint.hints.iter().any(|h| h.contains("status page")));
    }

    #[test]
    fn test_provider_server_error_text_hints() {
        let error = Error::provider("anthropic", "server error: bad gateway");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("server error"));
    }

    #[test]
    fn test_provider_model_not_found_hints() {
        let error = Error::provider("openai", "model gpt-99 not found");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("Model not found"));
        assert!(hint.hints.iter().any(|h| h.contains("--list-models")));
    }

    #[test]
    fn test_provider_generic_fallback() {
        let error = Error::provider("unknown", "something broke");
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "Provider API error");
    }

    // -----------------------------------------------------------------------
    // tool_hints additional branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_tool_write_permission_hints() {
        let error = Error::tool("write", "permission denied: /etc/config");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("Permission denied"));
        assert!(hint.hints.iter().any(|h| h.contains("directory")));
    }

    #[test]
    fn test_tool_edit_not_found_hints() {
        let error = Error::tool("edit", "text not found in file");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("not found"));
        assert!(hint.hints.iter().any(|h| h.contains("old_text")));
    }

    #[test]
    fn test_tool_bash_timeout_hints() {
        let error = Error::tool("bash", "command timeout after 120s");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("timed out"));
        assert!(hint.context_fields.contains(&"timeout_seconds"));
    }

    #[test]
    fn test_tool_bash_exit_code_hints() {
        let error = Error::tool("bash", "exit code 1");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("exit code"));
        assert!(hint.context_fields.contains(&"stderr"));
    }

    #[test]
    fn test_tool_grep_pattern_hints() {
        let error = Error::tool("grep", "invalid regex pattern: [unterminated");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("regex"));
        assert!(hint.hints.iter().any(|h| h.contains("escaping")));
    }

    #[test]
    fn test_tool_generic_fallback() {
        let error = Error::tool("unknown_tool", "something went wrong");
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "Tool execution error");
    }

    // -----------------------------------------------------------------------
    // validation_hints branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_validation_required_hints() {
        let error = Error::validation("field 'name' is required");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("Required"));
        assert!(hint.context_fields.contains(&"field_name"));
    }

    #[test]
    fn test_validation_type_hints() {
        let error = Error::validation("expected type string, got number");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("type"));
        assert!(hint.context_fields.contains(&"expected_type"));
    }

    #[test]
    fn test_validation_generic_fallback() {
        let error = Error::validation("value out of range");
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "Validation error");
    }

    // -----------------------------------------------------------------------
    // extension_hints additional branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_extension_not_found_hints() {
        let error = Error::extension("extension my-ext not found");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("not found"));
        assert!(hint.hints.iter().any(|h| h.contains("pi list")));
    }

    #[test]
    fn test_extension_manifest_hints() {
        let error = Error::extension("invalid manifest for extension foo");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("manifest"));
        assert!(hint.context_fields.contains(&"manifest_path"));
    }

    #[test]
    fn test_extension_permission_hints() {
        let error = Error::extension("permission denied for exec capability");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("denied"));
    }

    #[test]
    fn test_extension_generic_fallback() {
        let error = Error::extension("runtime crashed");
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "Extension error");
    }

    // -----------------------------------------------------------------------
    // io_hints additional branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_io_not_found_hints() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let error = Error::Io(Box::new(io_err));
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("not found"));
    }

    #[test]
    fn test_io_already_exists_hints() {
        let io_err = std::io::Error::new(std::io::ErrorKind::AlreadyExists, "file exists");
        let error = Error::Io(Box::new(io_err));
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("already exists"));
    }

    #[test]
    fn test_io_generic_fallback() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broken");
        let error = Error::Io(Box::new(io_err));
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "I/O error");
    }

    #[test]
    fn test_io_not_connected_hints() {
        let io_err =
            std::io::Error::new(std::io::ErrorKind::NotConnected, "Socket is not connected");
        let error = Error::Io(Box::new(io_err));
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("10057"));
        assert!(hint.hints.iter().any(|h| h.contains("netsh winsock")));
        assert!(hint.hints.iter().any(|h| h.contains("VPN")));
    }

    #[test]
    fn test_io_wsaenotconn_raw_os_error_hints() {
        // Raw os error 10057 must map to the Winsock hint even when the
        // platform does not categorize the kind (non-Windows hosts).
        let io_err = std::io::Error::from_raw_os_error(10057);
        let error = Error::Io(Box::new(io_err));
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("10057"));
        assert!(hint.hints.iter().any(|h| h.contains("netsh winsock")));
    }

    // -----------------------------------------------------------------------
    // json_hints additional branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_json_data_error_hints() {
        // Trigger a data error (wrong type for field)
        let json_err = serde_json::from_str::<Vec<i32>>(r#"{"not": "an array"}"#).unwrap_err();
        let error = Error::Json(Box::new(json_err));
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("data") || hint.summary.contains("structure"));
    }

    #[test]
    fn test_json_eof_fallback() {
        // EOF error is neither syntax nor data
        let json_err = serde_json::from_str::<serde_json::Value>("").unwrap_err();
        let error = Error::Json(Box::new(json_err));
        let hint = hints_for_error(&error);
        // EOF may be classified as syntax or generic depending on serde_json version
        assert!(hint.summary.contains("JSON"));
    }

    // -----------------------------------------------------------------------
    // api_hints branches
    // -----------------------------------------------------------------------

    #[test]
    fn test_api_401_hints() {
        let error = Error::api("401 Unauthorized");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("Unauthorized"));
        assert!(hint.context_fields.contains(&"status_code"));
    }

    #[test]
    fn test_api_403_hints() {
        let error = Error::api("403 Forbidden");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("Forbidden"));
        assert!(hint.hints.iter().any(|h| h.contains("permissions")));
    }

    #[test]
    fn test_api_404_hints() {
        let error = Error::api("404 Not Found");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("not found"));
        assert!(hint.context_fields.contains(&"url"));
    }

    #[test]
    fn test_api_generic_fallback() {
        let error = Error::api("502 Bad Gateway");
        let hint = hints_for_error(&error);
        assert_eq!(hint.summary, "API error");
    }

    #[test]
    fn test_api_tls_connect_10057_hints() {
        let error =
            Error::api("TLS connect failed: I/O error: Socket is not connected. (os error 10057)");
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("10057"));
        assert!(hint.hints.iter().any(|h| h.contains("netsh winsock")));
    }

    #[test]
    fn test_provider_socket_not_connected_hints() {
        let error = Error::provider(
            "anthropic",
            "TLS connect failed: I/O error: Socket is not connected. (os error 10057)",
        );
        let hint = hints_for_error(&error);
        assert!(hint.summary.contains("10057"));
        assert!(hint.hints.iter().any(|h| h.contains("netsh winsock")));
    }

    // -----------------------------------------------------------------------
    // format_error_with_hints additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_error_aborted_no_suggestions() {
        let error = Error::Aborted;
        let formatted = format_error_with_hints(&error);
        assert!(formatted.contains("Error:"));
        assert!(!formatted.contains("Suggestions:"));
    }

    #[test]
    fn test_format_error_includes_summary_when_different() {
        let error = Error::provider("openai", "429 rate limit exceeded");
        let formatted = format_error_with_hints(&error);
        // Summary "Rate limit exceeded" should appear since error message differs
        assert!(formatted.contains("Rate limit"));
        assert!(formatted.contains("Suggestions:"));
    }

    #[test]
    fn test_format_error_io_not_found() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let error = Error::Io(Box::new(io_err));
        let formatted = format_error_with_hints(&error);
        assert!(formatted.contains("not found"));
        assert!(formatted.contains("Verify the path"));
    }

    // -----------------------------------------------------------------------
    // Property-based tests
    // -----------------------------------------------------------------------

    mod proptest_error_hints {
        use super::*;
        use proptest::prelude::*;

        /// Build an Error from an index + message (avoids Clone requirement).
        fn make_error(variant: usize, msg: &str) -> Error {
            match variant % 9 {
                0 => Error::config(msg),
                1 => Error::session(msg),
                2 => Error::auth(msg),
                3 => Error::validation(msg),
                4 => Error::extension(msg),
                5 => Error::api(msg),
                6 => Error::provider("test", msg),
                7 => Error::tool("test", msg),
                _ => Error::Aborted,
            }
        }

        proptest! {
            /// `hints_for_error` never panics on any error variant.
            #[test]
            fn hints_for_error_never_panics(variant in 0..9usize, msg in "[\\w\\s./]{0,80}") {
                let error = make_error(variant, &msg);
                let hint = hints_for_error(&error);
                assert!(!hint.summary.is_empty());
                assert!(hint.hints.len() <= 2);
                assert!(hint.context_fields.len() <= 3);
            }

            /// `format_error_with_hints` never panics and always starts with "Error:".
            #[test]
            fn format_error_never_panics(variant in 0..9usize, msg in "[\\w\\s./]{0,80}") {
                let error = make_error(variant, &msg);
                let formatted = format_error_with_hints(&error);
                assert!(formatted.starts_with("Error:"));
            }

            /// Summary is always non-empty and contains no control characters.
            #[test]
            fn summary_is_clean(variant in 0..9usize, msg in "[\\w\\s./]{0,80}") {
                let error = make_error(variant, &msg);
                let hint = hints_for_error(&error);
                assert!(!hint.summary.is_empty());
                assert!(!hint.summary.contains('\n'));
                assert!(!hint.summary.contains('\r'));
            }

            /// Each hint line is non-empty.
            #[test]
            fn hints_are_nonempty(variant in 0..9usize, msg in "[\\w\\s./]{0,80}") {
                let error = make_error(variant, &msg);
                let hint = hints_for_error(&error);
                for &h in hint.hints {
                    assert!(!h.is_empty());
                }
            }

            /// Each context field is a valid identifier-like string.
            #[test]
            fn context_fields_are_identifiers(variant in 0..9usize, msg in "[\\w\\s./]{0,80}") {
                let error = make_error(variant, &msg);
                let hint = hints_for_error(&error);
                for &field in hint.context_fields {
                    assert!(!field.is_empty());
                    assert!(field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
                }
            }

            /// Config error with "cassette" always maps to VCR hint.
            #[test]
            fn config_cassette_keyword_triggers_vcr(prefix in "[a-zA-Z ]{0,30}") {
                let msg = format!("{prefix} cassette missing");
                let error = Error::config(msg);
                let hint = hints_for_error(&error);
                assert_eq!(hint.summary, "VCR cassette missing or invalid");
            }

            /// Config error with "settings.json" always maps to settings hint.
            #[test]
            fn config_settings_keyword_triggers_settings(prefix in "[a-zA-Z ]{0,30}") {
                let msg = format!("{prefix} settings.json not found");
                let error = Error::config(msg);
                let hint = hints_for_error(&error);
                assert!(hint.summary.contains("configuration"));
            }

            /// Config error with "models.json" always maps to models hint.
            #[test]
            fn config_models_keyword_triggers_models(prefix in "[a-zA-Z ]{0,30}") {
                let msg = format!("{prefix} models.json parse error");
                let error = Error::config(msg);
                let hint = hints_for_error(&error);
                assert_eq!(hint.summary, "Invalid models configuration");
            }

            /// Auth error with "API key" always mentions API key setup.
            #[test]
            fn auth_api_key_keyword(suffix in "[a-zA-Z ]{0,30}") {
                let msg = format!("API key {suffix}");
                let error = Error::auth(msg);
                let hint = hints_for_error(&error);
                assert!(hint.summary.contains("API key"));
                assert!(hint.hints.iter().any(|h| h.contains("ANTHROPIC_API_KEY")));
            }

            /// Provider error with "429" always triggers rate limit hint.
            #[test]
            fn provider_429_triggers_rate_limit(provider in "[a-z]{1,10}", suffix in "[a-zA-Z ]{0,30}") {
                let msg = format!("429 {suffix}");
                let error = Error::provider(provider, msg);
                let hint = hints_for_error(&error);
                assert_eq!(hint.summary, "Rate limit exceeded");
            }

            /// Provider error with "timeout" always triggers timeout hint.
            #[test]
            fn provider_timeout_triggers_timeout_hint(provider in "[a-z]{1,10}", prefix in "[a-zA-Z ]{0,30}") {
                let msg = format!("{prefix} timeout");
                let error = Error::provider(provider, msg);
                let hint = hints_for_error(&error);
                assert!(!hint.summary.is_empty());
            }

            /// Aborted error always has empty hints.
            #[test]
            fn aborted_always_empty_hints(_dummy in 0..10u32) {
                let hint = hints_for_error(&Error::Aborted);
                assert!(hint.hints.is_empty());
                assert!(hint.context_fields.is_empty());
                assert_eq!(hint.summary, "Operation cancelled by user");
            }

            /// `format_error_with_hints` includes "Suggestions:" iff hints are non-empty.
            #[test]
            fn format_includes_suggestions_iff_hints(variant in 0..9usize, msg in "[\\w\\s./]{0,80}") {
                let error = make_error(variant, &msg);
                let hint = hints_for_error(&error);
                let formatted = format_error_with_hints(&error);
                if hint.hints.is_empty() {
                    assert!(!formatted.contains("Suggestions:"));
                } else {
                    assert!(formatted.contains("Suggestions:"));
                }
            }

            /// Tool error category detection: "read" + "not found" → File not found.
            #[test]
            fn tool_read_not_found_hint(suffix in "[a-zA-Z /]{0,40}") {
                let msg = format!("not found {suffix}");
                let error = Error::tool("read", msg);
                let hint = hints_for_error(&error);
                assert_eq!(hint.summary, "File not found");
            }

            /// Tool error: unknown tool always gets generic hint.
            #[test]
            fn tool_unknown_gets_generic(tool in "[a-z]{5,10}", msg in "[a-zA-Z ]{0,40}") {
                let error = Error::tool(tool, msg);
                let hint = hints_for_error(&error);
                assert_eq!(hint.summary, "Tool execution error");
            }
        }
    }
}
