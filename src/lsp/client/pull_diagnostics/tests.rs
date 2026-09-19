use super::super::test_server::Fixture;
use super::*;
use std::path::{Path, PathBuf};

fn diagnostic(message: &str) -> Value {
    json!({
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
        "severity":1,"message":message,"source":"fixture"
    })
}

fn revision() -> Revision {
    Revision {
        version: 1,
        hash: 7,
    }
}

fn peer(root: &Path) -> Option<Fixture> {
    Fixture::connect(
        root,
        json!({
            "textDocumentSync":1,
            "diagnosticProvider":{"identifier":"fixture-diagnostics","interFileDependencies":false,"workspaceDiagnostics":false}
        }),
    )
}

fn source(peer: &Fixture, root: &Path, content: &str) -> (PathBuf, String) {
    let path = root.join("source.rs");
    std::fs::write(&path, content).unwrap();
    let path = path.canonicalize().unwrap();
    let uri = peer.client.ensure_synced(&path, "rust").unwrap();
    (path, uri)
}

fn refresh(peer: &Fixture, uri: &str) -> Result<()> {
    peer.runtime.block_on(
        peer.client
            .refresh_document_diagnostics(uri, Duration::from_secs(5)),
    )
}

#[test]
fn full_and_unchanged_reports_preserve_opaque_ids_and_require_a_baseline() {
    let full = parse_report(
        &json!({"kind":"full","resultId":"","items":[diagnostic("broken")]}),
        revision(),
        None,
    )
    .unwrap();
    assert_eq!(full.result_id.as_deref(), Some(""));
    let unchanged = parse_report(
        &json!({"kind":"unchanged","resultId":"next +/="}),
        revision(),
        Some(&full),
    )
    .unwrap();
    assert!(Arc::ptr_eq(&full.items, &unchanged.items));
    assert_eq!(unchanged.result_id.as_deref(), Some("next +/="));
    assert!(
        parse_report(
            &json!({"kind":"unchanged","resultId":"id"}),
            revision(),
            None
        )
        .is_err()
    );
    let other = Revision {
        version: 2,
        ..revision()
    };
    assert!(
        parse_report(
            &json!({"kind":"unchanged","resultId":"id"}),
            other,
            Some(&full)
        )
        .is_err()
    );
    let no_id = parse_report(&json!({"kind":"full","items":[]}), revision(), None).unwrap();
    assert!(
        parse_report(
            &json!({"kind":"unchanged","resultId":"id"}),
            revision(),
            Some(&no_id)
        )
        .is_err()
    );
}

#[test]
fn malformed_reports_do_not_become_clean_diagnostic_results() {
    for raw in [
        Value::Null,
        json!({}),
        json!({"kind":"other","items":[]}),
        json!({"kind":"full"}),
        json!({"kind":"full","items":null}),
        json!({"kind":"full","items":[{"message":"missing range"}]}),
        json!({"kind":"full","items":[{"range":{"start":{"line":2,"character":0},"end":{"line":0,"character":0}},"message":"inverted"}]}),
        json!({"kind":"full","items":[],"resultId":null}),
        json!({"kind":"unchanged"}),
        json!({"kind":"unchanged","resultId":"id","items":[]}),
    ] {
        assert!(
            parse_report(&raw, revision(), None).is_err(),
            "accepted {raw}"
        );
    }
}

#[test]
fn item_byte_and_result_id_limits_apply_before_retaining_reports() {
    assert!(
        parse_report(
            &json!({"kind":"full","items":vec![diagnostic("x"); MAX_REPORT_ITEMS + 1]}),
            revision(),
            None
        )
        .is_err()
    );
    assert!(
        parse_report(
            &json!({"kind":"full","items":[diagnostic(&"x".repeat(MAX_REPORT_BYTES))]}),
            revision(),
            None
        )
        .is_err()
    );
    assert!(
        parse_report(
            &json!({"kind":"full","items":[],"resultId":"x".repeat(MAX_RESULT_ID_BYTES + 1)}),
            revision(),
            None
        )
        .is_err()
    );
    let value = json!(["☃"]);
    let size = serde_json::to_vec(&value).unwrap().len();
    assert_eq!(encoded_size(&value, size).unwrap(), size);
    assert!(encoded_size(&value, size - 1).is_err());
}

