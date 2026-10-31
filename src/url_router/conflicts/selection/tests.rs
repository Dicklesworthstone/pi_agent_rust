use super::super::tests::{BLOCK, fixture};
use super::*;
use serde_json::json;

fn choice(snapshot: &Snapshot, index: usize, resolution: ConflictResolution) -> ConflictChoice {
    ConflictChoice {
        index,
        token: snapshot.regions[index].token.clone(),
        resolution,
    }
}

#[test]
fn selection_shapes_and_budgets_are_explicit_before_worktree_access() {
    let valid = ConflictChoice {
        index: 0,
        token: "a".repeat(64),
        resolution: ConflictResolution::Text("  雪\n".into()),
    };
    let value = serde_json::to_value(&valid).unwrap();
    let decoded: ConflictChoice = serde_json::from_value(value).unwrap();
    assert!(matches!(decoded.resolution, ConflictResolution::Text(ref text) if text == "  雪\n"));
    assert!(validate(std::slice::from_ref(&valid)).is_ok());
    for value in [
        json!({"index":0,"token":"a","resolution":null}),
        json!({"index":0,"token":"a","resolution":{"kind":"shell","value":"ignored"}}),
        json!({"index":0,"token":"a","resolution":{"kind":"ours"},"path":"elsewhere"}),
    ] {
        assert!(serde_json::from_value::<ConflictChoice>(value).is_err());
    }
    assert!(validate(&[]).is_err());
    assert!(validate(&vec![valid.clone(); MAX_CHOICES + 1]).is_err());
    assert!(validate(&[valid.clone(), valid.clone()]).is_err());
    for token in ["A".repeat(64), "a".repeat(63), "../target".into()] {
        assert!(
            validate(&[ConflictChoice {
                token,
                ..valid.clone()
            }])
            .is_err()
        );
    }
    assert!(
        validate(&[ConflictChoice {
            resolution: ConflictResolution::Text("x".repeat(MAX_REPLACEMENT_BYTES + 1)),
            ..valid
        }])
        .is_err()
    );
}

#[test]
fn slash_legacy_and_guarded_read_references_have_one_strict_grammar() {
    for reference in ["0/ours", "0 @ours"] {
        let parsed = parse_read(reference).unwrap();
        assert_eq!(parsed.index, Some(0));
        assert_eq!(parsed.side, "ours");
    }
    let token = "a".repeat(64);
    let reference = format!("2/{token}/base");
    let parsed = parse_read(&reference).unwrap();
    assert_eq!(parsed.token, Some(token.as_str()));
    for reference in [
        "-1",
        "0/nope",
        "0/ours @theirs",
        "*/ours",
        "0//ours",
        "0?x=1",
    ] {
        assert!(parse_read(reference).is_err(), "{reference}");
    }
}

#[test]
fn guarded_reads_and_reverse_order_batch_splices_preserve_literal_replacement() {
    let text = format!("prefix\n{BLOCK}between\n{BLOCK}tail");
    let dir = fixture("f.txt", &text);
    let snapshot = Snapshot::read(dir.path()).unwrap();
    let url = format!("conflict://1/{}/theirs", snapshot.regions[1].token);
    assert_eq!(
        crate::url_router::resolve(&url, dir.path())
            .unwrap()
            .content,
        "theirs 雪\n"
    );
    let choices = [
        choice(
            &snapshot,
            1,
            ConflictResolution::Text("  merged 😀\r\n".into()),
        ),
        choice(&snapshot, 0, ConflictResolution::Theirs),
    ];
    let selected = crate::url_router::apply_conflict_choices(dir.path(), &choices).unwrap();
    assert_eq!(
        selected
            .iter()
            .map(|region| region.index)
            .collect::<Vec<_>>(),
        vec![1, 0]
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "prefix\ntheirs 雪\nbetween\n  merged 😀\r\ntail"
    );
    assert!(crate::url_router::resolve(&url, dir.path()).is_err());
}

