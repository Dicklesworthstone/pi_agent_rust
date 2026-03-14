mod common;

use common::{TestHarness, run_async};
use pi::auth::{AuthCredential, AuthStorage};
use pi::http::client::Client;
use pi::vcr::{VcrMode, VcrRecorder};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

fn oauth_access_token(path: &Path, provider: &str) -> Option<String> {
    let auth = AuthStorage::load(path.to_path_buf()).ok()?;
    let cred = auth.get(provider)?;
    match cred {
        AuthCredential::OAuth { access_token, .. } => Some(access_token.clone()),
        _ => None,
    }
}

fn log_refresh_event(harness: &TestHarness, test: &str, event: &str, data: &[(&str, String)]) {
    let mut data_object = serde_json::Map::new();
    for (key, value) in data {
        data_object.insert((*key).to_string(), Value::String(value.clone()));
    }
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let entry = serde_json::json!({
        "schema": "pi.test.auth_event.v1",
        "test": test,
        "event": event,
        "timestamp_ms": timestamp_ms,
        "data": Value::Object(data_object.clone()),
    });
    eprintln!(
        "JSONL: {}",
        serde_json::to_string(&entry).expect("serialize refresh event")
    );

    harness
        .log()
        .info_ctx("refresh_event", "OAuth refresh event", |ctx| {
            ctx.push(("test".into(), test.to_string()));
            ctx.push(("event".into(), event.to_string()));
            for (key, value) in &data_object {
                ctx.push((key.clone(), value.as_str().unwrap_or_default().to_string()));
            }
        });
}