#[test]
fn report_cache_bounds_count_and_bytes_and_does_not_reuse_changed_versions() {
    let mut cache = ReportCache::default();
    let report = parse_report(
        &json!({"kind":"full","items":[],"resultId":"id"}),
        revision(),
        None,
    )
    .unwrap();
    for i in 0..MAX_REPORTS {
        assert!(cache.insert(i.to_string(), report.clone()).is_empty());
    }
    assert_eq!(cache.insert("extra".to_string(), report.clone()), vec!["0"]);
    assert_eq!(cache.reports.len(), MAX_REPORTS);
    assert!(cache.previous("extra", revision()).is_some());
    assert!(
        cache
            .previous(
                "extra",
                Revision {
                    version: 2,
                    ..revision()
                }
            )
            .is_none()
    );
    let large = Report {
        bytes: MAX_CACHE_BYTES / 2 + 1,
        ..report
    };
    cache.insert("large-a".to_string(), large.clone());
    let removed = cache.insert("large-b".to_string(), large);
    assert!(removed.contains(&"large-a".to_string()));
    assert!(cache.bytes <= MAX_CACHE_BYTES);
}

#[test]
fn pull_only_server_populates_existing_diagnostics_surface_and_reuses_result_ids() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    let (_, uri) = source(&peer, temp.path(), "broken");
    peer.configure(json!({"textDocument/diagnostic":[
        {"result":{"kind":"full","resultId":"","items":[diagnostic("broken")]}},
        {"result":{"kind":"unchanged","resultId":"second +/="}},
        {"result":{"kind":"full","resultId":"third","items":[]}}
    ]}));
    assert!(
        peer.runtime.block_on(
            peer.client
                .wait_for_diagnostics(&uri, Duration::from_secs(5))
        )
    );
    assert_eq!(
        peer.client.diagnostics_snapshot()[&uri][0]["message"],
        "broken"
    );
    refresh(&peer, &uri).unwrap();
    assert_eq!(peer.client.diagnostics_snapshot()[&uri].len(), 1);
    refresh(&peer, &uri).unwrap();
    assert!(peer.client.diagnostics_snapshot()[&uri].is_empty());
    let frames = peer.frames();
    let pulls: Vec<_> = frames
        .iter()
        .filter(|frame| frame["method"] == "textDocument/diagnostic")
        .collect();
    assert_eq!(pulls.len(), 3);
    assert_eq!(pulls[0]["params"]["identifier"], "fixture-diagnostics");
    assert!(pulls[0]["params"].get("previousResultId").is_none());
    assert_eq!(pulls[1]["params"]["previousResultId"], "");
    assert_eq!(pulls[2]["params"]["previousResultId"], "second +/=");
    let init = frames
        .iter()
        .find(|frame| frame["method"] == "initialize")
        .unwrap();
    assert_eq!(
        init["params"]["capabilities"]["textDocument"]["diagnostic"]["dynamicRegistration"],
        false
    );
    assert_eq!(
        init["params"]["capabilities"]["textDocument"]["diagnostic"]["relatedDocumentSupport"],
        false
    );
}

#[test]
fn resynchronized_source_does_not_send_or_accept_an_old_result_baseline() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    let (path, uri) = source(&peer, temp.path(), "before");
    peer.configure(json!({"textDocument/diagnostic":[
        {"result":{"kind":"full","resultId":"old","items":[diagnostic("old")]}},
        {"result":{"kind":"unchanged","resultId":"wrong"}},
        {"result":{"kind":"full","items":[diagnostic("new")]}}
    ]}));
    refresh(&peer, &uri).unwrap();
    std::fs::write(&path, "after").unwrap();
    peer.client.ensure_synced(&path, "rust").unwrap();
    assert!(refresh(&peer, &uri).is_err());
    assert!(!peer.client.diagnostics_snapshot().contains_key(&uri));
    refresh(&peer, &uri).unwrap();
    assert_eq!(
        peer.client.diagnostics_snapshot()[&uri][0]["message"],
        "new"
    );
    let frames = peer.frames();
    for frame in frames
        .iter()
        .filter(|frame| frame["method"] == "textDocument/diagnostic")
        .skip(1)
    {
        assert!(frame["params"].get("previousResultId").is_none());
    }
}

#[test]
fn late_report_for_a_changed_or_closed_document_is_not_published() {
    for close in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let Some(peer) = peer(temp.path()) else {
            return;
        };
        let (path, uri) = source(&peer, temp.path(), "before");
        peer.configure(json!({"textDocument/diagnostic":[{"hold":true}]}));
        peer.runtime.block_on(async {
            let mut pending = Box::pin(
                peer.client
                    .refresh_document_diagnostics(&uri, Duration::from_secs(5)),
            );
            assert!(futures::poll!(pending.as_mut()).is_pending());
            if close {
                peer.client.invalidate(&uri);
            } else {
                std::fs::write(&path, "after").unwrap();
                peer.client.ensure_synced(&path, "rust").unwrap();
            }
            peer.client
                .call_no_wait_notify(
                    "test/release",
                    json!({"result":{
                        "kind":"full","resultId":"stale","items":[diagnostic("stale")]
                    }}),
                )
                .unwrap();
            let error = pending.await.unwrap_err();
            assert!(error.to_string().contains("changed or closed"), "{error}");
        });
        assert!(!peer.client.diagnostics_snapshot().contains_key(&uri));
    }
}

