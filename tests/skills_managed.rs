//! Integration tests for `learn` + `manage_skill` (bd-cv653.4.2).
//!
//! Acceptance coverage:
//! 1. `learn(promote=true)` → a fresh skills discovery finds the managed
//!    skill in the dead-last tier.
//! 2. A user skill with the same name shadows the managed one; the
//!    collision diagnostic records it.
//! 3. `manage_skill` delete/update on content lacking the managed marker is
//!    refused (user-authored protection).
//! 4. Invalid promote draft → lesson stored, skill not written, warning.
//! 5. learn is gated behind memory.backend=local like the other bank tools.
//!
//! Isolation: unique pid-suffixed skill names in the real managed dir (the
//! same pattern as the module's unit tests — no env mutation, which this
//! crate forbids); created skills are deleted at test end.
//!
//! Logging: structured JSONL per tests/common/logging.rs, v2-validated,
//! recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::tools::{Tool, ToolOutput, ToolRegistry};
use serde_json::json;

fn first_text(output: &ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|block| match block {
            pi::model::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL v2 validation errors: {errors:?}");
}

fn block_on_local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future)
}

fn unique_name(tag: &str) -> String {
    format!("pi-it-{tag}-{}", std::process::id())
}

fn memory_config(backend: &str) -> pi::config::Config {
    pi::config::Config {
        memory: Some(pi::config::MemorySettings {
            backend: Some(backend.to_string()),
        }),
        ..Default::default()
    }
}

fn cleanup(name: &str) {
    let _ = pi::skills_managed::delete(name);
}

#[test]
fn promote_then_fresh_discovery_finds_managed_skill() {
    let case = "promote_then_fresh_discovery_finds_managed_skill";
    let harness = TestHarness::new(case);
    let cwd = harness.temp_path("proj");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let skill_name = unique_name("promote");

    let store = std::sync::Arc::new(pi::memory::MemoryStore::open(&cwd).expect("open"));
    let learn = pi::tools::LearnTool::new(store);
    let out = block_on_local(learn.execute(
        "call-1",
        json!({
            "lesson": "always run cargo check before committing",
            "promote": true,
            "skillName": skill_name
        }),
        None,
    ))
    .expect("learn");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("learn output: {text}"));
    assert!(text.contains("Promoted to managed skill"), "{text}");

    // Fresh discovery against the real agent dir: the managed tier sees it
    // with managed provenance.
    let agent_dir = pi::config::Config::global_dir();
    let loaded = pi::resources::load_skills(pi::resources::LoadSkillsOptions {
        cwd,
        agent_dir,
        skill_paths: Vec::new(),
        include_defaults: true,
    });
    let managed = loaded.skills.iter().find(|skill| skill.name == skill_name);
    harness.log().info(
        "verify",
        format!(
            "discovery: found={} source={:?}",
            managed.is_some(),
            managed.map(|skill| skill.source.as_str())
        ),
    );
    let managed = managed.expect("managed skill must be discovered");
    assert_eq!(
        managed.source, "managed",
        "provenance must be the managed tier"
    );
    cleanup(&skill_name);
    finish_case(&harness, case);
}

