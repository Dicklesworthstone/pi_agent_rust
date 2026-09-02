//! `current_time` tool (gh #207): model-facing wall-clock awareness.
//!
//! Models have no reliable notion of "now"; they guess from training data or
//! from timestamps that happen to appear in context. This tool returns the
//! host's current time in the forms a coding agent actually needs (UTC and
//! local ISO-8601, offset, Unix epoch, weekday) so date arithmetic,
//! "what changed today", changelog headings, and deadline reasoning start from
//! a real clock instead of a hallucinated one.
//!
//! The tool is essential-tier (tiny schema, no parameters) and declares read
//! effects only: it touches nothing but the system clock.

use crate::error::Result;
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use chrono::{DateTime, FixedOffset, Local, SecondsFormat, Utc};

/// Wall-clock reading rendered for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeSnapshot {
    /// RFC 3339 UTC timestamp with second precision and a `Z` suffix.
    pub utc: String,
    /// RFC 3339 local timestamp including the numeric offset.
    pub local: String,
    /// Local UTC offset in seconds east of Greenwich.
    pub offset_seconds: i32,
    /// Local UTC offset rendered as `+HH:MM` / `-HH:MM`.
    pub offset: String,
    /// Seconds since the Unix epoch.
    pub unix_seconds: i64,
    /// Milliseconds since the Unix epoch.
    pub unix_millis: i64,
    /// Full English weekday name in local time (`Monday` .. `Sunday`).
    pub weekday: String,
    /// ISO 8601 week number in local time.
    pub iso_week: u32,
    /// Timezone name from the `TZ` environment variable when it is set.
    pub timezone: Option<String>,
}

impl TimeSnapshot {
    /// Build a snapshot from explicit instants. Pure, so tests can pin the
    /// clock; [`TimeSnapshot::now`] is the production entry point.
    #[must_use]
    pub fn from_instants(
        utc: DateTime<Utc>,
        local: DateTime<FixedOffset>,
        timezone: Option<String>,
    ) -> Self {
        let offset_seconds = local.offset().local_minus_utc();
        let sign = if offset_seconds < 0 { '-' } else { '+' };
        let abs = offset_seconds.unsigned_abs();
        Self {
            utc: utc.to_rfc3339_opts(SecondsFormat::Secs, true),
            local: local.to_rfc3339_opts(SecondsFormat::Secs, false),
            offset_seconds,
            offset: format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60),
            unix_seconds: utc.timestamp(),
            unix_millis: utc.timestamp_millis(),
            weekday: local.format("%A").to_string(),
            iso_week: local.format("%V").to_string().parse().unwrap_or(0),
            timezone,
        }
    }

    /// Read the system clock.
    #[must_use]
    pub fn now() -> Self {
        let utc = Utc::now();
        let local: DateTime<FixedOffset> = Local::now().fixed_offset();
        let timezone = std::env::var("TZ")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Self::from_instants(utc, local, timezone)
    }

    /// The text block handed to the model.
    #[must_use]
    pub fn render_text(&self) -> String {
        let timezone = self
            .timezone
            .as_deref()
            .map_or_else(String::new, |tz| format!(" [{tz}]"));
        format!(
            "UTC: {}\nLocal: {} (UTC{}){timezone}\nUnix: {}\nWeekday: {}\nISO week: {}",
            self.utc, self.local, self.offset, self.unix_seconds, self.weekday, self.iso_week
        )
    }

    /// Structured `details` payload persisted with the tool result.
    #[must_use]
    pub fn details(&self) -> serde_json::Value {
        serde_json::json!({
            "utc": self.utc,
            "local": self.local,
            "offset": self.offset,
            "offsetSeconds": self.offset_seconds,
            "unixSeconds": self.unix_seconds,
            "unixMillis": self.unix_millis,
            "weekday": self.weekday,
            "isoWeek": self.iso_week,
            "timezone": self.timezone,
        })
    }
}

/// The `current_time` built-in tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct CurrentTimeTool;

impl CurrentTimeTool {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for CurrentTimeTool {
    fn name(&self) -> &str {
        "current_time"
    }

    fn label(&self) -> &str {
        "current_time"
    }

