mod common;

use common::{TestHarness, run_async};
use pi::auth::{AuthCredential, AuthStorage};
use pi::http::client::Client;
use pi::vcr::{VcrMode, VcrRecorder};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn cassette_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vcr")
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("read json");
    serde_json::from_str(&text).expect("parse json")
}

fn redact_auth_json(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let mut redacted = 0usize;
            for (key, child) in map.iter_mut() {
                if matches!(key.as_str(), "access_token" | "refresh_token" | "key") {
                    if !child.is_null() {
                        *child = Value::String("[REDACTED]".to_string());
                        redacted = redacted.saturating_add(1);
                    }
                } else {
                    redacted = redacted.saturating_add(redact_auth_json(child));
                }
            }
            redacted
        }
        Value::Array(items) => items.iter_mut().map(redact_auth_json).sum(),
        _ => 0,
    }
}

fn write_redacted_snapshot(harness: &TestHarness, src: &Path, name: &str) -> (PathBuf, usize) {
    let mut value = read_json(src);
    let redacted = redact_auth_json(&mut value);
    let path = harness.temp_path(name);
    let text = serde_json::to_string_pretty(&value).expect("serialize redacted auth json");
    std::fs::write(&path, text).expect("write redacted auth json");
    harness.record_artifact(name, &path);
    (path, redacted)
}

fn oauth_entry<'a>(value: &'a Value, provider: &str) -> &'a serde_json::Map<String, Value> {
    let Some(map) = value.as_object() else {
        panic!("expected auth.json root object");
    };
    let Some(Value::Object(entry)) = map.get(provider) else {
        panic!("expected provider entry for {provider}");
    };
    entry
}

fn oauth_field<'a>(entry: &'a serde_json::Map<String, Value>, key: &str) -> &'a str {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected string field {key}"))
}

async fn run_refresh_scenario(
    harness: &TestHarness,
    cassette_name: &str,
    refresh_token: &str,
    expected_new_access: Option<&str>,
    expected_new_refresh: Option<&str>,
    expected_error_fragment: Option<&str>,
) {
    let cassette_dir = cassette_root();
    let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
    harness.record_artifact(format!("{cassette_name}.json"), &cassette_path);
    assert!(
        cassette_path.exists(),
        "missing cassette {}",
        cassette_path.display()
    );

    let auth_path = harness.temp_path("auth.json");
    let old_access = format!("old-access-{cassette_name}");
    let old_refresh = refresh_token.to_string();

    let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth storage");
    auth.set(
        "anthropic",
        AuthCredential::OAuth {
            access_token: old_access.clone(),
            refresh_token: old_refresh.clone(),
            expires: 0,
        },
    );
    auth.save().expect("save auth.json");

    let (_, before_redactions) = write_redacted_snapshot(harness, &auth_path, "auth.before.json");

    harness
        .log()
        .info_ctx("vcr", "Running OAuth refresh scenario", |ctx| {
            ctx.push(("cassette".into(), cassette_name.to_string()));
            ctx.push(("auth_path".into(), auth_path.display().to_string()));
        });

    let recorder = VcrRecorder::new_with(cassette_name, VcrMode::Playback, &cassette_dir);
    let client = Client::new().with_vcr(recorder);

    let result = auth.refresh_expired_oauth_tokens_with_client(&client).await;

    let after_json = read_json(&auth_path);
    let entry = oauth_entry(&after_json, "anthropic");
    let after_access = oauth_field(entry, "access_token");
    let after_refresh = oauth_field(entry, "refresh_token");
    let after_expires = entry
        .get("expires")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("expected i64 field expires"));

    let now = chrono::Utc::now().timestamp_millis();

    match (
        expected_new_access,
        expected_new_refresh,
        expected_error_fragment,
    ) {
        (Some(expected_access), Some(expected_refresh), None) => {
            result.expect("expected refresh to succeed");
            assert_eq!(after_access, expected_access);
            assert_eq!(after_refresh, expected_refresh);
            assert!(
                after_expires > now,
                "expected expires > now, got expires={after_expires}, now={now}"
            );
        }
        (None, None, Some(fragment)) => {
            let err = result.expect_err("expected refresh to fail");
            let message = err.to_string();
            assert!(
                message.contains(fragment),
                "expected error to contain '{fragment}', got '{message}'"
            );

            // File should remain unchanged on refresh failure.
            assert_eq!(after_access, old_access);
            assert_eq!(after_refresh, old_refresh);

            let error_path = harness.temp_path("refresh.error.txt");
            std::fs::write(&error_path, &message).expect("write error artifact");
            harness.record_artifact("refresh.error.txt", &error_path);
        }
        _ => panic!("invalid test expectation"),
    }

    let (_, after_redactions) = write_redacted_snapshot(harness, &auth_path, "auth.after.json");

    harness
        .log()
        .info_ctx("summary", "OAuth refresh summary", |ctx| {
            ctx.push(("old_access_sha256".into(), sha256_hex(&old_access)));
            ctx.push(("new_access_sha256".into(), sha256_hex(after_access)));
            ctx.push(("before_redactions".into(), before_redactions.to_string()));
            ctx.push(("after_redactions".into(), after_redactions.to_string()));
        });

    // Emit deterministic JSONL logs + artifact index for this scenario.
    let logs_path = harness.temp_path("auth_refresh.log.jsonl");
    harness
        .write_jsonl_logs_normalized(&logs_path)
        .expect("write jsonl logs");
    harness.record_artifact("auth_refresh.log.jsonl", &logs_path);

    let artifacts_path = harness.temp_path("auth_refresh.artifacts.jsonl");
    harness
        .write_artifact_index_jsonl_normalized(&artifacts_path)
        .expect("write artifact index");
    harness.record_artifact("auth_refresh.artifacts.jsonl", &artifacts_path);
}

#[test]
fn auth_oauth_refresh_success_vcr() {
    let harness = TestHarness::new("auth_oauth_refresh_success_vcr");
    run_async(async move {
        run_refresh_scenario(
            &harness,
            "oauth_refresh_success",
            "refresh-success",
            Some("new-access-success"),
            Some("new-refresh-success"),
            None,
        )
        .await;
    });
}

#[test]
fn auth_oauth_refresh_expired_refresh_token_vcr() {
    let harness = TestHarness::new("auth_oauth_refresh_expired_refresh_token_vcr");
    run_async(async move {
        run_refresh_scenario(
            &harness,
            "oauth_refresh_expired",
            "refresh-expired",
            None,
            None,
            Some("refresh token expired"),
        )
        .await;
    });
}

#[test]
fn auth_oauth_refresh_invalid_refresh_token_vcr() {
    let harness = TestHarness::new("auth_oauth_refresh_invalid_refresh_token_vcr");
    run_async(async move {
        run_refresh_scenario(
            &harness,
            "oauth_refresh_invalid",
            "refresh-invalid",
            None,
            None,
            Some("invalid refresh token"),
        )
        .await;
    });
}

#[test]
fn auth_oauth_refresh_network_failure_vcr() {
    let harness = TestHarness::new("auth_oauth_refresh_network_failure_vcr");
    run_async(async move {
        run_refresh_scenario(
            &harness,
            "oauth_refresh_network_failure",
            "refresh-network",
            None,
            None,
            Some("Service unavailable"),
        )
        .await;
    });
}