#[test]
fn unchanged_literal_conflict_writes_do_not_supply_prewalk_mutation_evidence() {
    use crate::tools::Tool as _;
    let original = format!("prefix\n{BLOCK}tail\n");
    let dir = fixture("f.txt", &original);
    let snapshot = Snapshot::read(dir.path()).unwrap();
    let located = &snapshot.regions[0];
    let unchanged = &snapshot.files[located.file].text[located.region.span.clone()];
    let url = format!("conflict://0/{}", located.token);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let tool = crate::tools::WriteTool::new(dir.path());
    let mut state = crate::prewalk::PrewalkState::new(
        crate::prewalk::PrewalkTarget {
            provider: "test".into(),
            model: "implementation".into(),
            thinking: None,
        },
        true,
    );
    let args = json!({"path":url,"content":unchanged});
    let output = runtime
        .block_on(tool.execute("unchanged", args.clone(), None))
        .unwrap();
    assert!(!output.is_error);
    let details = output.details.as_ref().unwrap();
    assert_eq!(details["paths"].as_array().unwrap().len(), 1);
    assert_eq!(details["changedPaths"], json!([]));
    state.observe(tool.prewalk_evidence(&args, &output));
    assert!(!state.ready());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        original
    );

    // An unchanged replacement preserves the guarded snapshot. Applying a
    // genuinely different literal then provides the committed-image receipt.
    let args = json!({"path":url,"content":"merged\n"});
    let output = runtime
        .block_on(tool.execute("changed", args.clone(), None))
        .unwrap();
    assert!(!output.is_error);
    assert_eq!(
        output.details.as_ref().unwrap()["changedPaths"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    state.observe(tool.prewalk_evidence(&args, &output));
    assert!(state.ready());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "prefix\nmerged\ntail\n"
    );
}

#[test]
fn one_stale_or_invalid_choice_prevents_every_replacement() {
    let text = format!("{BLOCK}gap\n{BLOCK}");
    let dir = fixture("f.txt", &text);
    let snapshot = Snapshot::read(dir.path()).unwrap();
    let good = choice(&snapshot, 0, ConflictResolution::Ours);
    let mut stale = choice(&snapshot, 1, ConflictResolution::Theirs);
    stale.token = "0".repeat(64);
    assert!(
        apply_conflict_choices(dir.path(), &[good.clone(), stale])
            .unwrap_err()
            .to_string()
            .contains("STALE")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        text
    );
    assert!(apply_conflict_choices(dir.path(), &[good.clone(), good]).is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        text
    );
}

#[test]
fn renumbered_regions_never_inherit_an_older_choice() {
    let text = format!("{BLOCK}gap\n{BLOCK}");
    let dir = fixture("f.txt", &text);
    let snapshot = Snapshot::read(dir.path()).unwrap();
    let stale = choice(&snapshot, 0, ConflictResolution::Ours);
    crate::url_router::write_conflict_resolution(dir.path(), 0, "theirs").unwrap();
    let remaining = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert!(
        apply_conflict_choices(dir.path(), &[stale])
            .unwrap_err()
            .to_string()
            .contains("STALE")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        remaining
    );
}

#[test]
fn empty_authored_text_is_explicit_deletion_not_an_absent_base() {
    let dir = fixture("f.txt", BLOCK);
    let snapshot = Snapshot::read(dir.path()).unwrap();
    apply_conflict_choices(
        dir.path(),
        &[choice(
            &snapshot,
            0,
            ConflictResolution::Text(String::new()),
        )],
    )
    .unwrap();
    assert_eq!(std::fs::read(dir.path().join("f.txt")).unwrap(), b"");
}

#[test]
fn multi_file_choices_validate_together_without_staging_the_index() {
    use std::io::Write as _;
    let dir = fixture("a.txt", BLOCK);
    let root = dir.path();
    // Copy the actual fixture's three unmerged stages to a second literal
    // path. Only this temporary test index is edited to prepare the fixture.
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--unmerged", "-z", "--", "a.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut records = Vec::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record.iter().position(|byte| *byte == b'\t').unwrap();
        records.extend_from_slice(&record[..tab]);
        records.extend_from_slice(b"\tb.txt\0");
    }
    let mut child = std::process::Command::new("git")
        .current_dir(root)
        .args(["update-index", "-z", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&records).unwrap();
    assert!(child.wait().unwrap().success());
    std::fs::write(root.join("b.txt"), BLOCK).unwrap();
    let index = std::fs::read(root.join(".git/index")).unwrap();
    let snapshot = Snapshot::read(root).unwrap();
    let choices = [
        choice(&snapshot, 0, ConflictResolution::Ours),
        choice(&snapshot, 1, ConflictResolution::Base),
    ];
    apply_conflict_choices(root, &choices).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "ours 😀\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
}

#[test]
fn cancelled_owner_cannot_publish_a_previously_prepared_choice() {
    let dir = fixture("f.txt", BLOCK);
    let snapshot = Snapshot::read(dir.path()).unwrap();
    let choices = [choice(&snapshot, 0, ConflictResolution::Theirs)];
    let owner = crate::agent_cx::AgentCx::for_request();
    owner.set_cancel_requested(true);
    let result = futures::executor::block_on(
        owner.with_current(async { apply_conflict_choices(dir.path(), &choices) }),
    );
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        BLOCK
    );
}