#[test]
fn a_newer_pull_supersedes_an_inflight_report_even_before_lane_admission() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    let (_, uri) = source(&peer, temp.path(), "source");
    peer.configure(json!({"textDocument/diagnostic":[{"hold":true}]}));
    peer.runtime.block_on(async {
        let mut first = Box::pin(
            peer.client
                .refresh_document_diagnostics(&uri, Duration::from_secs(5)),
        );
        assert!(futures::poll!(first.as_mut()).is_pending());
        let mut second = Box::pin(
            peer.client
                .refresh_document_diagnostics(&uri, Duration::from_secs(5)),
        );
        assert!(futures::poll!(second.as_mut()).is_pending());
        drop(second);
        peer.client
            .call_no_wait_notify(
                "test/release",
                json!({"result":{
                    "kind":"full","items":[diagnostic("superseded")]
                }}),
            )
            .unwrap();
        assert!(first.await.unwrap_err().to_string().contains("superseded"));
    });
    assert!(!peer.client.diagnostics_snapshot().contains_key(&uri));
}

#[test]
fn failed_pull_preserves_previous_diagnostics_instead_of_clearing_them() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    let (_, uri) = source(&peer, temp.path(), "source");
    peer.configure(json!({"textDocument/diagnostic":[
        {"result":{"kind":"full","resultId":"old","items":[diagnostic("retain me")]}},
        {"result":{"kind":"full","items":null}},
        {"error":{"code":-32603,"message":"provider failed"}}
    ]}));
    refresh(&peer, &uri).unwrap();
    assert!(refresh(&peer, &uri).is_err());
    assert!(
        !peer.runtime.block_on(
            peer.client
                .wait_for_diagnostics(&uri, Duration::from_secs(5))
        )
    );
    assert_eq!(
        peer.client.diagnostics_snapshot()[&uri][0]["message"],
        "retain me"
    );
}

#[test]
fn unsupported_unsynchronized_and_zero_budget_pulls_never_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({"textDocumentSync":1})) else {
        return;
    };
    let (_, uri) = source(&peer, temp.path(), "source");
    assert!(refresh(&peer, &uri).is_err());
    assert!(
        !peer
            .frames()
            .iter()
            .any(|frame| frame["method"] == "textDocument/diagnostic")
    );
    let Some(pull_peer) = self::peer(temp.path()) else {
        return;
    };
    assert!(refresh(&pull_peer, &uri).is_err());
    pull_peer
        .client
        .ensure_synced(&temp.path().join("source.rs"), "rust")
        .unwrap();
    assert!(
        pull_peer
            .runtime
            .block_on(
                pull_peer
                    .client
                    .refresh_document_diagnostics(&uri, Duration::ZERO)
            )
            .is_err()
    );
    assert!(
        !pull_peer
            .frames()
            .iter()
            .any(|frame| frame["method"] == "textDocument/diagnostic")
    );
}

#[test]
fn pull_timeouts_cancel_the_request_without_automatic_replay() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    let (_, uri) = source(&peer, temp.path(), "source");
    peer.configure(json!({"textDocument/diagnostic":[{"hold":true}]}));
    let error = peer
        .runtime
        .block_on(
            peer.client
                .refresh_document_diagnostics(&uri, Duration::from_millis(30)),
        )
        .unwrap_err();
    assert!(error.to_string().contains("LSP_TIMEOUT"), "{error}");
    assert!(!peer.client.diagnostics_snapshot().contains_key(&uri));
    let frames = peer.frames();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "textDocument/diagnostic")
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "$/cancelRequest")
            .count(),
        1
    );
}

#[test]
fn unadvertised_related_documents_are_not_imported_as_requested_results() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    let (_, uri) = source(&peer, temp.path(), "source");
    peer.configure(json!({"textDocument/diagnostic":[{"result":{
        "kind":"full","items":[diagnostic("requested")],
        "relatedDocuments":{"file:///unrequested.rs":{"kind":"full","items":[diagnostic("unrequested")]}}
    }}]}));
    refresh(&peer, &uri).unwrap();
    let snapshot = peer.client.diagnostics_snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[&uri][0]["message"], "requested");
}

