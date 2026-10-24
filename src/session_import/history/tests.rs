use super::*;
use serde_json::json;

fn message(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
    json!({"type":role,"uuid":id,"parentUuid":parent,"message":{"role":role,"content":text}})
}

fn encode(entries: &[Value]) -> String {
    entries
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn active_ids(history: &History<'_>) -> Vec<String> {
    history
        .order
        .iter()
        .filter_map(|&index| {
            if history.excluded(index) {
                return None;
            }
            let value: Value = serde_json::from_slice(history.lines[index].bytes).ok()?;
            matches!(value["type"].as_str(), Some("user" | "assistant"))
                .then(|| value["uuid"].as_str().unwrap().to_string())
        })
        .collect()
}

fn branched() -> Vec<Value> {
    let mut side = message("side", Some("root"), "assistant", "subagent output");
    side["isSidechain"] = json!(true);
    let mut team = message("team", Some("root"), "assistant", "teammate output");
    team["teamName"] = json!("reviewers");
    vec![
        message("root", None, "user", "original question"),
        message("old", Some("root"), "assistant", "abandoned answer"),
        message("new", Some("root"), "assistant", "chosen answer"),
        side,
        team,
        json!({"type":"progress","uuid":"late-progress","parentUuid":"old"}),
        json!({"type":"file-history-snapshot","messageId":"root"}),
    ]
}

#[test]
fn latest_main_leaf_ignores_progress_sidechain_and_team_tails() {
    let raw = encode(&branched());
    let history = History::prepare(ImportSource::Claude, raw.as_bytes(), None).unwrap();
    assert_eq!(history.leaf.as_deref(), Some("new"));
    assert_eq!(history.branches, 2);
    assert_eq!(active_ids(&history), ["root", "new"]);
    assert_eq!(history.excluded.len(), 3);
    assert_eq!(history.order.len(), history.lines.len());
    assert_eq!(
        history.order.iter().copied().collect::<HashSet<_>>().len(),
        history.lines.len()
    );
}

#[test]
fn file_order_breaks_leaf_ties_but_ancestry_orders_forward_parents() {
    let mut child = message("child", Some("parent"), "assistant", "last answer");
    child["timestamp"] = json!("1900-01-01T00:00:00Z");
    let raw = encode(&[
        child,
        message("parent", Some("root"), "user", "second question"),
        message("root", None, "user", "first question"),
    ]);
    let history = History::prepare(ImportSource::Claude, raw.as_bytes(), None).unwrap();
    assert_eq!(active_ids(&history), ["root", "parent", "child"]);
    assert_eq!(history.branches, 1);
    assert_eq!(history.lines[history.order[0]].number, 3);
}

#[test]
fn hidden_messages_bridge_ancestry_but_never_become_provider_context() {
    let mut hidden = message("hidden", Some("root"), "user", "internal metadata");
    hidden["isMeta"] = json!(true);
    let raw = encode(&[
        message("root", None, "user", "question"),
        hidden,
        json!({"type":"attachment","uuid":"bridge","parentUuid":"hidden"}),
        message("answer", Some("bridge"), "assistant", "answer"),
    ]);
    let history = History::prepare(ImportSource::Claude, raw.as_bytes(), None).unwrap();
    assert_eq!(active_ids(&history), ["root", "answer"]);
    assert!(history.excluded(1));
}

#[test]
fn compaction_uses_the_new_root_without_following_logical_parent() {
    let raw = encode(&[
        message("old", None, "user", "obsolete full history"),
        json!({"type":"system","subtype":"compact_boundary","uuid":"boundary",
            "parentUuid":null,"logicalParentUuid":"old"}),
        message("summary", Some("boundary"), "user", "compacted history"),
        message("answer", Some("summary"), "assistant", "next answer"),
    ]);
    let history = History::prepare(ImportSource::Claude, raw.as_bytes(), None).unwrap();
    assert_eq!(active_ids(&history), ["summary", "answer"]);
    assert!(history.excluded(0));
}

#[test]
fn explicit_selection_imports_an_alternate_or_nonterminal_ancestry() {
    let raw = encode(&branched());
    for (leaf, expected) in [
        ("root", vec!["root"]),
        ("old", vec!["root", "old"]),
        ("side", vec!["root", "side"]),
        ("team", vec!["root", "team"]),
    ] {
        let history = History::prepare(ImportSource::Claude, raw.as_bytes(), Some(leaf)).unwrap();
        assert_eq!(active_ids(&history), expected);
    }
    for leaf in ["missing", "late-progress", "", "\n"] {
        assert!(History::prepare(ImportSource::Claude, raw.as_bytes(), Some(leaf)).is_err());
    }
}

#[test]
fn sidechain_only_logs_require_deliberate_selection() {
    let mut side = message("side", None, "assistant", "isolated agent");
    side["isSidechain"] = json!(true);
    let raw = encode(&[side]);
    assert!(History::prepare(ImportSource::Claude, raw.as_bytes(), None).is_err());
    assert_eq!(
        active_ids(&History::prepare(ImportSource::Claude, raw.as_bytes(), Some("side")).unwrap()),
        ["side"]
    );
}

#[test]
fn default_main_selection_refuses_to_cross_into_sidechain_ancestry() {
    let mut side = message("side", None, "assistant", "isolated agent");
    side["isSidechain"] = json!(true);
    let raw = encode(&[
        side,
        message("main", Some("side"), "user", "ambiguous join"),
    ]);
    assert!(History::prepare(ImportSource::Claude, raw.as_bytes(), None).is_err());
    assert!(History::prepare(ImportSource::Claude, raw.as_bytes(), Some("main")).is_ok());
}

#[test]
fn incomplete_ambiguous_and_cyclic_graphs_never_become_shorter_successes() {
    for entries in [
        vec![message("leaf", Some("missing"), "user", "question")],
        vec![
            message("x", Some("y"), "user", "x"),
            message("y", Some("x"), "assistant", "y"),
        ],
        vec![message("self", Some("self"), "assistant", "cycle")],
        vec![
            message("dup", None, "user", "first"),
            message("dup", None, "user", "different"),
        ],
        vec![
            message("root", None, "user", "first"),
            json!({"type":"assistant","message":{"content":"unlinked"}}),
        ],
        vec![
            message("root", None, "user", "first"),
            json!({"type":"assistant","uuid":"missing-parent","message":{"content":"partial"}}),
        ],
    ] {
        let raw = encode(&entries);
        assert!(
            History::prepare(ImportSource::Claude, raw.as_bytes(), None).is_err(),
            "{raw}"
        );
    }
}

#[test]
fn identical_duplicate_envelopes_are_archived_and_do_not_repeat_messages() {
    let root = message("root", None, "user", "once");
    let raw = encode(&[
        root.clone(),
        message("answer", Some("root"), "assistant", "answer"),
        root,
    ]);
    let history = History::prepare(ImportSource::Claude, raw.as_bytes(), None).unwrap();
    assert_eq!(active_ids(&history), ["root", "answer"]);
    assert!(history.excluded(2));
}

#[test]
fn duplicate_structural_json_fields_are_not_last_key_wins() {
    for raw in [
        r#"{"type":"user","uuid":"first","uuid":"last","parentUuid":null,"message":{"content":"x"}}"#,
        r#"{"type":"user","uuid":"first","parentUuid":"missing","parentUuid":null,"message":{"content":"x"}}"#,
        r#"{"type":"user","uuid":"first","parentUuid":null,"isSidechain":true,"isSidechain":false,"message":{"content":"x"}}"#,
    ] {
        assert!(History::prepare(ImportSource::Claude, raw.as_bytes(), None).is_err());
    }
}

#[test]
fn line_numbers_keep_blank_lines_and_corruption_as_audit_input() {
    let first = message("first", None, "user", "question");
    let last = message("last", Some("first"), "assistant", "answer");
    let raw = format!("\n{first}\n\nnot-json\n\n{last}\n");
    let history = History::prepare(ImportSource::Claude, raw.as_bytes(), None).unwrap();
    assert_eq!(
        history
            .lines
            .iter()
            .map(|line| line.number)
            .collect::<Vec<_>>(),
        [2, 4, 6]
    );
    assert_eq!(active_ids(&history), ["first", "last"]);
    assert_eq!(history.lines[history.order[0]].bytes, b"not-json");
}

#[test]
fn legacy_unlinked_and_codex_records_keep_their_original_order() {
    let raw = b"\n{\"type\":\"user\",\"uuid\":null,\"message\":{\"content\":\"x\"}}\ncorrupt\n";
    for source in [ImportSource::Claude, ImportSource::Codex] {
        let history = History::prepare(source, raw, None).unwrap();
        assert!(history.leaf.is_none());
        assert!(history.excluded.is_empty());
        assert_eq!(history.order, [0, 1]);
        assert!(History::prepare(source, raw, Some("any")).is_err());
    }
}

#[test]
fn identifiers_and_flags_are_validated_before_ancestry_is_selected() {
    for (key, value) in [
        ("uuid", json!("x".repeat(MAX_ID_BYTES + 1))),
        ("uuid", json!(5)),
        ("parentUuid", json!(false)),
        ("isSidechain", json!("false")),
        ("isMeta", json!(null)),
        ("teamName", json!(5)),
    ] {
        let mut entry = message("root", None, "user", "question");
        entry[key] = value;
        assert!(History::prepare(ImportSource::Claude, encode(&[entry]).as_bytes(), None).is_err());
    }
}

#[test]
fn large_duplicate_runs_are_coalesced_without_quadratic_ancestry_scans() {
    let entry = message("root", None, "user", "question");
    let raw = format!("{entry}\n").repeat(5000);
    let history = History::prepare(ImportSource::Claude, raw.as_bytes(), None).unwrap();
    assert_eq!(active_ids(&history), ["root"]);
    assert_eq!(history.excluded.len(), 4999);
}

#[test]
fn nonblank_record_limit_prevents_unbounded_graph_working_sets() {
    let raw = "{}\n".repeat(MAX_RECORDS + 1);
    let error = History::prepare(ImportSource::Claude, raw.as_bytes(), None)
        .err()
        .expect("bounded source");
    assert!(error.to_string().contains("record limit"));
}

fn read_messages(path: &str) -> Vec<crate::model::Message> {
    let session = futures::executor::block_on(crate::session::Session::open(path)).unwrap();
    session.to_messages_for_current_path()
}

#[test]
fn public_import_reopens_one_branch_and_preserves_excluded_source_bytes() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("claude.jsonl");
    let target = root.path().join("native");
    let entries = branched();
    let raw = encode(&entries);
    std::fs::write(&source, &raw).unwrap();
    let outcome = super::super::import_claude(&source, Some(&target)).unwrap();
    assert_eq!(outcome.imported, 2);
    assert_eq!(outcome.skipped, 3);
    let messages = read_messages(&outcome.session_path);
    let active = serde_json::to_string(&messages).unwrap();
    assert!(active.contains("chosen answer"));
    for excluded in ["abandoned answer", "subagent output", "teammate output"] {
        assert!(
            !active.contains(excluded),
            "{excluded} leaked into native context"
        );
    }
    let saved: Vec<Value> = std::fs::read_to_string(&outcome.session_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for entry in &entries[1..2] {
        assert!(
            saved
                .iter()
                .any(|value| value.pointer("/data/raw").and_then(Value::as_str)
                    == Some(entry.to_string().as_str()))
        );
    }
    assert!(
        saved
            .iter()
            .any(|value| value.pointer("/data/selectedLeafId") == Some(&json!("new")))
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), raw);
    let again = super::super::import_claude_branch(&source, "new", Some(&target)).unwrap();
    assert!(again.already_imported);
    assert_eq!(again.session_id, outcome.session_id);
    let alternate = super::super::import_claude_branch(&source, "old", Some(&target)).unwrap();
    assert!(!alternate.already_imported);
    assert_ne!(alternate.session_id, outcome.session_id);
    let active = serde_json::to_string(&read_messages(&alternate.session_path)).unwrap();
    assert!(active.contains("abandoned answer"));
    assert!(!active.contains("chosen answer"));
    assert!(
        super::super::import_claude_branch(&source, "old", Some(&target))
            .unwrap()
            .already_imported
    );
}

