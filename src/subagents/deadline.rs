//! One execution budget shared by a delegation's queue, children and retries.
//!
//! Local enforcement uses a monotonic clock. An absolute wall-clock ceiling is
//! forwarded only to native descendants, which translate it to their own
//! monotonic clock. This is lifecycle control, not a sandbox against a child
//! that deliberately changes its environment or escapes process ownership.

use crate::error::{Error, Result};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_mins(15);
pub(super) const MAX_TIMEOUT_SECS: u64 = 86_400;
const TIMEOUT_ENV: &str = "PI_SUBAGENT_TIMEOUT_SECS";
const DEADLINE_ENV: &str = "PI_SUBAGENT_DEADLINE_UNIX_MS";
pub(super) const TIMED_OUT: &str =
    "PI_SUBAGENT_TIMEOUT: delegation execution budget expired; unfinished work was not accepted";

#[derive(Clone, Copy, Debug)]
pub(super) struct Deadline {
    at: Instant,
    expires_unix_ms: u64,
}

fn invalid_timeout() -> Error {
    Error::tool(
        "subagent",
        "PI_SUBAGENT_INVALID_TIMEOUT: timeoutSeconds and PI_SUBAGENT_TIMEOUT_SECS must be integers from 1 to 86400; an SDK timeout must be between 1 ms and 24 hours",
    )
}

fn inherited_error() -> Error {
    Error::tool(
        "subagent",
        "PI_SUBAGENT_INVALID_DEADLINE: inherited deadline must be a positive Unix timestamp in milliseconds",
    )
}

fn parse_seconds(value: &str) -> Result<Duration> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_timeout());
    }
    seconds(value.parse::<u64>().map_err(|_| invalid_timeout())?)
}

fn seconds(value: u64) -> Result<Duration> {
    if !(1..=MAX_TIMEOUT_SECS).contains(&value) {
        return Err(invalid_timeout());
    }
    Ok(Duration::from_secs(value))
}

impl Deadline {
    pub(super) fn earlier(self, other: Self) -> Self {
        if self.at <= other.at { self } else { other }
    }

