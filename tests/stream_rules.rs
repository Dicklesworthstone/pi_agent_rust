//! Integration and characterization tests for Time-Traveling Stream Rules (TTSR) (bd-cv653.3.4).

use tempfile::tempdir;

use pi::stream_rules::{
    GrievancesLedger, RollingStreamMatcher, StreamChannel, StreamRule, StreamRuleStore, TtsrAction,
    TtsrCoordinator,
};

#[test]
fn test_stream_rule_split_chunk_boundary_matching() {
    let rules = vec![
        StreamRule {
            id: "no-box-leak".to_string(),
            name: "No Box::leak".to_string(),
            pattern: r"Box::leak".to_string(),
            body: "Never use Box::leak; use structured concurrency and scoped references."
                .to_string(),
            enabled: true,
            created_from: None,
            cooldown_turns: None,
        },
        StreamRule {
            id: "no-todo".to_string(),
            name: "No TODO".to_string(),
            pattern: r"TODO\(\w+\)".to_string(),
            body: "Resolve all TODOs before submitting.".to_string(),
            enabled: true,
            created_from: None,
            cooldown_turns: None,
        },
    ];

    let mut matcher = RollingStreamMatcher::new(&rules, 4096);

    // Feed text split right across the regex pattern
    let m1 = matcher.feed(
        "fn memory_danger() {\n    let ptr = Box::",
        StreamChannel::AssistantText,
    );
    assert!(m1.is_none());

    let m2 = matcher.feed("leak(boxed);\n}", StreamChannel::AssistantText);
    let Some(match_result) = m2 else {
        assert!(false, "Matcher should detect split pattern across chunks");
        return;
    };

    assert_eq!(match_result.rule_id, "no-box-leak");
    assert_eq!(match_result.rule_name, "No Box::leak");
    assert_eq!(match_result.matched_excerpt, "Box::leak");
}

#[test]
fn test_tool_call_argument_stream_is_strictly_exempt() {
    let rules = vec![StreamRule {
        id: "no-unwrap".to_string(),
        name: "No Unwrap".to_string(),
        pattern: r"\.unwrap\(\)".to_string(),
        body: "Never use unwrap.".to_string(),
        enabled: true,
        created_from: None,
        cooldown_turns: None,
    }];

    let mut matcher = RollingStreamMatcher::new(&rules, 4096);

    // A tool call argument containing the pattern in its JSON payload
    let m = matcher.feed(
        r#"{"file_path": "src/main.rs", "content": "let val = opt.unwrap();"}"#,
        StreamChannel::ToolCallArgument,
    );
    assert!(m.is_none());
}

#[test]
fn test_ttsr_coordinator_injection_and_cap_guard() {
    let rules = vec![StreamRule {
        id: "no-raw-sql".to_string(),
        name: "No Raw SQL".to_string(),
        pattern: r"(?i)SELECT \* FROM users".to_string(),
        body: "Use parameterized queries or ORM models instead of SELECT * FROM users.".to_string(),
        enabled: true,
        created_from: None,
        cooldown_turns: None,
    }];

    let mut coordinator = TtsrCoordinator::new(&rules, 3, 4096);
    coordinator.advance_turn(1);

    // Turn 1, Attempt 1: model triggers rule
    let act1 = coordinator.process_chunk(
        "Let us execute: SELECT * FROM users WHERE id = 1",
        StreamChannel::AssistantText,
    );
    let TtsrAction::AbortAndInject {
        rule,
        matched_excerpt,
        reminder_message,
    } = act1
    else {
        assert!(false, "Should abort and inject on first match");
        return;
    };
    assert_eq!(rule.id, "no-raw-sql");
    assert!(matched_excerpt.contains("SELECT * FROM users"));
    assert!(reminder_message.contains("[SYSTEM REMINDER: Violation of stream rule 'No Raw SQL']"));
    assert!(reminder_message.contains("Use parameterized queries"));

    // Turn 1, Attempt 2: model triggers rule again
    coordinator.reset_attempt();
    let act2 = coordinator.process_chunk(
        "Still querying: select * from users",
        StreamChannel::AssistantText,
    );
    assert!(matches!(act2, TtsrAction::AbortAndInject { .. }));

    // Turn 1, Attempt 3: model triggers rule again (injection #3 reaches the cap of 3)
    coordinator.reset_attempt();
    let act3 = coordinator.process_chunk("select * from users again", StreamChannel::AssistantText);
    assert!(matches!(act3, TtsrAction::AbortAndInject { .. }));

    // Turn 1, Attempt 4: model triggers rule again -> CapExceeded halts stream to notify user
    coordinator.reset_attempt();
    let act4 = coordinator.process_chunk(
        "select * from users once more",
        StreamChannel::AssistantText,
    );
    let TtsrAction::CapExceeded {
        rule: cap_rule,
        total_injections,
        ..
    } = act4
    else {
        assert!(false, "Should exceed turn cap and stop without looping");
        return;
    };
    assert_eq!(cap_rule.id, "no-raw-sql");
    assert_eq!(total_injections, 3);
}

#[test]
fn test_stream_rule_store_lifecycle_and_grievances() {
    let Ok(tmp) = tempdir() else {
        return;
    };
    let project_root = tmp.path();

    let mut store = StreamRuleStore::load_for_project(project_root);
    assert_eq!(store.list_all_rules().len(), 0);

    let rule = StreamRule {
        id: "no-magic-numbers".to_string(),
        name: "No Magic Numbers".to_string(),
        pattern: r"MAGIC_VALUE_99".to_string(),
        body: "Define named constants instead of magic literal values.".to_string(),
        enabled: true,
        created_from: None,
        cooldown_turns: Some(1),
    };

    let Ok(()) = store.add_rule(rule, false) else {
        assert!(false, "add_rule should succeed");
        return;
    };

    assert_eq!(store.list_all_rules().len(), 1);
    assert_eq!(store.list_project_rules().len(), 1);

    // Test export
    let Ok(exported_json) = store.export_json() else {
        assert!(false, "export_json should succeed");
        return;
    };
    assert!(exported_json.contains("no-magic-numbers"));

    // Reload from disk to verify persistence
    let reloaded = StreamRuleStore::load_for_project(project_root);
    assert_eq!(reloaded.list_project_rules().len(), 1);

    // Record grievance
    let Ok(grievance) = GrievancesLedger::record_complaint(
        project_root,
        "Model used magic literal MAGIC_VALUE_99 in unit tests",
        Some("no-magic-numbers"),
    ) else {
        assert!(false, "record_complaint should succeed");
        return;
    };

    assert_eq!(
        grievance.suggested_rule_id,
        Some("no-magic-numbers".to_string())
    );

    let Ok(grievances) = GrievancesLedger::list_grievances(project_root) else {
        assert!(false, "list_grievances should succeed");
        return;
    };
    assert_eq!(grievances.len(), 1);
    assert_eq!(grievances[0].id, grievance.id);

    // Forge candidate rule
    let forged = GrievancesLedger::forge_candidate_rule(&grievance);
    assert!(forged.body.contains("Avoid recurring issue"));
    assert_eq!(forged.created_from, Some(grievance.id));
}
