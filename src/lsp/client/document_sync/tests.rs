use super::*;
use proptest::prelude::*;

#[test]
fn synchronization_capability_forms_and_defaults() {
    assert_eq!(
        SyncPolicy::parse(&json!({})).unwrap(),
        SyncPolicy::default()
    );
    for kind in 0..=2 {
        assert_eq!(
            SyncPolicy::parse(&json!({"textDocumentSync":kind})).unwrap(),
            SyncPolicy {
                change: kind,
                open_close: kind != 0,
                save: None
            }
        );
    }
    assert_eq!(
        SyncPolicy::parse(&json!({"textDocumentSync":{}})).unwrap(),
        SyncPolicy::default()
    );
    for (save, expected) in [
        (json!(false), None),
        (json!(true), Some(false)),
        (json!({}), Some(false)),
        (json!({"includeText":true}), Some(true)),
    ] {
        assert_eq!(
            SyncPolicy::parse(&json!({"textDocumentSync":{
                "openClose":true,"change":2,"save":save
            }}))
            .unwrap(),
            SyncPolicy {
                change: 2,
                open_close: true,
                save: expected
            }
        );
    }
}

#[test]
fn invalid_sync_policies_are_not_guessed() {
    for sync in [
        json!(-1),
        json!(3),
        json!(false),
        json!(null),
        json!("2"),
        json!({"change":3}),
        json!({"change":true}),
        json!({"openClose":"yes"}),
        json!({"save":0}),
        json!({"save":{"includeText":"yes"}}),
    ] {
        assert!(
            SyncPolicy::parse(&json!({"textDocumentSync":sync})).is_err(),
            "{sync}"
        );
    }
    for encoding in [json!("utf-8"), json!("utf-32"), json!(null), json!(7)] {
        assert!(SyncPolicy::parse(&json!({"positionEncoding":encoding})).is_err());
    }
    assert!(SyncPolicy::parse(&json!({"positionEncoding":"utf-16"})).is_ok());
}

// An independent reference maps protocol positions into raw UTF-8 offsets.
// It rejects surrogate-pair and CRLF interiors instead of rounding them.
fn reference_offset(text: &str, wanted: &Value) -> usize {
    let (wanted_line, wanted_column) = (
        wanted["line"].as_u64().unwrap(),
        wanted["character"].as_u64().unwrap(),
    );
    let (mut line, mut column, mut offset) = (0, 0, 0);
    while offset < text.len() {
        if (line, column) == (wanted_line, wanted_column) {
            return offset;
        }
        let rest = &text[offset..];
        if rest.starts_with("\r\n") {
            offset += 2;
            line += 1;
            column = 0;
        } else {
            let ch = rest.chars().next().unwrap();
            offset += ch.len_utf8();
            if matches!(ch, '\n' | '\r') {
                line += 1;
                column = 0;
            } else {
                column += ch.len_utf16() as u64;
            }
        }
    }
    assert_eq!(
        (line, column),
        (wanted_line, wanted_column),
        "invalid position in {text:?}"
    );
    offset
}

fn replay_change(before: &str, change: &Value) -> String {
    let start = reference_offset(before, &change["range"]["start"]);
    let end = reference_offset(before, &change["range"]["end"]);
    format!(
        "{}{}{}",
        &before[..start],
        change["text"].as_str().unwrap(),
        &before[end..]
    )
}

#[test]
fn incremental_edits_roundtrip_unicode_and_every_line_ending() {
    let samples = [
        "",
        "abc",
        "abXc",
        "😀x",
        "😃x",
        "a\rb",
        "a\r\nb",
        "a\nb",
        "\r",
        "\r\n",
        "\n",
        "☃\r\n😀\nlast",
        "e\u{301}",
    ];
    for before in samples {
        for after in samples {
            let change = incremental_change(before, after);
            assert_eq!(
                replay_change(before, &change),
                after,
                "{before:?} -> {after:?}: {change}"
            );
        }
    }
}

#[test]
fn incremental_edits_send_only_the_changed_span_and_utf16_positions() {
    assert_eq!(
        incremental_change("prefix😀Xsuffix", "prefix😀Ysuffix"),
        json!({
            "range":{"start":{"line":0,"character":8},"end":{"line":0,"character":9}}, "text":"Y"
        })
    );
    assert_eq!(
        incremental_change("a\r\n😀x", "a\r\n😀y"),
        json!({
            "range":{"start":{"line":1,"character":2},"end":{"line":1,"character":3}}, "text":"y"
        })
    );
}

