//! Preserve Connect error categories across the agent's text error boundary.
//!
//! Streaming Connect failures normally arrive over HTTP 200. The adapter must
//! keep the actual protocol code, not fabricate an HTTP status or let optional
//! message prose authorize retries. The ordinary recovery policy still owns
//! durability/auth/overflow refusals, budgets, backoff and output visibility.

use super::FailoverClass;

const CODES: &[&str] = &[
    "canceled",
    "unknown",
    "invalid_argument",
    "deadline_exceeded",
    "not_found",
    "already_exists",
    "permission_denied",
    "resource_exhausted",
    "failed_precondition",
    "aborted",
    "out_of_range",
    "unimplemented",
    "internal",
    "unavailable",
    "data_loss",
    "unauthenticated",
];
const PREFIX: &str = "PI_CONNECT_ERROR[";

/// Only a validated wire code can construct a diagnostic. Keep the spelling
/// from the protocol's closed vocabulary; unknown values fail at the adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectCode(&'static str);

impl ConnectCode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        CODES
            .iter()
            .copied()
            .find(|code| *code == value)
            .map(Self)
    }

    pub(crate) fn diagnostic(self, message: &str) -> String {
        if message.is_empty() {
            format!("{PREFIX}{}]: Cursor stream returned {}", self.0, self.0)
        } else {
            format!("{PREFIX}{}]: {message}", self.0)
        }
    }

    /// A conservative policy for continuing an interrupted provider request.
    /// `aborted` requires a higher-level transaction restart; `internal`,
    /// `unknown` and `data_loss` do not prove a retryable capacity failure.
    /// Never promote those to retries just because they map to unary HTTP 5xx.
    const fn recovery_class(self) -> Option<FailoverClass> {
        match self.0.as_bytes() {
            b"resource_exhausted" => Some(FailoverClass::Quota),
            b"unavailable" => Some(FailoverClass::Overload),
            b"deadline_exceeded" => Some(FailoverClass::Transient),
            _ => None,
        }
    }
}

fn strip_ascii_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