#[test]
fn user_skill_shadows_managed_with_diagnostic() {
    let case = "user_skill_shadows_managed_with_diagnostic";
    let harness = TestHarness::new(case);
    let cwd = harness.temp_path("proj");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let agent_dir = harness.temp_path("agent");
    let name = unique_name("shadow");

    // Managed skill in the harness agent dir.
    let managed_dir = agent_dir.join("skills.managed").join(&name);
    std::fs::create_dir_all(&managed_dir).expect("managed dir");
    std::fs::write(
        managed_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: managed version\nmanaged: true\n---\n\nbody\n"),
    )
    .expect("write managed");

    // User skill with the same name wins.
    let user_dir = agent_dir.join("skills").join(&name);
    std::fs::create_dir_all(&user_dir).expect("user dir");
    std::fs::write(
        user_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: user version\n---\n\nbody\n"),
    )
    .expect("write user");

    let loaded = pi::resources::load_skills(pi::resources::LoadSkillsOptions {
        cwd,
        agent_dir,
        skill_paths: Vec::new(),
        include_defaults: true,
    });
    let winner = loaded
        .skills
        .iter()
        .find(|skill| skill.name == name)
        .expect("winner");
    harness
        .log()
        .info("verify", format!("winner source: {}", winner.source));
    assert_eq!(
        winner.source, "user",
        "the user skill must shadow the managed one"
    );
    let collision = loaded
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains(&name));
    assert!(
        collision,
        "a collision diagnostic must name the shadowed skill: {:?}",
        loaded
            .diagnostics
            .iter()
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
    finish_case(&harness, case);
}

#[test]
fn manage_skill_refuses_unmanaged_content() {
    let case = "manage_skill_refuses_unmanaged_content";
    let harness = TestHarness::new(case);
    let name = unique_name("unmanaged");

    // Plant user-authored content inside the managed dir WITHOUT the marker.
    let dir = pi::skills_managed::managed_skills_dir().join(&name);
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: user authored\n---\n\nbody\n"),
    )
    .expect("write");

    let tool = pi::tools::ManageSkillTool;
    let out = block_on_local(tool.execute("call-1", json!({"op": "delete", "name": name}), None))
        .expect("execute");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("delete refusal: {text}"));
    assert!(out.is_error, "refusal must surface as is_error");
    assert!(text.contains("PI_SKILL_NOT_MANAGED"), "{text}");
    assert!(
        dir.join("SKILL.md").exists(),
        "user-authored content must survive the refused delete"
    );
    let _ = std::fs::remove_dir_all(&dir);
    finish_case(&harness, case);
}

#[test]
fn unusable_skill_name_falls_back_to_a_lesson_derived_name() {
    let case = "unusable_skill_name_falls_back_to_a_lesson_derived_name";
    let harness = TestHarness::new(case);
    let cwd = harness.temp_path("proj");
    std::fs::create_dir_all(&cwd).expect("cwd");

    let store = std::sync::Arc::new(pi::memory::MemoryStore::open(&cwd).expect("open"));
    let learn = pi::tools::LearnTool::new(std::sync::Arc::clone(&store));
    // A requested name that slugifies to nothing is unusable, and the learn
    // tool documents that the skill name is then derived from the lesson.
    // (The previous fixture, "INVALID NAME!!", slugifies to the valid
    // "invalid-name" and its "promotion skipped" expectation only held on
    // hosts that already carried a leftover managed skill of that name.)
    let derived = "a-lesson-with-an-impossible-skill-name";
    let _ = pi::skills_managed::delete(derived);
    let input = json!({
        "lesson": "a lesson with an impossible skill name",
        "promote": true,
        "skillName": "!!!"
    });
    let out = block_on_local(learn.execute("call-1", input, None)).expect("learn");
    let text = first_text(&out);
    harness
        .log()
        .info("verify", format!("unusable-name promote: {text}"));
    assert!(text.contains("Lesson captured"), "{text}");
    assert!(
        text.contains(&format!("Promoted to managed skill '{derived}'")),
        "the lesson-derived name must be used: {text}"
    );
    let listed = pi::skills_managed::list().expect("list");
    assert!(
        listed.iter().any(|skill| skill.name == derived),
        "derived skill must be written: {listed:?}"
    );
    assert!(
        listed
            .iter()
            .all(|skill| skill.name != "!!!" && skill.name != "lesson"),
        "neither the raw name nor the placeholder may be written: {listed:?}"
    );

    // Promoting a lesson that derives the same skill name is refused (the
    // derived skill already exists) while the lesson itself is still
    // captured. The wording differs by trailing punctuation only: the memory
    // store rejects an identical lesson before promotion is attempted, and
    // slugification drops the punctuation, so the derived name is the same.
    let repeat = json!({
        "lesson": "a lesson with an impossible skill name!",
        "promote": true,
        "skillName": "!!!"
    });
    let again = block_on_local(learn.execute("call-2", repeat, None)).expect("learn again");
    let again_text = first_text(&again);
    harness
        .log()
        .info("verify", format!("repeat promote: {again_text}"));
    assert!(
        again_text.contains("Skill promotion skipped") && again_text.contains("Lesson captured"),
        "a refused promotion must keep the lesson: {again_text}"
    );

    let hits = store.recall("impossible skill name", None).expect("recall");
    assert!(!hits.is_empty(), "lesson must be kept: {hits:?}");
    let _ = pi::skills_managed::delete(derived);
    finish_case(&harness, case);
}