proptest! {
    #[test]
    fn incremental_change_reconstructs_arbitrary_unicode(
        before in prop::collection::vec(any::<char>(), 0..80),
        after in prop::collection::vec(any::<char>(), 0..80),
        shared in prop::collection::vec(any::<char>(), 0..30),
    ) {
        let shared: String = shared.into_iter().collect();
        let before = format!("{shared}{}{shared}", before.into_iter().collect::<String>());
        let after = format!("{shared}{}{shared}", after.into_iter().collect::<String>());
        prop_assert_eq!(replay_change(&before, &incremental_change(&before, &after)), after);
    }
}

#[test]
fn source_admission_rejects_large_files_and_directories() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("large");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(MAX_DOCUMENT_BYTES as u64 + 1)
        .unwrap();
    assert!(
        read_document(&path)
            .unwrap_err()
            .to_string()
            .contains("LSP_DOCUMENT_LIMIT")
    );
    assert!(
        read_document(temp.path())
            .unwrap_err()
            .to_string()
            .contains("LSP_FILE_UNREADABLE")
    );
}

#[cfg(unix)]
mod protocol {
    use super::*;
    use std::time::Duration;

    // Real Content-Length framing over a subprocess; no transport mock. The
    // fixture records exactly what Pi sent and supports ordered diagnostics.
    const SERVER: &str = r"
import json, sys
frames = []
caps = json.loads(sys.argv[1])
def send(message):
    body = json.dumps(message, ensure_ascii=False).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
while True:
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line == b'\r\n': break
        key, value = line.decode('ascii').split(':', 1)
        if key.lower() == 'content-length': length = int(value)
    raw = sys.stdin.buffer.read(length)
    if len(raw) != length: sys.exit(2)
    message = json.loads(raw)
    method = message.get('method')
    if method == 'exit': sys.exit(0)
    if 'id' not in message:
        frames.append(message)
        continue
    if method == 'initialize': result = {'capabilities': caps}
    elif method == 'test/frames': result = frames
    elif method == 'test/publish':
        send({'jsonrpc':'2.0','method':'textDocument/publishDiagnostics','params':message['params']})
        result = {}
    else: result = {}
    send({'jsonrpc':'2.0','id':message['id'],'result':result})
";

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn connect(root: &Path, caps: Value, runtime: &asupersync::runtime::Runtime) -> LspClient {
        runtime
            .block_on(LspClient::connect(
                "python3",
                &[
                    "-u".to_string(),
                    "-c".to_string(),
                    SERVER.to_string(),
                    caps.to_string(),
                ],
                &[],
                root,
                None,
                Duration::from_secs(5),
            ))
            .unwrap()
    }