#[test]
fn checked_cache_read_distinguishes_missing_and_explicit_empty_reports() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({"textDocumentSync":1})) else {
        return;
    };
    let (_, uri) = source(&peer, temp.path(), "source");
    let error = peer
        .runtime
        .block_on(peer.client.document_diagnostics(&uri, Duration::ZERO))
        .unwrap_err();
    assert!(error.to_string().contains("LSP_DIAGNOSTICS_PENDING"));
    peer.client
        .accept_diagnostics(&json!({"uri":uri,"diagnostics":[]}));
    assert!(
        peer.runtime
            .block_on(peer.client.document_diagnostics(&uri, Duration::ZERO))
            .unwrap()
            .is_empty()
    );
    peer.client.kill();
    let error = peer
        .runtime
        .block_on(peer.client.document_diagnostics(&uri, Duration::ZERO))
        .unwrap_err();
    assert!(error.to_string().contains("LSP_TRANSPORT_CLOSED"));
}

#[test]
fn checked_pull_preserves_original_failure_even_with_a_previous_cached_report() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    let (_, uri) = source(&peer, temp.path(), "source");
    peer.configure(json!({"textDocument/diagnostic":[
        {"result":{"kind":"full","resultId":"prior","items":[diagnostic("prior")]}},
        {"error":{"code":-32603,"message":"refresh failed"}}
    ]}));
    let items = peer
        .runtime
        .block_on(
            peer.client
                .document_diagnostics(&uri, Duration::from_secs(5)),
        )
        .unwrap();
    assert_eq!(items[0]["message"], "prior");
    let error = peer
        .runtime
        .block_on(
            peer.client
                .document_diagnostics(&uri, Duration::from_secs(5)),
        )
        .unwrap_err();
    assert!(error.to_string().contains("refresh failed"));
    assert_eq!(
        peer.client.diagnostics_snapshot()[&uri][0]["message"],
        "prior"
    );
}

#[test]
fn checked_diagnostics_rejects_changed_document_while_waiting_for_a_push() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({"textDocumentSync":1})) else {
        return;
    };
    let (path, uri) = source(&peer, temp.path(), "before");
    peer.runtime.block_on(async {
        let mut pending = Box::pin(
            peer.client
                .document_diagnostics(&uri, Duration::from_secs(5)),
        );
        assert!(futures::poll!(pending.as_mut()).is_pending());
        std::fs::write(&path, "after").unwrap();
        peer.client.ensure_synced(&path, "rust").unwrap();
        peer.client
            .accept_diagnostics(&json!({"uri":uri,"diagnostics":[diagnostic("new revision")]}));
        let error = pending.await.unwrap_err();
        assert!(error.to_string().contains("changed or closed"));
    });
}

#[test]
fn checked_cache_read_cannot_return_success_after_owner_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = peer(temp.path()) else {
        return;
    };
    let (_, uri) = source(&peer, temp.path(), "source");
    peer.client
        .accept_diagnostics(&json!({"uri":uri,"diagnostics":[]}));
    let owner = peer
        .runtime
        .request_cx_with_budget(asupersync::Budget::new());
    owner.cancel_with(
        asupersync::types::CancelKind::User,
        Some("cancel diagnostic query"),
    );
    let _guard = owner.set_current_restricted();
    let error = futures::executor::block_on(peer.client.document_diagnostics(&uri, Duration::ZERO))
        .unwrap_err();
    assert!(error.to_string().contains("LSP_CANCELLED"));
}

#[test]
fn unversioned_baselines_cannot_reuse_result_ids_or_accept_close_reopen_races() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(
        temp.path(),
        json!({
            "textDocumentSync":0,
            "diagnosticProvider":{"interFileDependencies":false,"workspaceDiagnostics":false}
        }),
    ) else {
        return;
    };
    let (path, uri) = source(&peer, temp.path(), "same contents");
    peer.configure(json!({"textDocument/diagnostic":[
        {"result":{"kind":"full","resultId":"first","items":[]}},
        {"result":{"kind":"full","resultId":"second","items":[]}},
        {"hold":true}
    ]}));
    refresh(&peer, &uri).unwrap();
    refresh(&peer, &uri).unwrap();
    for frame in peer
        .frames()
        .iter()
        .filter(|frame| frame["method"] == "textDocument/diagnostic")
    {
        assert!(frame["params"].get("previousResultId").is_none());
    }
    peer.runtime.block_on(async {
        let mut pending = Box::pin(
            peer.client
                .refresh_document_diagnostics(&uri, Duration::from_secs(5)),
        );
        assert!(futures::poll!(pending.as_mut()).is_pending());
        peer.client.invalidate(&uri);
        peer.client.ensure_synced(&path, "rust").unwrap();
        peer.client
            .call_no_wait_notify(
                "test/release",
                json!({"result":{
                    "kind":"full","items":[diagnostic("retired incarnation")]
                }}),
            )
            .unwrap();
        assert!(
            pending
                .await
                .unwrap_err()
                .to_string()
                .contains("changed or closed")
        );
    });
    assert!(!peer.client.diagnostics_snapshot().contains_key(&uri));
}