    fn description(&self) -> &str {
        "Return the host's current wall-clock time: UTC and local ISO-8601 \
         timestamps, UTC offset, Unix epoch seconds, weekday, and ISO week. \
         Call this before reasoning about today's date, deadlines, \"recent\" \
         changes, or timestamped filenames. Takes no arguments."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn effects(&self) -> ToolEffects {
        // Reads the system clock only; never touches the workspace.
        ToolEffects::read()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let snapshot = TimeSnapshot::now();
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(snapshot.render_text()))],
            details: Some(snapshot.details()),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn pinned() -> TimeSnapshot {
        // 2026-09-01T23:15:42Z rendered in UTC-4 (a Tuesday, ISO week 36).
        let utc = Utc.with_ymd_and_hms(2026, 9, 1, 23, 15, 42).unwrap(); // ubs:ignore test fixture
        let offset = FixedOffset::west_opt(4 * 3600).unwrap(); // ubs:ignore test fixture
        TimeSnapshot::from_instants(
            utc,
            utc.with_timezone(&offset),
            Some("America/New_York".to_string()),
        )
    }

    #[test]
    fn snapshot_renders_every_field_from_a_pinned_clock() {
        let snap = pinned();
        assert_eq!(snap.utc, "2026-09-01T23:15:42Z");
        assert_eq!(snap.local, "2026-09-01T19:15:42-04:00");
        assert_eq!(snap.offset, "-04:00");
        assert_eq!(snap.offset_seconds, -4 * 3600);
        assert_eq!(snap.unix_seconds, 1_788_304_542);
        assert_eq!(snap.unix_millis, 1_788_304_542_000);
        assert_eq!(snap.weekday, "Tuesday");
        assert_eq!(snap.iso_week, 36);
        assert_eq!(
            snap.render_text(),
            "UTC: 2026-09-01T23:15:42Z\n\
             Local: 2026-09-01T19:15:42-04:00 (UTC-04:00) [America/New_York]\n\
             Unix: 1788304542\n\
             Weekday: Tuesday\n\
             ISO week: 36"
        );
    }

    #[test]
    fn positive_offsets_and_missing_timezone_render_cleanly() {
        let utc = Utc.with_ymd_and_hms(2027, 1, 3, 0, 30, 0).unwrap(); // ubs:ignore test fixture
        let offset = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap(); // ubs:ignore test fixture
        let snap = TimeSnapshot::from_instants(utc, utc.with_timezone(&offset), None);
        assert_eq!(snap.local, "2027-01-03T06:00:00+05:30");
        assert_eq!(snap.offset, "+05:30");
        // Local date is Sunday 3 Jan 2027, which ISO-8601 assigns to week 53 of 2026.
        assert_eq!(snap.weekday, "Sunday");
        assert_eq!(snap.iso_week, 53);
        assert!(!snap.render_text().contains('['), "{}", snap.render_text());
        assert_eq!(snap.details()["timezone"], serde_json::Value::Null);
    }

    #[test]
    fn details_payload_uses_stable_camel_case_keys() {
        let details = pinned().details();
        for key in [
            "utc",
            "local",
            "offset",
            "offsetSeconds",
            "unixSeconds",
            "unixMillis",
            "weekday",
            "isoWeek",
            "timezone",
        ] {
            assert!(details.get(key).is_some(), "missing details key {key}");
        }
        assert_eq!(details["timezone"], "America/New_York");
    }

    #[test]
    fn tool_contract_is_tiny_and_read_only() {
        let tool = CurrentTimeTool::new();
        assert_eq!(tool.name(), "current_time");
        assert_eq!(tool.label(), "current_time");
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"].as_object().unwrap().is_empty()); // ubs:ignore test assertion
        assert!(params.get("required").is_none());
        assert!(tool.effects().reads());
        assert!(!tool.effects().writes());
    }

    #[test]
    fn execute_reads_the_live_clock_and_stays_consistent_with_itself() {
        asupersync::test_utils::run_test(|| async {
            let tool = CurrentTimeTool::new();
            let before = Utc::now().timestamp();
            let output = tool
                .execute("call-1", serde_json::json!({}), None)
                .await
                .expect("current_time never fails"); // ubs:ignore test assertion
            let after = Utc::now().timestamp();
            assert!(!output.is_error);
            let details = output.details.expect("details present"); // ubs:ignore test assertion
            let unix = details["unixSeconds"].as_i64().expect("unixSeconds"); // ubs:ignore test assertion
            assert!(
                (before..=after).contains(&unix),
                "unixSeconds {unix} outside [{before}, {after}]"
            );
            let text = match output.content.first() {
                Some(ContentBlock::Text(text)) => text.text.clone(),
                other => panic!("unexpected content block: {other:?}"), // ubs:ignore test assertion
            };
            let utc = details["utc"].as_str().expect("utc"); // ubs:ignore test assertion
            assert!(text.starts_with(&format!("UTC: {utc}\n")), "{text}");
            assert!(text.contains("\nWeekday: "), "{text}");
            let parsed = DateTime::parse_from_rfc3339(utc).expect("utc parses"); // ubs:ignore test assertion
            assert_eq!(parsed.timestamp(), unix);
        });
    }
}