#[test]
fn learn_is_gated_with_the_bank() {
    let case = "learn_is_gated_with_the_bank";
    let harness = TestHarness::new(case);
    let cwd = harness.temp_path("proj");
    std::fs::create_dir_all(&cwd).expect("cwd");

    let local = ToolRegistry::new(&["read"], &cwd, Some(&memory_config("local")));
    let local_names: Vec<&str> = local.tools().iter().map(|tool| tool.name()).collect();
    assert!(
        local_names.contains(&"learn"),
        "backend=local must expose learn: {local_names:?}"
    );
    assert!(
        local_names.contains(&"manage_skill"),
        "manage_skill is always available: {local_names:?}"
    );

    let off = ToolRegistry::new(&["read"], &cwd, Some(&memory_config("off")));
    let off_names: Vec<&str> = off.tools().iter().map(|tool| tool.name()).collect();
    assert!(
        !off_names.contains(&"learn"),
        "backend=off must hide learn: {off_names:?}"
    );
    assert!(
        off_names.contains(&"manage_skill"),
        "manage_skill stays available when the bank is off: {off_names:?}"
    );
    finish_case(&harness, case);
}

#[test]
fn manage_skill_crud_through_tool() {
    let case = "manage_skill_crud_through_tool";
    let harness = TestHarness::new(case);
    let name = unique_name("crud");
    let tool = pi::tools::ManageSkillTool;

    let created = block_on_local(tool.execute(
        "call-1",
        json!({"op": "create", "name": name, "description": "crud skill", "content": "body"}),
        None,
    ))
    .expect("create");
    let created_text = first_text(&created);
    harness
        .log()
        .info("verify", format!("create: {created_text}"));
    assert!(created_text.contains("created"), "{created_text}");

    let listed = block_on_local(tool.execute("call-1", json!({"op": "list"}), None)).expect("list");
    let listed_text = first_text(&listed);
    assert!(listed_text.contains(&name), "{listed_text}");

    let deleted =
        block_on_local(tool.execute("call-1", json!({"op": "delete", "name": name}), None))
            .expect("delete");
    assert!(first_text(&deleted).contains("deleted"));
    cleanup(&name);
    finish_case(&harness, case);
}

#[test]
fn audit_ledger_records_mutations() {
    let case = "audit_ledger_records_mutations";
    let harness = TestHarness::new(case);
    let name = unique_name("audit");
    pi::skills_managed::create(&name, "audit skill", "body").expect("create");
    pi::skills_managed::update(&name, None, "body two").expect("update");
    pi::skills_managed::delete(&name).expect("delete");

    let ledger = pi::skills_managed::managed_skills_dir().join("audit.jsonl");
    let content = std::fs::read_to_string(&ledger).expect("read ledger");
    let ops: Vec<String> = content
        .lines()
        .filter(|line| line.contains(&name))
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v["op"].as_str().map(str::to_string))
                .unwrap_or_default()
        })
        .collect();
    harness
        .log()
        .info("verify", format!("audit ops for {name}: {ops:?}"));
    assert_eq!(ops, vec!["create", "update", "delete"]);
    finish_case(&harness, case);
}