    /// Explicit SDK policy beats the environment policy. A model request may
    /// shorten that host policy, never lengthen it; inheritance is always a cap.
    pub(super) fn for_request(
        host_timeout: Option<Duration>,
        requested_seconds: Option<u64>,
    ) -> Result<Self> {
        let host = match host_timeout {
            Some(timeout) => timeout,
            None => match std::env::var(TIMEOUT_ENV) {
                Ok(raw) => parse_seconds(&raw)?,
                Err(std::env::VarError::NotPresent) => DEFAULT_TIMEOUT,
                Err(std::env::VarError::NotUnicode(_)) => return Err(invalid_timeout()),
            },
        };
        let inherited = match std::env::var(DEADLINE_ENV) {
            Ok(raw) => {
                if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(inherited_error());
                }
                Some(raw.parse::<u64>().map_err(|_| inherited_error())?)
            }
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => return Err(inherited_error()),
        };
        Self::at(
            host,
            requested_seconds,
            inherited,
            Instant::now(),
            SystemTime::now(),
        )
    }

    fn at(
        host: Duration,
        requested_seconds: Option<u64>,
        inherited_ms: Option<u64>,
        now: Instant,
        wall: SystemTime,
    ) -> Result<Self> {
        if host < Duration::from_millis(1) || host > Duration::from_secs(MAX_TIMEOUT_SECS) {
            return Err(invalid_timeout());
        }
        let limit = requested_seconds
            .map(seconds)
            .transpose()?
            .map_or(host, |request| host.min(request));
        let wall_ms = u64::try_from(
            wall.duration_since(UNIX_EPOCH)
                .map_err(|_| inherited_error())?
                .as_millis(),
        )
        .map_err(|_| inherited_error())?;
        let limit_ms = u64::try_from(limit.as_millis()).map_err(|_| invalid_timeout())?;
        let mut expires_unix_ms = wall_ms.checked_add(limit_ms).ok_or_else(inherited_error)?;
        if let Some(inherited) = inherited_ms {
            if inherited == 0 {
                return Err(inherited_error());
            }
            expires_unix_ms = expires_unix_ms.min(inherited);
        }
        let remaining = Duration::from_millis(expires_unix_ms.saturating_sub(wall_ms));
        let at = now.checked_add(remaining).ok_or_else(inherited_error)?;
        Ok(Self {
            at,
            expires_unix_ms,
        })
    }

    pub(super) fn check(self) -> std::result::Result<(), &'static str> {
        self.check_at(Instant::now())
    }

    fn check_at(self, now: Instant) -> std::result::Result<(), &'static str> {
        if now >= self.at {
            Err(TIMED_OUT)
        } else {
            Ok(())
        }
    }

    pub(super) fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// The child gets a cooperative per-turn limit and an absolute ceiling for
    /// its own delegations. The parent's precise deadline remains authoritative.
    /// Call before adding the task arguments, so flags cannot become prompt text.
    pub(super) fn configure_child(
        self,
        command: &mut Command,
    ) -> std::result::Result<(), &'static str> {
        self.check()?;
        let remaining = self.remaining();
        if remaining.is_zero() {
            return Err(TIMED_OUT);
        }
        let whole_seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
        command
            .arg("--max-time")
            .arg(whole_seconds.to_string())
            .env(DEADLINE_ENV, self.expires_unix_ms.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000)
    }

    #[test]
    fn request_can_shorten_but_never_extend_host_policy() {
        let now = Instant::now();
        for (request, expected) in [(None, 10), (Some(2), 2), (Some(60), 10)] {
            let deadline =
                Deadline::at(Duration::from_secs(10), request, None, now, wall()).unwrap();
            assert_eq!(
                deadline.at.duration_since(now),
                Duration::from_secs(expected)
            );
        }
    }

    #[test]
    fn inherited_ceiling_caps_every_new_local_budget() {
        let now = Instant::now();
        let deadline =
            Deadline::at(DEFAULT_TIMEOUT, Some(300), Some(1_003_000), now, wall()).unwrap();
        assert_eq!(deadline.at.duration_since(now), Duration::from_secs(3));
        assert_eq!(deadline.expires_unix_ms, 1_003_000);
        let later = Deadline::at(
            DEFAULT_TIMEOUT,
            None,
            Some(deadline.expires_unix_ms),
            now + Duration::from_secs(2),
            wall() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(
            later.at, deadline.at,
            "nested work must not restart the budget"
        );
    }

    #[test]
    fn expired_inheritance_stays_expired() {
        let now = Instant::now();
        let deadline = Deadline::at(DEFAULT_TIMEOUT, None, Some(999_999), now, wall()).unwrap();
        assert_eq!(deadline.check_at(now), Err(TIMED_OUT));
        assert_eq!(deadline.at, now);
    }

    #[test]
    fn copying_for_queue_and_retry_does_not_reset_expiry() {
        let now = Instant::now();
        let original = Deadline::at(Duration::from_secs(5), None, None, now, wall()).unwrap();
        let queued = original;
        let retry = queued;
        assert!(retry.check_at(now + Duration::from_secs(4)).is_ok());
        assert_eq!(
            queued.check_at(now + Duration::from_secs(5)),
            Err(TIMED_OUT)
        );
        assert_eq!(retry.at, original.at);
    }

    #[test]
    fn invalid_limits_are_rejected_without_echoing_input() {
        for raw in [
            "",
            "0",
            "-1",
            "1.5",
            " 30",
            "30 ",
            "86401",
            "999999999999999999999",
            "credential-secret",
        ] {
            let error = parse_seconds(raw).unwrap_err().to_string();
            assert!(error.contains("PI_SUBAGENT_INVALID_TIMEOUT"));
            assert!(!error.contains("credential-secret"));
        }
        for raw in ["1", "900", "86400"] {
            assert!(parse_seconds(raw).is_ok());
        }
        for limit in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_secs(86_401),
        ] {
            assert!(Deadline::at(limit, None, None, Instant::now(), wall()).is_err());
        }
        assert!(Deadline::at(DEFAULT_TIMEOUT, Some(0), None, Instant::now(), wall()).is_err());
    }

    #[test]
    fn sdk_subsecond_budgets_keep_millisecond_precision() {
        let now = Instant::now();
        let deadline = Deadline::at(Duration::from_millis(25), None, None, now, wall()).unwrap();
        assert_eq!(deadline.at.duration_since(now), Duration::from_millis(25));
        assert_eq!(deadline.expires_unix_ms, 1_000_025);
    }

    #[test]
    fn child_configuration_replaces_ambient_deadline_and_never_uses_zero_seconds() {
        let deadline = Deadline::at(
            Duration::from_secs(2),
            None,
            None,
            Instant::now(),
            SystemTime::now(),
        )
        .unwrap();
        let mut command = Command::new("pi");
        command.env(DEADLINE_ENV, "1");
        deadline.configure_child(&mut command).unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert_eq!(args[0], "--max-time");
        assert!((1..=2).contains(&args[1].parse::<u64>().unwrap()));
        let inherited = command
            .get_envs()
            .find(|(key, _)| *key == DEADLINE_ENV)
            .unwrap()
            .1
            .unwrap();
        assert_eq!(
            inherited.to_str().unwrap().parse::<u64>().unwrap(),
            deadline.expires_unix_ms
        );
    }

    #[test]
    fn invalid_epoch_and_unrepresentable_clock_fail_closed() {
        let now = Instant::now();
        assert!(Deadline::at(DEFAULT_TIMEOUT, None, Some(0), now, wall()).is_err());
        assert!(
            Deadline::at(
                DEFAULT_TIMEOUT,
                None,
                None,
                now,
                UNIX_EPOCH - Duration::from_secs(1)
            )
            .is_err()
        );
    }
}