#[allow(clippy::too_many_lines)]
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
    let near_expiry = chrono::Utc::now().timestamp_millis() + 5 * 60 * 1000;

    let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth storage");
    auth.set(
        "anthropic",
        AuthCredential::OAuth {
            access_token: old_access.clone(),
            refresh_token: old_refresh.clone(),
            expires: near_expiry,
            token_url: None,
            client_id: None,
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
    log_refresh_event(
        harness,
        cassette_name,
        "token_about_to_expire",
        &[
            ("provider", "anthropic".to_string()),
            ("expires_in_seconds", "300".to_string()),
        ],
    );
    log_refresh_event(
        harness,
        cassette_name,
        "refresh_triggered",
        &[("provider", "anthropic".to_string())],
    );

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
            let new_expires_in = (after_expires - now).max(0) / 1000;
            log_refresh_event(
                harness,
                cassette_name,
                "refresh_completed",
                &[
                    ("provider", "anthropic".to_string()),
                    ("new_expires_in", new_expires_in.to_string()),
                ],
            );
        }
        (None, None, Some(fragment)) => {
            let err = result.expect_err("expected refresh to fail");
            let message = err.to_string();
            assert!(
                message.contains(fragment),
                "expected error to contain '{fragment}', got '{message}'"
            );
            let hints = err.hints();
            assert!(
                hints.hints.iter().any(|hint| hint.contains("/login")),
                "expected refresh failure hints to include /login guidance, got {:?}",
                hints.hints
            );

            // File should remain unchanged on refresh failure.
            assert_eq!(after_access, old_access);
            assert_eq!(after_refresh, old_refresh);
            log_refresh_event(
                harness,
                cassette_name,
                "refresh_failed",
                &[
                    ("provider", "anthropic".to_string()),
                    ("error_fragment", fragment.to_string()),
                ],
            );

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

#[test]
fn auth_oauth_refresh_api_key_credentials_skip_refresh_vcr() {
    let harness = TestHarness::new("auth_oauth_refresh_api_key_credentials_skip_refresh_vcr");
    run_async(async move {
        let cassette_dir = cassette_root();
        let cassette_path = cassette_dir.join("oauth_refresh_network_failure.json");
        harness.record_artifact("oauth_refresh_network_failure.json", &cassette_path);
        assert!(
            cassette_path.exists(),
            "missing cassette {}",
            cassette_path.display()
        );

        let auth_path = harness.temp_path("auth.json");
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth storage");
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: pi::auth::ApiKeyValue::Raw("google-api-key-test".to_string()),
            },
        );
        auth.save().expect("save auth.json");
        let (_, before_redactions) =
            write_redacted_snapshot(&harness, &auth_path, "auth.apikey.before.json");

        let recorder = VcrRecorder::new_with(
            "oauth_refresh_network_failure",
            VcrMode::Playback,
            &cassette_dir,
        );
        let client = Client::new().with_vcr(recorder);

        auth.refresh_expired_oauth_tokens_with_client(&client)
            .await
            .expect("API-key credentials should not trigger OAuth refresh");
        log_refresh_event(
            &harness,
            "auth_oauth_refresh_api_key_credentials_skip_refresh_vcr",
            "refresh_skipped_non_oauth_credential",
            &[
                ("provider", "google".to_string()),
                ("credential_type", "api_key".to_string()),
            ],
        );

        let after_json = read_json(&auth_path);
        let Some(root) = after_json.as_object() else {
            panic!("expected auth.json root object");
        };
        let google_entry = root.get("google").unwrap_or_else(|| {
            panic!("expected google API-key credential to remain in auth.json");
        });
        let google_obj = google_entry
            .as_object()
            .unwrap_or_else(|| panic!("expected google auth entry object"));
        assert_eq!(
            google_obj.get("type").and_then(Value::as_str),
            Some("api_key")
        );
        assert_eq!(
            google_obj.get("key").and_then(Value::as_str),
            Some("google-api-key-test")
        );

        let (_, after_redactions) =
            write_redacted_snapshot(&harness, &auth_path, "auth.apikey.after.json");

        harness
            .log()
            .info_ctx("summary", "API-key refresh-skip summary", |ctx| {
                ctx.push(("before_redactions".into(), before_redactions.to_string()));
                ctx.push(("after_redactions".into(), after_redactions.to_string()));
            });

        let logs_path = harness.temp_path("auth_refresh_apikey_skip.log.jsonl");
        harness
            .write_jsonl_logs_normalized(&logs_path)
            .expect("write jsonl logs");
        harness.record_artifact("auth_refresh_apikey_skip.log.jsonl", &logs_path);
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn auth_oauth_refresh_race_condition_vcr() {
    let harness = TestHarness::new("auth_oauth_refresh_race_condition_vcr");
    run_async(async move {
        let cassette_dir = cassette_root();
        let cassette_name = "oauth_refresh_success";
        let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
        harness.record_artifact(format!("{cassette_name}.json"), &cassette_path);
        assert!(
            cassette_path.exists(),
            "missing cassette {}",
            cassette_path.display()
        );

        let auth_path = harness.temp_path("auth.json");
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth storage");
        auth.set(
            "anthropic",
            AuthCredential::OAuth {
                access_token: "race-old-access".to_string(),
                refresh_token: "refresh-success".to_string(),
                expires: chrono::Utc::now().timestamp_millis() + 5 * 60 * 1000,
                token_url: None,
                client_id: None,
            },
        );
        auth.save().expect("seed auth.json");
        let (_, before_redactions) =
            write_redacted_snapshot(&harness, &auth_path, "auth.race.before.json");

        let shared_recorder =
            VcrRecorder::new_with(cassette_name, VcrMode::Playback, &cassette_dir);
        let first_client = Client::new().with_vcr(shared_recorder.clone());
        let second_client = Client::new().with_vcr(shared_recorder);

        log_refresh_event(
            &harness,
            "auth_oauth_refresh_race_condition_vcr",
            "token_about_to_expire",
            &[
                ("provider", "anthropic".to_string()),
                ("expires_in_seconds", "300".to_string()),
            ],
        );
        log_refresh_event(
            &harness,
            "auth_oauth_refresh_race_condition_vcr",
            "refresh_triggered",
            &[("provider", "anthropic".to_string())],
        );

        let first_auth_path = auth_path.clone();
        let first_refresh = async move {
            let mut first_auth = AuthStorage::load(first_auth_path).expect("load first auth");
            first_auth
                .refresh_expired_oauth_tokens_with_client(&first_client)
                .await
        };

        let second_auth_path = auth_path.clone();
        let second_refresh = async move {
            let mut seen_refreshed = false;
            // Use a generous iteration budget with yield_now() so the first future
            // actually gets scheduled on the single-threaded test runtime.
            // The previous 80-iteration / 10ms-sleep loop completed in ~160ms total
            // because asupersync timers fire early under enable_parking(false),
            // starving the first refresh future.
            for _ in 0..500 {
                if oauth_access_token(second_auth_path.as_path(), "anthropic").as_deref()
                    == Some("new-access-success")
                {
                    seen_refreshed = true;
                    break;
                }
                asupersync::runtime::yield_now().await;
                asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10))
                    .await;
            }
            assert!(
                seen_refreshed,
                "second refresh attempt never observed first refreshed token"
            );

            let mut second_auth = AuthStorage::load(second_auth_path).expect("load second auth");
            second_auth
                .refresh_expired_oauth_tokens_with_client(&second_client)
                .await
        };

        let (first_result, second_result) =
            futures::future::join(first_refresh, second_refresh).await;
        first_result.expect("first refresh should succeed");
        second_result.expect(
            "second refresh should skip network because token was refreshed by first attempt",
        );

        let after_json = read_json(&auth_path);
        let entry = oauth_entry(&after_json, "anthropic");
        let after_access = oauth_field(entry, "access_token");
        let after_refresh = oauth_field(entry, "refresh_token");
        let after_expires = entry
            .get("expires")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| panic!("expected i64 field expires"));
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(after_access, "new-access-success");
        assert_eq!(after_refresh, "new-refresh-success");
        assert!(
            after_expires > now,
            "expected expires > now, got expires={after_expires}, now={now}"
        );

        let (_, after_redactions) =
            write_redacted_snapshot(&harness, &auth_path, "auth.race.after.json");
        log_refresh_event(
            &harness,
            "auth_oauth_refresh_race_condition_vcr",
            "refresh_completed",
            &[
                ("provider", "anthropic".to_string()),
                (
                    "new_expires_in",
                    ((after_expires - now).max(0) / 1000).to_string(),
                ),
            ],
        );
        harness
            .log()
            .info_ctx("summary", "OAuth refresh race summary", |ctx| {
                ctx.push(("before_redactions".into(), before_redactions.to_string()));
                ctx.push(("after_redactions".into(), after_redactions.to_string()));
            });

        let logs_path = harness.temp_path("auth_refresh_race.log.jsonl");
        harness
            .write_jsonl_logs_normalized(&logs_path)
            .expect("write jsonl logs");
        harness.record_artifact("auth_refresh_race.log.jsonl", &logs_path);

        let artifacts_path = harness.temp_path("auth_refresh_race.artifacts.jsonl");
        harness
            .write_artifact_index_jsonl_normalized(&artifacts_path)
            .expect("write artifact index");
        harness.record_artifact("auth_refresh_race.artifacts.jsonl", &artifacts_path);
    });
}