#[test]
fn excluded_tool_results_cannot_complete_a_call_in_the_selected_branch() {
    use crate::model::{ContentBlock, Message, StopReason};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("claude.jsonl");
    let mut call = message("call", Some("root"), "assistant", "");
    call["message"]["content"] = json!([{"type":"tool_use","id":"tool-a","name":"bash","input":{"command":"not executed by import"}}]);
    let mut old_output = message("old-output", Some("call"), "user", "");
    old_output["message"]["content"] =
        json!([{"type":"tool_result","tool_use_id":"tool-a","content":"abandoned result"}]);
    let raw = encode(&[
        message("root", None, "user", "question"),
        call,
        old_output,
        message("new-branch", Some("call"), "user", "take another approach"),
    ]);
    std::fs::write(&source, raw).unwrap();
    let outcome = super::super::import_claude(&source, Some(&root.path().join("native"))).unwrap();
    let messages = read_messages(&outcome.session_path);
    assert_eq!(messages.len(), 3);
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)))
    );
    let Message::Assistant(assistant) = &messages[1] else {
        panic!("historical assistant")
    };
    assert_eq!(assistant.stop_reason, StopReason::Stop);
    assert!(
        !assistant
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)))
    );
    assert!(
        !serde_json::to_string(&messages)
            .unwrap()
            .contains("abandoned result")
    );
}

