//! Property-based tests for extension runtime types and hashing invariants.
//!
//! These tests are intentionally "no-mock": they validate pure invariants on
//! `HostcallRequest` construction and hashing, which are foundational to the
//! extension dispatcher + VCR matching layers.
#![forbid(unsafe_code)]

use pi::extensions_js::{HostcallKind, HostcallRequest};
use proptest::prelude::*;
use serde_json::{Value, json};

fn tool_name_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("read".to_string()),
        Just("grep".to_string()),
        Just("find".to_string()),
        Just("ls".to_string()),
        Just("write".to_string()),
        Just("edit".to_string()),
        Just("bash".to_string()),
        "[A-Za-z0-9 _\\-]{0,24}".prop_map(|s| s),
    ]
}

fn op_name_strategy() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_\\-]{0,24}".prop_map(|s| s)
}

fn json_leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        // Keep strings bounded to avoid pathological shrinking/time.
        ".{0,64}".prop_map(|s| json!(s)),
    ]
}

fn json_value() -> impl Strategy<Value = Value> {
    json_leaf().prop_recursive(3, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("[A-Za-z0-9_]{1,10}", inner, 0..4).prop_map(|map| {
                let mut out = serde_json::Map::new();
                for (key, value) in map {
                    out.insert(key, value);
                }
                Value::Object(out)
            }),
        ]
    })
}

fn hostcall_kind_and_payload() -> impl Strategy<Value = (HostcallKind, Value)> {
    prop_oneof![
        (tool_name_strategy(), json_value())
            .prop_map(|(name, payload)| { (HostcallKind::Tool { name }, payload) }),
        (".{0,32}", json_value()).prop_map(|(cmd, payload)| (HostcallKind::Exec { cmd }, payload)),
        json_value().prop_map(|payload| (HostcallKind::Http, payload)),
        (op_name_strategy(), json_value())
            .prop_map(|(op, payload)| { (HostcallKind::Session { op }, payload) }),
        (op_name_strategy(), json_value())
            .prop_map(|(op, payload)| (HostcallKind::Ui { op }, payload)),
        (op_name_strategy(), json_value())
            .prop_map(|(op, payload)| { (HostcallKind::Events { op }, payload) }),
        json_value().prop_map(|payload| (HostcallKind::Log, payload)),
    ]
}

fn request(kind: HostcallKind, payload: Value) -> HostcallRequest {
    HostcallRequest {
        call_id: "call-1".to_string(),
        kind,
        payload,
        trace_id: 0,
        extension_id: None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1024,
        max_shrink_iters: 0,
        .. ProptestConfig::default()
    })]

    #[test]
    fn params_hash_is_stable_hex((kind, payload) in hostcall_kind_and_payload()) {
        let req = request(kind, payload);
        let hash = req.params_hash();
        prop_assert_eq!(hash.len(), 64);
        prop_assert!(hash.chars().all(|ch| matches!(ch, '0'..='9' | 'a'..='f')));
        prop_assert_eq!(hash, req.params_hash());
    }

    #[test]
    fn required_capability_is_total(tool_name in tool_name_strategy()) {
        let req = request(HostcallKind::Tool { name: tool_name.clone() }, Value::Null);
        let cap = req.required_capability();
        prop_assert!(!cap.is_empty());
        prop_assert!(matches!(
            cap.as_str(),
            "read" | "write" | "exec" | "tool" | "http" | "session" | "ui" | "events" | "log"
        ));

        let normalized = tool_name.trim().to_ascii_lowercase();
        let expected = match normalized.as_str() {
            "read" | "grep" | "find" | "ls" => "read",
            "write" | "edit" => "write",
            "bash" => "exec",
            _ => "tool",
        };
        prop_assert_eq!(cap, expected);
    }

    #[test]
    fn required_capability_matches_kind_for_non_tool((kind, payload) in hostcall_kind_and_payload()) {
        let req = request(kind.clone(), payload);
        let cap = req.required_capability();
        match kind {
            HostcallKind::Tool { .. } => prop_assert!(matches!(
                cap.as_str(),
                "read" | "write" | "exec" | "tool"
            )),
            HostcallKind::Exec { .. } => prop_assert_eq!(cap, "exec"),
            HostcallKind::Http => prop_assert_eq!(cap, "http"),
            HostcallKind::Session { .. } => prop_assert_eq!(cap, "session"),
            HostcallKind::Ui { .. } => prop_assert_eq!(cap, "ui"),
            HostcallKind::Events { .. } => prop_assert_eq!(cap, "events"),
            HostcallKind::Log => prop_assert_eq!(cap, "log"),
        }
    }

    #[test]
    fn session_op_kind_always_yields_session_capability(op in op_name_strategy()) {
        let req = request(HostcallKind::Session { op }, Value::Null);
        prop_assert_eq!(req.required_capability(), "session");
    }

    #[test]
    fn params_hash_differs_for_different_session_ops(
        op_a in op_name_strategy(),
        op_b in op_name_strategy().prop_filter("different op", |b| !b.is_empty()),
        payload in json_value(),
    ) {
        // Skip if ops are the same after normalization
        if op_a.to_ascii_lowercase().replace('_', "") == op_b.to_ascii_lowercase().replace('_', "") {
            return Ok(());
        }
        let req_a = request(HostcallKind::Session { op: op_a }, payload.clone());
        let req_b = request(HostcallKind::Session { op: op_b }, payload);
        // Different ops with same payload should produce different hashes
        prop_assert_ne!(req_a.params_hash(), req_b.params_hash());
    }

    #[test]
    fn exec_params_for_hash_preserves_kind_cmd(
        cmd in ".{0,32}",
        payload_cmd in ".{0,32}",
        extra in prop::collection::btree_map(
            "[A-Za-z0-9_]{1,10}".prop_filter("not cmd", |k| k != "cmd"),
            json_value(),
            0..4
        )
    ) {
        let mut obj = serde_json::Map::new();
        obj.insert("cmd".to_string(), Value::String(payload_cmd));
        for (key, value) in extra {
            obj.insert(key, value);
        }

        let req = request(HostcallKind::Exec { cmd: cmd.clone() }, Value::Object(obj));
        let params = req.params_for_hash();
        prop_assert_eq!(params.get("cmd"), Some(&Value::String(cmd)));
    }
}