/// `None` means this is not a Connect diagnostic and normal classification
/// applies. `Some(None)` means a permanent or malformed Connect diagnostic:
/// its body must not fall through to transient substring or HTTP guessing.
/// Only a leading marker after known Error display wrappers counts. A marker
/// quoted in an HTTP body, request id or model output cannot replace its code.
pub(super) fn connect_recovery(error_text: &str) -> Option<Option<FailoverClass>> {
    let mut text = error_text.trim_start();
    // Display wrapping is bounded. No search for a marker buried in payload
    // text and no allocation proportional to an untrusted code or provider id.
    for _ in 0..=4 {
        if let Some(rest) = strip_ascii_prefix(text, PREFIX) {
            let end = rest
                .as_bytes()
                .iter()
                .take(32)
                .position(|byte| *byte == b']');
            return Some(end.and_then(|end| {
                if !rest[end..].starts_with("]: ") {
                    return None;
                }
                // Recovery callers already lowercase some diagnostics. Keep
                // that representation equivalent; wire parsing stays strict.
                CODES
                    .iter()
                    .copied()
                    .find(|code| code.eq_ignore_ascii_case(&rest[..end]))
                    .map(ConnectCode)
                    .and_then(ConnectCode::recovery_class)
            }));
        }
        if let Some(rest) = strip_ascii_prefix(text, "Provider error:") {
            let (provider, message) = rest.trim_start().split_once(": ")?;
            if provider.is_empty() || provider.contains(['\n', '\r']) {
                return None;
            }
            text = message.trim_start();
        } else if let Some(rest) = ["API error:", "IO error:", "SSE error:", "Error:"]
            .iter()
            .find_map(|prefix| strip_ascii_prefix(text, prefix))
        {
            text = rest.trim_start();
        } else {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::failover::{
        RetryPolicy, TerminalReason, TurnDecision, TurnOutcome, TurnProgress,
        call_error_is_retryable, classify_failover, decide, error_result_is_retryable,
    };
    use crate::model::{AssistantMessage, StopReason};

    fn message(text: &str) -> AssistantMessage {
        AssistantMessage {
            error_message: Some(text.to_string()),
            stop_reason: StopReason::Error,
            ..AssistantMessage::default()
        }
    }

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_retries: 2,
            max_failovers_per_turn: 1,
            base_delay_ms: 500,
            max_delay_ms: 2_000,
        }
    }

    fn progress(retry_count: u32, failovers_this_turn: u32) -> TurnProgress {
        TurnProgress {
            retry_count,
            failovers_this_turn,
            stream_can_retry: true,
        }
    }

    fn diagnostic(code: &str, message: &str) -> String {
        ConnectCode::parse(code).unwrap().diagnostic(message)
    }

    #[test]
    fn wire_code_vocabulary_is_closed_and_case_sensitive() {
        assert_eq!(CODES.len(), 16);
        for (index, code) in CODES.iter().enumerate() {
            assert!(!CODES[..index].contains(code));
            assert!(ConnectCode::parse(code).is_some());
        }
        for invalid in [
            "",
            "UNAVAILABLE",
            " unavailable",
            "unavailable ",
            "cancelled",
            "503",
        ] {
            assert!(ConnectCode::parse(invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn every_code_survives_empty_and_opaque_message_diagnostics() {
        for code in CODES {
            for detail in [
                "",
                "opaque explanation 🦀",
                "503 bytes, not an HTTP response",
            ] {
                let value = ConnectCode::parse(code).unwrap();
                let text = value.diagnostic(detail);
                assert!(text.starts_with(&format!("PI_CONNECT_ERROR[{code}]: ")));
                assert_eq!(connect_recovery(&text), Some(value.recovery_class()));
                if !detail.is_empty() {
                    assert!(text.ends_with(detail));
                }
            }
        }
    }

    #[test]
    fn known_display_wrappers_preserve_codes_without_parsing_body_markers() {
        let text = diagnostic("unavailable", "opaque");
        for wrapped in [
            text.clone(),
            Error::api(&text).to_string(),
            Error::provider("cursor", &text).to_string(),
            Error::api(Error::provider("cursor", &text).to_string()).to_string(),
            format!("IO error: {text}"),
            format!("SSE error: {text}"),
            format!("  api error: {text}"),
            text.to_ascii_uppercase(),
        ] {
            assert_eq!(
                connect_recovery(&wrapped),
                Some(Some(FailoverClass::Overload))
            );
        }
        for body in [
            format!("request failed; {text}"),
            format!("HTTP 400: {text}"),
            format!("request_id={text}"),
            format!("\"{text}\""),
        ] {
            assert_eq!(connect_recovery(&body), None, "{body}");
        }
        let permanent = diagnostic("invalid_argument", &text);
        assert_eq!(connect_recovery(&permanent), Some(None));
    }

    #[test]
    fn malformed_leading_markers_cannot_fall_back_to_retry_prose() {
        for text in [
            "PI_CONNECT_ERROR[unavailable]:x timeout",
            "PI_CONNECT_ERROR[not_a_code]: quota exceeded",
            "PI_CONNECT_ERROR[unavailable timeout",
            "PI_CONNECT_ERROR[]: HTTP 503",
            "PI_CONNECT_ERROR[🦀]: timeout",
        ] {
            assert_eq!(connect_recovery(text), Some(None));
            assert_eq!(classify_failover(text), None);
            assert!(!call_error_is_retryable(&Error::api(text)));
            assert!(!error_result_is_retryable(&message(text), None));
        }
        let long = format!("PI_CONNECT_ERROR[{}]: timeout", "x".repeat(100));
        assert_eq!(connect_recovery(&long), Some(None));
    }

    #[test]
    fn recoverable_codes_use_shared_backoff_then_failover_and_stop_at_the_budget() {
        for (code, class) in [
            ("resource_exhausted", FailoverClass::Quota),
            ("unavailable", FailoverClass::Overload),
            ("deadline_exceeded", FailoverClass::Transient),
        ] {
            for detail in ["", "opaque", "body quotes HTTP 400 and request_id=503"] {
                let text = diagnostic(code, detail);
                assert_eq!(classify_failover(&text), Some(class), "{text}");
                let message = message(&text);
                assert!(error_result_is_retryable(&message, None));
                for error in [Error::api(&text), Error::provider("cursor", &text)] {
                    assert!(call_error_is_retryable(&error), "{error}");
                    for outcome in [
                        TurnOutcome::Completed(&message),
                        TurnOutcome::Failed(&error),
                    ] {
                        for (state, expected) in [
                            (
                                progress(0, 0),
                                TurnDecision::Retry {
                                    attempt: 1,
                                    delay_ms: 500,
                                },
                            ),
                            (
                                progress(1, 0),
                                TurnDecision::Retry {
                                    attempt: 2,
                                    delay_ms: 1_000,
                                },
                            ),
                            (progress(2, 0), TurnDecision::FailOver),
                            (progress(2, 1), TurnDecision::Finish { success: false }),
                        ] {
                            assert_eq!(
                                decide(outcome, &state, &policy(), None),
                                expected,
                                "{text}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn permanent_codes_do_not_retry_or_fail_over_from_transient_body_words() {
        for code in CODES {
            if matches!(
                *code,
                "resource_exhausted" | "unavailable" | "deadline_exceeded"
            ) {
                continue;
            }
            for detail in [
                "opaque",
                "HTTP 503: timeout; quota exceeded; service unavailable",
            ] {
                let text = diagnostic(code, detail);
                let message = message(&text);
                assert_eq!(classify_failover(&text), None, "{text}");
                assert!(!error_result_is_retryable(&message, None), "{text}");
                for error in [Error::api(&text), Error::provider("cursor", &text)] {
                    assert!(!call_error_is_retryable(&error), "{error}");
                    for outcome in [
                        TurnOutcome::Completed(&message),
                        TurnOutcome::Failed(&error),
                    ] {
                        for state in [progress(0, 0), progress(2, 0)] {
                            assert_eq!(
                                decide(outcome, &state, &policy(), None),
                                TurnDecision::Finish { success: false },
                                "{text}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn auth_overflow_and_persistence_refusals_still_outrank_recoverable_codes() {
        for code in ["resource_exhausted", "unavailable", "deadline_exceeded"] {
            for detail in [
                "invalid_api_key",
                "permission_denied",
                "HTTP 401: retry delay",
                "prompt is too long",
                "context_length_exceeded",
            ] {
                let text = diagnostic(code, detail);
                let message = message(&text);
                let error = Error::api(&text);
                assert_eq!(classify_failover(&text), None);
                assert!(!error_result_is_retryable(&message, None));
                assert!(!call_error_is_retryable(&error));
                for outcome in [
                    TurnOutcome::Completed(&message),
                    TurnOutcome::Failed(&error),
                ] {
                    assert_eq!(
                        decide(outcome, &progress(0, 0), &policy(), None),
                        TurnDecision::Finish { success: false }
                    );
                }
            }
            let text = diagnostic(
                code,
                &Error::session_persistence("save failed").to_string(),
            );
            let message = message(&text);
            let error = Error::provider("cursor", &text);
            for outcome in [
                TurnOutcome::Completed(&message),
                TurnOutcome::Failed(&error),
            ] {
                assert_eq!(
                    decide(outcome, &progress(0, 0), &policy(), None),
                    TurnDecision::Terminal(TerminalReason::SessionPersistence)
                );
            }
        }
    }

    #[test]
    fn visible_output_and_token_evidence_still_block_reentry() {
        let text = diagnostic("unavailable", "opaque");
        let mut message = message(&text);
        let error = Error::api(&text);
        let mut visible = progress(0, 0);
        visible.stream_can_retry = false;
        for outcome in [
            TurnOutcome::Completed(&message),
            TurnOutcome::Failed(&error),
        ] {
            assert_eq!(
                decide(outcome, &visible, &policy(), None),
                TurnDecision::Finish { success: false }
            );
        }
        message.usage.input = 8_193;
        assert_eq!(
            decide(
                TurnOutcome::Completed(&message),
                &progress(0, 0),
                &policy(),
                Some(8_192),
            ),
            TurnDecision::Finish { success: false }
        );
        message.stop_reason = StopReason::Aborted;
        assert_eq!(
            decide(
                TurnOutcome::Completed(&message),
                &progress(0, 0),
                &policy(),
                None,
            ),
            TurnDecision::Terminal(TerminalReason::Aborted)
        );
    }

    #[test]
    fn typed_transient_io_does_not_override_a_permanent_connect_diagnostic() {
        let text = diagnostic("invalid_argument", "timeout");
        let error = Error::Io(Box::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            text,
        )));
        assert!(error.is_transient());
        assert!(!call_error_is_retryable(&error));
        assert_eq!(
            decide(
                TurnOutcome::Failed(&error),
                &progress(0, 0),
                &policy(),
                None,
            ),
            TurnDecision::Finish { success: false }
        );
    }

    #[test]
    fn other_provider_http_statuses_are_not_replaced_by_quoted_connect_markers() {
        let permanent = diagnostic("invalid_argument", "opaque");
        let transient = diagnostic("unavailable", "opaque");
        assert_eq!(
            classify_failover(&format!("HTTP 503: {permanent}")),
            Some(FailoverClass::Overload)
        );
        assert_eq!(classify_failover(&format!("HTTP 400: {transient}")), None);
    }
}