#[test]
fn forward_parent_tool_exchange_stays_complete_after_native_reopen() {
    use crate::model::Message;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("claude.jsonl");
    let mut call = message("call", Some("root"), "assistant", "");
    call["message"]["content"] = json!([{"type":"tool_use","id":"tc","name":"read","input":{}}]);
    let mut output = message("output", Some("call"), "user", "");
    output["message"]["content"] =
        json!([{"type":"tool_result","tool_use_id":"tc","content":"source text"}]);
    std::fs::write(
        &source,
        encode(&[output, call, message("root", None, "user", "read source")]),
    )
    .unwrap();
    let outcome = super::super::import_claude(&source, Some(&root.path().join("native"))).unwrap();
    let messages = read_messages(&outcome.session_path);
    assert_eq!(messages.len(), 3);
    assert_eq!(outcome.skipped, 0);
    assert!(
        matches!(&messages[2], Message::ToolResult(result) if result.tool_name == "read" && result.tool_call_id == "tc")
    );
}

#[test]
fn invalid_branch_rejects_before_creating_any_destination() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("claude.jsonl");
    let target = root.path().join("must-not-exist");
    let raw = encode(&[message("broken", Some("missing"), "user", "question")]);
    std::fs::write(&source, &raw).unwrap();
    assert!(super::super::import_claude(&source, Some(&target)).is_err());
    assert!(!target.exists());
    assert_eq!(std::fs::read_to_string(&source).unwrap(), raw);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