    fn frames(client: &LspClient, runtime: &asupersync::runtime::Runtime) -> Vec<Value> {
        runtime
            .block_on(client.call("test/frames", json!({}), Duration::from_secs(5)))
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["method"] != "initialized")
            .cloned()
            .collect()
    }

    fn source(root: &Path, text: &str) -> std::path::PathBuf {
        let path = root.join("source.txt");
        std::fs::write(&path, text).unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn full_sync_updates_without_reopening_and_sends_requested_save_text() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime();
        let client = connect(
            temp.path(),
            json!({"textDocumentSync":{"openClose":true,"change":1,"save":{"includeText":true}}}),
            &rt,
        );
        let path = source(temp.path(), "before");
        let uri = client.ensure_synced(&path, "plaintext").unwrap();
        let first = client.document_snapshots()[&path];
        client.ensure_synced(&path, "plaintext").unwrap();
        assert_eq!(client.document_snapshots()[&path].version, first.version);
        std::fs::write(&path, "after😀").unwrap();
        client.ensure_synced(&path, "plaintext").unwrap();
        let messages = frames(&client, &rt);
        assert_eq!(
            messages
                .iter()
                .map(|m| m["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "textDocument/didOpen",
                "textDocument/didChange",
                "textDocument/didSave"
            ]
        );
        assert_eq!(
            messages[1]["params"]["contentChanges"],
            json!([{"text":"after😀"}])
        );
        assert_eq!(
            messages[2]["params"],
            json!({"textDocument":{"uri":uri},"text":"after😀"})
        );
        let current = client.document_snapshots()[&path];
        assert!(current.version > first.version);
        assert_eq!(current.hash, content_hash("after😀"));
        client.kill();
    }

    #[test]
    fn incremental_sync_preserves_a_live_document_and_reopens_only_for_language_changes() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime();
        let client = connect(temp.path(), json!({"textDocumentSync":2}), &rt);
        let before = "a\r\n😀x\rtail";
        let after = "a\r\n😀long\rtail";
        let path = source(temp.path(), before);
        client.ensure_synced(&path, "plaintext").unwrap();
        std::fs::write(&path, after).unwrap();
        client.ensure_synced(&path, "plaintext").unwrap();
        let messages = frames(&client, &rt);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["method"], "textDocument/didChange");
        assert_eq!(
            replay_change(before, &messages[1]["params"]["contentChanges"][0]),
            after
        );
        client.ensure_synced(&path, "rust").unwrap();
        let messages = frames(&client, &rt);
        assert_eq!(messages[2]["method"], "textDocument/didClose");
        assert_eq!(messages[3]["method"], "textDocument/didOpen");
        assert_eq!(messages[3]["params"]["textDocument"]["languageId"], "rust");
        assert_eq!(messages[3]["params"]["textDocument"]["text"], after);
        client.kill();
    }

    #[test]
    fn absent_sync_and_save_options_do_not_emit_unrequested_notifications() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime();
        let client = connect(temp.path(), json!({}), &rt);
        let path = source(temp.path(), "first");
        client.ensure_synced(&path, "plaintext").unwrap();
        std::fs::write(&path, "second").unwrap();
        client.ensure_synced(&path, "plaintext").unwrap();
        assert!(client.document_snapshots().is_empty());
        client.invalidate_all();
        assert!(frames(&client, &rt).is_empty());
        client.kill();
    }

    #[test]
    fn save_without_include_text_and_open_only_fallback_follow_server_options() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime();
        let client = connect(
            temp.path(),
            json!({"textDocumentSync":{"openClose":true,"save":true}}),
            &rt,
        );
        let path = source(temp.path(), "first");
        client.ensure_synced(&path, "plaintext").unwrap();
        std::fs::write(&path, "second").unwrap();
        client.ensure_synced(&path, "plaintext").unwrap();
        let messages = frames(&client, &rt);
        assert_eq!(
            messages
                .iter()
                .map(|m| m["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "textDocument/didOpen",
                "textDocument/didClose",
                "textDocument/didOpen",
                "textDocument/didSave"
            ]
        );
        assert!(messages[3]["params"].get("text").is_none());
        client.kill();
    }

    #[test]
    fn versioned_diagnostics_cannot_survive_document_changes_or_close() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime();
        let client = connect(temp.path(), json!({"textDocumentSync":1}), &rt);
        let path = source(temp.path(), "first");
        let uri = client.ensure_synced(&path, "plaintext").unwrap();
        let old_version = client.document_snapshots()[&path].version;
        let publish = |version: Value| {
            rt.block_on(client.call(
                "test/publish",
                json!({"uri":uri,"version":version,"diagnostics":[{"message":"fixture"}]}),
                Duration::from_secs(5),
            ))
            .unwrap();
            client.diagnostics_snapshot()
        };
        assert!(publish(json!(old_version)).contains_key(&uri));
        std::fs::write(&path, "second").unwrap();
        client.ensure_synced(&path, "plaintext").unwrap();
        let version = client.document_snapshots()[&path].version;
        for invalid in [
            json!(old_version),
            json!(version + 1),
            json!("2"),
            json!(-1),
        ] {
            assert!(!publish(invalid).contains_key(&uri));
        }
        assert!(publish(json!(version)).contains_key(&uri));
        client.invalidate(&uri);
        assert!(!publish(json!(version)).contains_key(&uri));
        client.ensure_synced(&path, "plaintext").unwrap();
        assert!(client.document_snapshots()[&path].version > version);
        assert!(!publish(json!(version)).contains_key(&uri));
        client.kill();
    }

    #[test]
    fn source_and_version_limits_fail_before_sending_a_new_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime();
        let client = connect(temp.path(), json!({"textDocumentSync":2}), &rt);
        let path = source(temp.path(), "first");
        client.ensure_synced(&path, "plaintext").unwrap();
        let original = client.document_snapshots()[&path];
        client
            .next_document_version
            .store(u64::MAX, Ordering::SeqCst);
        std::fs::write(&path, "second").unwrap();
        assert!(
            client
                .ensure_synced(&path, "plaintext")
                .unwrap_err()
                .to_string()
                .contains("LSP_VERSION_EXHAUSTED")
        );
        assert_eq!(client.document_snapshots()[&path].version, original.version);
        assert_eq!(frames(&client, &rt).len(), 1);
        client.next_document_version.store(2, Ordering::SeqCst);
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_DOCUMENT_BYTES as u64 + 1)
            .unwrap();
        assert!(client.ensure_synced(&path, "plaintext").is_err());
        assert_eq!(frames(&client, &rt).len(), 1);
        client.kill();
    }

    #[test]
    fn dead_transport_cannot_report_a_cached_document_as_synchronized() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime();
        let client = connect(temp.path(), json!({"textDocumentSync":2}), &rt);
        let path = source(temp.path(), "first");
        client.ensure_synced(&path, "plaintext").unwrap();
        client.kill();
        assert!(client.ensure_synced(&path, "plaintext").is_err());
        assert_eq!(client.open_document_count(), 0);
        assert!(client.document_snapshots().is_empty());
    }

    #[test]
    fn source_cache_eviction_closes_the_oldest_document_and_reopen_uses_a_new_version() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime();
        let client = connect(temp.path(), json!({"textDocumentSync":2}), &rt);
        let first = source(temp.path(), "first");
        let uri = client.ensure_synced(&first, "plaintext").unwrap();
        let first_version = client.document_snapshots()[&first].version;
        for index in 0..MAX_OPEN_DOCUMENTS {
            let path = temp.path().join(format!("source-{index}"));
            std::fs::write(&path, "x").unwrap();
            client.ensure_synced(&path, "plaintext").unwrap();
        }
        assert_eq!(client.open_document_count(), MAX_OPEN_DOCUMENTS);
        assert!(!client.document_snapshots().contains_key(&first));
        let messages = frames(&client, &rt);
        assert!(
            messages
                .iter()
                .any(|m| m["method"] == "textDocument/didClose"
                    && m["params"]["textDocument"]["uri"] == uri)
        );
        client.ensure_synced(&first, "plaintext").unwrap();
        assert!(client.document_snapshots()[&first].version > first_version);
        assert_eq!(client.open_document_count(), MAX_OPEN_DOCUMENTS);
        client.kill();
    }

    #[test]
    fn uri_aliases_share_diagnostics_waiting_and_document_invalidation() {
        let temp = tempfile::tempdir().unwrap();
        let rt = runtime();
        let client = connect(temp.path(), json!({"textDocumentSync":2}), &rt);
        let path = source(temp.path(), "first");
        let uri = client.ensure_synced(&path, "plaintext").unwrap();
        let version = client.document_snapshots()[&path].version;
        let alias = format!("FILE://LOCALHOST{}", uri.strip_prefix("file://").unwrap())
            .replace("source.txt", "%73ource%2Etxt");
        rt.block_on(client.call(
            "test/publish",
            json!({
                "uri":alias,"version":version,"diagnostics":[{"message":"alias diagnostic"}]
            }),
            Duration::from_secs(5),
        ))
        .unwrap();
        let diagnostics = client.diagnostics_snapshot();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics.contains_key(&uri));
        assert!(!diagnostics.contains_key(&alias));
        assert!(rt.block_on(client.wait_for_diagnostics(&alias, Duration::ZERO)));
        assert!(!rt.block_on(client.wait_for_diagnostics("file://relative", Duration::ZERO)));
        client.invalidate(&alias);
        assert_eq!(client.open_document_count(), 0);
        assert!(client.diagnostics_snapshot().is_empty());
        let messages = frames(&client, &rt);
        let last = messages.last().unwrap();
        assert_eq!(last["method"], "textDocument/didClose");
        assert_eq!(last["params"]["textDocument"]["uri"], uri);
        client.ensure_synced(&path, "plaintext").unwrap();
        rt.block_on(client.call(
            "test/publish",
            json!({
                "uri":alias,"version":version,"diagnostics":[{"message":"stale alias"}]
            }),
            Duration::from_secs(5),
        ))
        .unwrap();
        assert!(client.diagnostics_snapshot().is_empty());
        client.kill();
    }
}
