#![allow(clippy::too_many_lines)]
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{Error as IoError, ErrorKind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use pi::validation_broker::{
    VALIDATION_BROKER_DECISION_SCHEMA, VALIDATION_BROKER_INPUT_SCHEMA,
    VALIDATION_BROKER_SLOT_RECORD_SCHEMA, VALIDATION_BROKER_SLOT_SCHEMA,
    VALIDATION_BROKER_SLOT_STORE_SCHEMA, VALIDATION_BROKER_STRESS_EVIDENCE_SCHEMA,
    ValidationAdmissionDecision, ValidationAdmissionPolicy, ValidationAdmissionRequestContext,
    ValidationBrokerInputParts, ValidationBrokerInputSnapshot, ValidationBrokerStressBudgets,
    ValidationBrokerStressProfile, ValidationBrokerStressVerdict, ValidationRejectedReusableSlot,
    ValidationSlotArtifact, ValidationSlotLease, ValidationSlotRequest, ValidationSlotState,
    ValidationSlotStore, ValidationSlotStoreSnapshot, ValidationSlotStoreStatus,
    ValidationSourceProvenance, ValidationSourceState, decide_validation_admission,
    evaluate_validation_broker_stress_budget, normalize_available_source, normalize_beads_json,
    normalize_doctor_json, normalize_git_status_text, normalize_headroom_json,
    normalize_rch_queue_text, normalize_unavailable_source,
};
use serde::Deserialize;
use serde_json::{Value, json};

type TestResult = Result<(), String>;

const START: &str = "2026-05-14T07:00:00Z";
const HEARTBEAT: &str = "2026-05-14T07:05:00Z";
const EXPIRES: &str = "2026-05-14T07:30:00Z";
const RENEWED_EXPIRES: &str = "2026-05-14T08:00:00Z";
const STALE_AT: &str = "2026-05-14T08:30:00Z";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_root(label: &str) -> Result<PathBuf, String> {
    let mut root = std::env::var("TMPDIR").map_or_else(|_| std::env::temp_dir(), PathBuf::from);
    root.push("pi_validation_broker_tests");
    std::fs::create_dir_all(&root).map_err(|err| format!("create temp parent: {err}"))?;

    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut candidate_name = String::with_capacity(label.len() + 24);
    for offset in 0..10_000 {
        candidate_name.clear();
        candidate_name.push_str(label);
        candidate_name.push('_');
        write!(&mut candidate_name, "{}", unique + offset)
            .map_err(|err| format!("write temp candidate name: {err}"))?;
        let candidate = root.join(&candidate_name);
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => return Err(temp_root_create_error(&err)),
        }
    }

    Err("create temp root: exhausted deterministic candidates".to_string())
}

fn temp_root_create_error(err: &IoError) -> String {
    format!("create temp root: {err}")
}

#[cfg(unix)]
#[test]
fn reuse_binds_workspace_and_executable_bytes_before_and_after_validation() -> TestResult {
    let root = temp_root("freshness")?;
    let tool = temp_root("freshness_tool")?.join("validator.exe");
    std::fs::write(&tool, b"validator v1").map_err(to_string)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
            .map_err(to_string)?;
    }
    let mut request = base_request("freshness");
    request.cwd = root.to_string_lossy().into_owned();
    request.command = vec![tool.to_string_lossy().into_owned()];
    let artifacts = vec![ValidationSlotArtifact {
        path: "result.json".into(),
        sha256: Some("result".into()),
        schema: Some("test.v1".into()),
    }];
    for path in ["source.rs", "Cargo.toml", "Cargo.lock"] {
        let input = root.join(path); // ubs:ignore[rust.security.path-traversal] Only the three literal fixture filenames above enter this join.
        std::fs::write(&input, b"original").map_err(to_string)?;
        let mut lease = ValidationSlotLease::acquire(request.clone(), START, EXPIRES) // ubs:ignore[rust.collections.clone-in-loop] Each of three fixed regression cases needs independently owned lease input.
            .map_err(to_string)?;
        lease
            .mark_reusable("SilentReef", HEARTBEAT, artifacts.clone()) // ubs:ignore[rust.collections.clone-in-loop] A one-item artifact fixture is consumed independently by each fixed case.
            .map_err(to_string)?;
        require(
            lease
                .matches_request_equivalence(&request)
                .map_err(to_string)?,
            "unchanged bytes reuse",
        )?;
        std::fs::write(&input, b"modified").map_err(to_string)?;
        require(
            !lease
                .matches_request_equivalence(&request)
                .map_err(to_string)?,
            "same HEAD with changed bytes cannot reuse",
        )?;
        require(
            lease
                .mark_reusable("SilentReef", HEARTBEAT, artifacts.clone()) // ubs:ignore[rust.collections.clone-in-loop] The ownership-taking failure path needs the same one-item fixture in each fixed case.
                .is_err(),
            "changed during validation cannot become reusable",
        )?;
        std::fs::write(&input, b"original").map_err(to_string)?;
    }
    let lease = ValidationSlotLease::acquire(request.clone(), START, EXPIRES).map_err(to_string)?;
    std::fs::create_dir(root.join("empty_input_directory")).map_err(to_string)?;
    require(
        !lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "empty input directory creation invalidates validation",
    )?;
    let mut lease =
        ValidationSlotLease::acquire(request.clone(), START, EXPIRES).map_err(to_string)?;
    std::fs::write(&tool, b"validator v2").map_err(to_string)?;
    require(
        !lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "external executable changes cannot reuse",
    )?;
    require(
        lease
            .mark_reusable("SilentReef", HEARTBEAT, artifacts.clone())
            .is_err(),
        "tool changed during validation",
    )?;
    let mut legacy = serde_json::to_value(&lease).map_err(to_string)?;
    legacy
        .as_object_mut()
        .ok_or("lease object")?
        .remove("input_identity");
    lease = serde_json::from_value(legacy).map_err(to_string)?;
    require(
        !lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "legacy identity cannot reuse",
    )?;
    require(
        lease
            .mark_reusable("SilentReef", HEARTBEAT, artifacts)
            .is_err(),
        "legacy identity cannot become reusable",
    )
}

#[test]
fn unavailable_identity_is_never_reusable() -> TestResult {
    let mut request = base_request("unavailable-identity");
    request.command = vec!["unavailable-validator".into()];
    let mut lease =
        ValidationSlotLease::acquire(request.clone(), START, EXPIRES).map_err(to_string)?;
    require(
        lease.input_identity.is_none(),
        "unresolvable tool has no identity",
    )?;
    require(
        !lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "unavailable identity fails closed",
    )?;
    require(
        lease
            .mark_reusable(
                "SilentReef",
                HEARTBEAT,
                vec![ValidationSlotArtifact {
                    path: "result.json".into(),
                    sha256: None,
                    schema: None,
                }],
            )
            .is_err(),
        "missing inputs cannot become reusable",
    )
}

#[cfg(not(unix))]
#[test]
fn unsupported_native_capture_refuses_reuse_and_preserves_active_ownership() -> TestResult {
    let root = temp_root("unsupported_capture")?;
    let mut request = base_request("unsupported-native");
    request.cwd = root.to_string_lossy().into_owned();
    // This is an existing native executable, not an unresolvable-command case.
    request.command = vec![
        std::env::current_exe()
            .map_err(to_string)?
            .to_string_lossy()
            .into_owned(),
    ];
    request.command_class = "custom_validation".into();
    let mut lease =
        ValidationSlotLease::acquire(request.clone(), START, EXPIRES).map_err(to_string)?;
    require(
        lease.input_identity.is_none(),
        "unsupported capture remains explicitly unavailable",
    )?;
    let before = lease.clone();
    require(
        lease
            .mark_reusable("SilentReef", HEARTBEAT, reusable_fault_artifacts())
            .is_err(),
        "native executable presence does not invent input identity",
    )?;
    require(lease == before, "failed reuse preserves active lease")?;
    let store = ValidationSlotStore::new(temp_root("unsupported_store")?.join("slots.jsonl"));
    store
        .append_lease("active", START, &lease)
        .map_err(to_string)?;
    let decision = decide_validation_admission(
        admission_context_for("unsupported-native-active", request, START, 4),
        &healthy_inputs()?,
        &store.load_snapshot(),
        &ValidationAdmissionPolicy::default(),
        HEARTBEAT,
    )
    .map_err(to_string)?;
    require(
        decision.decision == ValidationAdmissionDecision::Wait,
        "unsupported capture still preserves active ownership",
    )?;
    require(
        decision.reusable_slot.is_none() && decision.coalesced_artifacts.is_empty(),
        "unsupported capture cannot supply reused artifacts",
    )
}

#[cfg(unix)]
fn freshness_fixture(label: &str) -> Result<(PathBuf, PathBuf, ValidationSlotRequest), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let root = temp_root(label)?;
    let tool = temp_root(&format!("{label}_tool"))?.join("validator");
    std::fs::write(&tool, b"#!/bin/sh\nexit 0\n").map_err(to_string)?;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).map_err(to_string)?;
    let mut request = base_request(label);
    request.cwd = root.to_string_lossy().into_owned();
    request.command = vec![
        tool.to_string_lossy().into_owned(),
        "--test".into(),
        "focused".into(),
    ];
    request.command_class = "custom_validation".into();
    request.target_dir = root.join("target").to_string_lossy().into_owned();
    request.tmpdir = root.join("tmp").to_string_lossy().into_owned();
    Ok((root, tool, request))
}

#[cfg(unix)]
fn persisted_freshness_lease(
    request: &ValidationSlotRequest,
) -> Result<ValidationSlotLease, String> {
    let mut lease =
        ValidationSlotLease::acquire(request.clone(), START, EXPIRES).map_err(to_string)?;
    lease
        .mark_reusable("SilentReef", HEARTBEAT, reusable_fault_artifacts())
        .map_err(to_string)?;
    let store = ValidationSlotStore::new(temp_root("freshness_store")?.join("slots.jsonl"));
    store
        .append_lease("reusable", HEARTBEAT, &lease)
        .map_err(to_string)?;
    let mut snapshot = ValidationSlotStore::new(store.path()).load_snapshot();
    require(
        !snapshot.is_degraded(),
        "freshness lease reloads without degradation",
    )?;
    snapshot
        .latest_by_slot_id
        .remove(&request.slot_id)
        .ok_or_else(|| "reloaded lease missing".into())
}

#[cfg(unix)]
#[test]
fn persisted_freshness_tracks_renames_permissions_and_external_executable_identity() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let (root, tool, request) = freshness_fixture("identity_changes")?;
    let source = root.join("src");
    std::fs::create_dir(&source).map_err(to_string)?;
    std::fs::write(source.join("input.rs"), "original input").map_err(to_string)?;
    let lease = persisted_freshness_lease(&request)?;
    require(
        lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "unchanged recovered evidence matches",
    )?;

    let renamed = root.join("renamed");
    std::fs::rename(&source, &renamed).map_err(to_string)?;
    require(
        !lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "directory rename invalidates recovered evidence",
    )?;
    std::fs::rename(&renamed, &source).map_err(to_string)?;
    require(
        lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "identical restored names and contents match",
    )?;

    for path in [source.clone(), source.join("input.rs"), tool.clone()] {
        let permissions = std::fs::metadata(&path).map_err(to_string)?.permissions();
        let changed = permissions.mode() ^ 0o010;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(changed))
            .map_err(to_string)?;
        require(
            !lease
                .matches_request_equivalence(&request)
                .map_err(to_string)?,
            "mode-only changes invalidate evidence",
        )?;
        std::fs::set_permissions(&path, permissions).map_err(to_string)?;
        require(
            lease
                .matches_request_equivalence(&request)
                .map_err(to_string)?,
            "restored permissions match",
        )?;
    }

    let old_tool = tool.with_extension("saved");
    std::fs::rename(&tool, &old_tool).map_err(to_string)?;
    std::fs::write(&tool, b"#!/bin/sh\nexit 1\n").map_err(to_string)?;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).map_err(to_string)?;
    require(
        !lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "atomic external executable replacement invalidates evidence",
    )?;
    let mut recovered = lease;
    require(
        recovered
            .mark_reusable("SilentReef", HEARTBEAT, reusable_fault_artifacts())
            .is_err(),
        "changed executable cannot republish recovered evidence",
    )?;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).map_err(to_string)?;
    let unavailable = ValidationSlotLease::acquire(request, START, EXPIRES).map_err(to_string)?;
    require(
        unavailable.input_identity.is_none(),
        "non-executable candidate cannot establish identity",
    )
}

#[cfg(unix)]
#[test]
fn symlink_substitutions_and_oversized_inputs_fail_closed_after_restart() -> TestResult {
    let (root, _tool, request) = freshness_fixture("symlink_inputs")?;
    let source = root.join("src");
    std::fs::create_dir(&source).map_err(to_string)?;
    std::fs::write(source.join("input.rs"), "same contents").map_err(to_string)?;
    let mut lease = persisted_freshness_lease(&request)?;
    let outside = temp_root("outside_input")?;
    std::fs::write(outside.join("input.rs"), "same contents").map_err(to_string)?;
    std::fs::rename(&source, root.join("original_src")).map_err(to_string)?;
    std::os::unix::fs::symlink(&outside, &source).map_err(to_string)?;
    require(
        !lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "ancestor symlink never reuses identical outside bytes",
    )?;
    let before = lease.clone();
    require(
        lease
            .mark_reusable("SilentReef", HEARTBEAT, reusable_fault_artifacts())
            .is_err(),
        "symlink capture cannot become reusable",
    )?;
    require(
        lease == before,
        "failed freshness check leaves recovered lease unchanged",
    )?;
    require(
        std::fs::read_to_string(outside.join("input.rs")).map_err(to_string)? == "same contents",
        "outside source remains unchanged",
    )?;

    let (large_root, _, large_request) = freshness_fixture("bounded_inputs")?;
    std::fs::File::create(large_root.join("large.input"))
        .map_err(to_string)?
        .set_len(512 * 1024 * 1024 + 1)
        .map_err(to_string)?;
    let mut bounded =
        ValidationSlotLease::acquire(large_request, START, EXPIRES).map_err(to_string)?;
    require(
        bounded.input_identity.is_none(),
        "oversized sparse input cannot establish identity",
    )?;
    require(
        bounded
            .mark_reusable("SilentReef", HEARTBEAT, reusable_fault_artifacts())
            .is_err(),
        "oversized capture cannot publish reusable evidence",
    )
}

#[test]
fn unavailable_and_legacy_identity_preserve_active_ownership_but_never_reuse() -> TestResult {
    let mut request = base_request("unavailable_active");
    request.command = vec![
        "unavailable-validator".into(),
        "--test".into(),
        "focused".into(),
    ];
    request.command_class = "custom_validation".into();
    let active =
        ValidationSlotLease::acquire(request.clone(), START, EXPIRES).map_err(to_string)?;
    require(
        active.input_identity.is_none(),
        "fixture has unavailable executable identity",
    )?;
    for legacy in [false, true] {
        let mut value = serde_json::to_value(&active).map_err(to_string)?;
        if legacy {
            value
                .as_object_mut()
                .ok_or("lease object")?
                .remove("input_identity");
        }
        let lease: ValidationSlotLease = serde_json::from_value(value).map_err(to_string)?;
        let store = ValidationSlotStore::new(temp_root("active_identity")?.join("slots.jsonl"));
        store
            .append_lease("active", START, &lease)
            .map_err(to_string)?;
        let snapshot = ValidationSlotStore::new(store.path()).load_snapshot();
        let decision = decide_validation_admission(
            admission_context_for("active_without_identity", request.clone(), START, 4), // ubs:ignore[rust.collections.clone-in-loop] Two current/legacy fixtures each consume an independently owned admission request.
            &healthy_inputs()?,
            &snapshot,
            &ValidationAdmissionPolicy::default(),
            HEARTBEAT,
        )
        .map_err(to_string)?;
        require(
            decision.decision == ValidationAdmissionDecision::Wait,
            "unknown input identity never authorizes duplicate active work",
        )?;
        require(
            decision.policy.active_equivalent_slots == 1,
            "active owner remains visible",
        )?;

        let mut legacy_result = lease;
        legacy_result.state = ValidationSlotState::Reusable;
        legacy_result.artifacts = reusable_fault_artifacts();
        store
            .append_lease("legacy_result", HEARTBEAT, &legacy_result)
            .map_err(to_string)?;
        let policy = ValidationAdmissionPolicy {
            reuse_required: true,
            ..ValidationAdmissionPolicy::default()
        };
        let decision = decide_validation_admission(
            admission_context_for("result_without_identity", request.clone(), START, 4), // ubs:ignore[rust.collections.clone-in-loop] Each of two persisted-result cases consumes an independent fixture request.
            &healthy_inputs()?,
            &store.load_snapshot(),
            &policy,
            HEARTBEAT,
        )
        .map_err(to_string)?;
        require(
            decision.decision == ValidationAdmissionDecision::DegradedBlock,
            "legacy result is never reusable evidence",
        )?;
        require(
            decision.rejected_reusable_slots.iter().any(|slot| {
                slot.reasons
                    .iter()
                    .any(|reason| reason == "input_identity_unavailable_or_changed")
            }),
            "unavailable provenance is explicit",
        )?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn source_paths_cannot_be_excluded_as_validation_outputs() -> TestResult {
    let (root, _, mut request) = freshness_fixture("output_exclusions")?;
    std::fs::create_dir(root.join("src")).map_err(to_string)?;
    std::fs::write(root.join("src/input.rs"), "source").map_err(to_string)?;
    for invalid in [root.clone(), root.join("src"), root.join("src/input.rs")] {
        request.target_dir = invalid.to_string_lossy().into_owned();
        let lease = ValidationSlotLease::acquire(request.clone(), START, EXPIRES) // ubs:ignore[rust.collections.clone-in-loop] Three invalid-output cases each need an independently owned fixture request.
            .map_err(to_string)?;
        require(
            lease.input_identity.is_none(),
            "untrusted output exclusion refuses input capture",
        )?;
    }
    request.target_dir = root.join("target").to_string_lossy().into_owned();
    let lease = persisted_freshness_lease(&request)?;
    std::fs::create_dir(root.join("target")).map_err(to_string)?;
    std::fs::write(root.join("target/result.json"), "generated output").map_err(to_string)?;
    require(
        lease
            .matches_request_equivalence(&request)
            .map_err(to_string)?,
        "declared generated outputs do not invalidate source bytes",
    )
}

#[cfg(target_os = "linux")]
#[test]
fn admission_scans_only_eligible_reuse_after_authority_checks() -> TestResult {
    use rustix::fs::inotify;
    use std::mem::MaybeUninit;
    let (root, _, request) = freshness_fixture("scan_scope")?;
    std::fs::write(root.join("input.rs"), "validation input").map_err(to_string)?;
    let reusable = persisted_freshness_lease(&request)?;
    for scenario in [
        "active",
        "terminal",
        "unrelated",
        "doctor_failed",
        "store_degraded",
        "local_fallback",
        "eligible",
    ] {
        let mut lease = reusable.clone(); // ubs:ignore[rust.collections.clone-in-loop] Seven scan-admission cases mutate independent copies of the baseline lease.
        let mut inputs = healthy_inputs()?;
        match scenario {
            "active" => lease.state = ValidationSlotState::Active,
            "terminal" => lease.state = ValidationSlotState::Released,
            "unrelated" => lease.git_head = "different-head".into(),
            "doctor_failed" => inputs.doctor.has_failures = true,
            "local_fallback" => inputs.rch.local_fallback = true,
            _ => {}
        }
        let mut snapshot = slot_snapshot(vec![lease]);
        if scenario == "store_degraded" {
            snapshot.status = ValidationSlotStoreStatus::Degraded;
        }
        let events = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
            .map_err(to_string)?;
        inotify::add_watch(
            &events,
            &root,
            inotify::WatchFlags::OPEN | inotify::WatchFlags::ACCESS,
        )
        .map_err(to_string)?;
        let decision = decide_validation_admission(
            admission_context_for(scenario, request.clone(), START, 4), // ubs:ignore[rust.collections.clone-in-loop] Each of seven scan-admission cases consumes its own fixture request.
            &inputs,
            &snapshot,
            &ValidationAdmissionPolicy::default(),
            HEARTBEAT,
        )
        .map_err(to_string)?;
        let mut buffer = [MaybeUninit::uninit(); 512];
        let mut reader = inotify::Reader::new(&events, &mut buffer);
        if scenario == "eligible" {
            require(
                decision.decision == ValidationAdmissionDecision::Coalesce,
                "eligible evidence coalesces",
            )?;
            require(
                reader.next().is_ok(),
                "positive control observed actual workspace read",
            )?;
        } else {
            require(
                matches!(reader.next(), Err(rustix::io::Errno::WOULDBLOCK)),
                "irrelevant or blocked plans do not read workspace inputs",
            )?;
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn inherited_path_changes_are_checked_at_completion() -> TestResult {
    const CHILD: &str = "PI_VALIDATION_PATH_CHILD";
    const REQUEST: &str = "PI_VALIDATION_PATH_REQUEST";
    const STORE: &str = "PI_VALIDATION_PATH_STORE";
    if let Ok(stage) = std::env::var(CHILD) {
        let request: ValidationSlotRequest = serde_json::from_slice(
            &std::fs::read(std::env::var(REQUEST).map_err(to_string)?).map_err(to_string)?,
        )
        .map_err(to_string)?;
        let store = ValidationSlotStore::new(std::env::var(STORE).map_err(to_string)?);
        let inherited = !request.environment.contains_key("PATH");
        if stage == "acquire" {
            let lease = ValidationSlotLease::acquire(request, START, EXPIRES).map_err(to_string)?;
            require(
                lease
                    .input_identity
                    .as_ref()
                    .is_some_and(|identity| identity.executable_search_path_inherited == inherited),
                "capture records explicit versus inherited PATH",
            )?;
            return store
                .append_lease("active", START, &lease)
                .map_err(to_string);
        }
        let mut lease = store
            .load_snapshot()
            .latest_by_slot_id
            .remove(&request.slot_id)
            .ok_or("persisted PATH lease missing")?;
        let before = lease.clone();
        let result = lease.mark_reusable("SilentReef", HEARTBEAT, reusable_fault_artifacts());
        if stage == "changed" && inherited {
            require(
                result.is_err(),
                "inherited PATH drift must block completion",
            )?;
            require(lease == before, "PATH drift leaves lease unchanged")
        } else {
            result.map_err(to_string)
        }
    } else {
        use std::os::unix::fs::PermissionsExt as _;
        let (root, tool, mut request) = freshness_fixture("path_provenance")?;
        std::fs::write(root.join("input.rs"), "source").map_err(to_string)?;
        let first_path = tool.parent().ok_or("tool parent")?;
        let second_path = temp_root("path_alternative")?;
        std::fs::write(second_path.join("validator"), b"#!/bin/sh\nexit 1\n").map_err(to_string)?;
        std::fs::set_permissions(
            second_path.join("validator"),
            std::fs::Permissions::from_mode(0o755),
        )
        .map_err(to_string)?;
        request.command[0] = "validator".into();
        let evidence = temp_root("path_evidence")?;
        let request_path = evidence.join("request.json");
        for inherited in [false, true] {
            if inherited {
                request.environment.remove("PATH");
            } else {
                request
                    .environment
                    .insert("PATH".into(), first_path.to_string_lossy().into_owned());
            }
            std::fs::write(
                &request_path,
                serde_json::to_vec(&request).map_err(to_string)?,
            )
            .map_err(to_string)?;
            let store_path = evidence.join(if inherited {
                "inherited.jsonl"
            } else {
                "explicit.jsonl"
            });
            for (stage, path) in [
                ("acquire", first_path),
                ("changed", second_path.as_path()),
                ("unchanged", first_path),
            ] {
                let output =
                    std::process::Command::new(std::env::current_exe().map_err(to_string)?)
                        .args([
                            "--exact",
                            "inherited_path_changes_are_checked_at_completion",
                            "--nocapture",
                        ])
                        .env(CHILD, stage)
                        .env(REQUEST, &request_path)
                        .env(STORE, &store_path)
                        .env("PATH", path)
                        .output()
                        .map_err(to_string)?;
                require(
                    output.status.success(),
                    format!(
                        // ubs:ignore[rust.perf.string-alloc-in-loop] At most six focused subprocess cases allocate their failure diagnostics.
                        "PATH subprocess {stage} failed: {} {}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ),
                )?;
            }
        }
        Ok(())
    }
}

fn base_request(slot_id: &str) -> ValidationSlotRequest {
    static WORKSPACE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let workspace = WORKSPACE.get_or_init(|| {
        let root = temp_root("fixture_workspace").expect("fixture workspace");
        for name in ["rch", "cargo", "ubs", "rch.exe", "cargo.exe", "ubs.exe"] {
            std::fs::write(root.join(name), b"validation fixture executable") // ubs:ignore[rust.security.path-traversal] Only the six literal fixture filenames above enter this join.
                .expect("fixture tool");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o755)) // ubs:ignore[rust.security.path-traversal] The same six literal fixture filenames are chmodded inside their test directory.
                    .expect("executable fixture tool");
            }
        }
        root
    });
    let mut environment = BTreeMap::new();
    environment.insert("PATH".into(), workspace.to_string_lossy().into_owned());
    environment.insert(
        "CARGO_TARGET_DIR".to_string(),
        "/data/tmp/pi_agent_rust_cargo/silentreef/target".to_string(),
    );
    environment.insert(
        "TMPDIR".to_string(),
        "/data/tmp/pi_agent_rust_cargo/silentreef/tmp".to_string(),
    );

    ValidationSlotRequest {
        slot_id: slot_id.to_string(),
        owner_agent: "SilentReef".to_string(),
        bead_id: "bd-gusp4.2".to_string(),
        command: vec![
            "rch".to_string(),
            "exec".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "check".to_string(),
            "--all-targets".to_string(),
        ],
        command_class: "cargo_check".to_string(),
        cwd: workspace.to_string_lossy().into_owned(),
        git_head: "cf653c29b5836afabf979bb44325d4712de7088d".to_string(),
        feature_flags: vec!["default".to_string()],
        target_dir: "/data/tmp/pi_agent_rust_cargo/silentreef/target".to_string(),
        tmpdir: "/data/tmp/pi_agent_rust_cargo/silentreef/tmp".to_string(),
        runner: "rch_required".to_string(),
        rust_toolchain: Some("nightly".to_string()),
        rch_job_id: Some("rch-job-123".to_string()),
        environment,
        expected_artifacts: vec![ValidationSlotArtifact {
            path: "target/debug/deps/pi.d".to_string(),
            sha256: None,
            schema: Some("cargo_metadata".to_string()),
        }],
        artifact_schema: Some("cargo_check_result.v1".to_string()),
        artifact_hash: Some("artifact-hash-1".to_string()),
    }
}

fn acquire(slot_id: &str) -> Result<ValidationSlotLease, String> {
    ValidationSlotLease::acquire(base_request(slot_id), START, EXPIRES)
        .map_err(|err| format!("acquire lease: {err}"))
}

fn provenance(source: &str) -> Result<ValidationSourceProvenance, String> {
    ValidationSourceProvenance::new(
        source,
        vec![source.to_string(), "--json".to_string()],
        "/data/projects/pi_agent_rust",
        START,
        Some(format!("artifacts/{source}.json")),
    )
    .map_err(to_string)
}

fn admission_context(request_id: &str) -> ValidationAdmissionRequestContext {
    admission_context_for(request_id, base_request("slot-request"), START, 4)
}

fn admission_context_for(
    request_id: &str,
    request: ValidationSlotRequest,
    requested_at_utc: &str,
    bead_priority: u8,
) -> ValidationAdmissionRequestContext {
    ValidationAdmissionRequestContext {
        request_id: request_id.to_string(),
        request,
        requested_at_utc: requested_at_utc.to_string(),
        bead_priority,
    }
}

fn healthy_inputs() -> Result<ValidationBrokerInputSnapshot, String> {
    inputs_with(
        "Build Queue\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\nWorker Availability\n  -> 4 / 18 slots free\n",
        false,
        false,
        &json!({"issues": [
            {"id": "bd-active", "status": "in_progress", "assignee": "Codex", "updated_at": RENEWED_EXPIRES}
        ]}),
    )
}

fn saturated_inputs() -> Result<ValidationBrokerInputSnapshot, String> {
    inputs_with(
        "Build Queue\n  - 4 Active Build(s)\n  - 2 Queued Build(s)\nWorker Availability\n  -> 0 / 18 slots free\n",
        false,
        false,
        &json!({"issues": []}),
    )
}

fn local_fallback_inputs() -> Result<ValidationBrokerInputSnapshot, String> {
    inputs_with(
        "Build Queue\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\nWorker Availability\n  -> 4 / 18 slots free\nRCH fails open; command may run with local fallback\n",
        false,
        false,
        &json!({"issues": []}),
    )
}

fn low_scratch_inputs() -> Result<ValidationBrokerInputSnapshot, String> {
    inputs_with(
        "Build Queue\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\nWorker Availability\n  -> 4 / 18 slots free\n",
        false,
        true,
        &json!({"issues": []}),
    )
}

fn stale_bead_inputs() -> Result<ValidationBrokerInputSnapshot, String> {
    inputs_with(
        "Build Queue\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\nWorker Availability\n  -> 4 / 18 slots free\n",
        false,
        false,
        &json!({"issues": [
            {"id": "bd-stale", "status": "in_progress", "assignee": "Other", "updated_at": START}
        ]}),
    )
}

fn inputs_with(
    rch_raw: &str,
    low_cargo: bool,
    low_scratch: bool,
    beads_value: &serde_json::Value,
) -> Result<ValidationBrokerInputSnapshot, String> {
    let rch = normalize_rch_queue_text(provenance("rch")?, rch_raw).map_err(to_string)?;
    let cargo_available = if low_cargo { 5_000_u64 } else { 50_000_u64 };
    let scratch_available = if low_scratch { 5_000_u64 } else { 50_000_u64 };
    let cargo_headroom = normalize_headroom_json(
        provenance("cargo_headroom")?,
        &json!({"available_bytes": cargo_available, "required_bytes": 10_000_u64}),
    )
    .map_err(to_string)?;
    let doctor = normalize_doctor_json(
        provenance("doctor")?,
        &json!({"checks": [{"name": "scratch", "status": "ok"}]}),
    )
    .map_err(to_string)?;
    let git = normalize_git_status_text(provenance("git")?, "3048e53f3", "## main...origin/main\n")
        .map_err(to_string)?;
    let beads = normalize_beads_json(provenance("beads")?, beads_value, STALE_AT, 3600)
        .map_err(to_string)?;
    let scratch_headroom = normalize_headroom_json(
        provenance("scratch_headroom")?,
        &json!({"available_bytes": scratch_available, "required_bytes": 10_000_u64}),
    )
    .map_err(to_string)?;
    let agent_mail = normalize_available_source(provenance("agent_mail")?).map_err(to_string)?;

    ValidationBrokerInputSnapshot::from_parts(ValidationBrokerInputParts {
        captured_at_utc: STALE_AT.to_string(),
        rch,
        cargo_headroom,
        doctor,
        git,
        beads,
        scratch_headroom,
        agent_mail,
    })
    .map_err(to_string)
}

fn slot_snapshot(leases: Vec<ValidationSlotLease>) -> ValidationSlotStoreSnapshot {
    let latest_by_slot_id = leases
        .iter()
        .map(|lease| (lease.slot_id.clone(), lease.clone()))
        .collect();
    ValidationSlotStoreSnapshot {
        schema: VALIDATION_BROKER_SLOT_STORE_SCHEMA.to_string(),
        status: ValidationSlotStoreStatus::Available,
        leases,
        latest_by_slot_id,
        degraded_reasons: Vec::new(),
    }
}

#[derive(Debug, Deserialize)]
struct FaultCorpus {
    schema: String,
    event_log_path: String,
    scenarios: Vec<FaultScenario>,
}

#[derive(Debug, Deserialize)]
struct FaultScenario {
    scenario_id: String,
    faults: Vec<String>,
    request: FaultRequest,
    inputs: FaultInputs,
    #[serde(default)]
    slot_store: Vec<FaultSlot>,
    #[serde(default)]
    policy: FaultPolicy,
    artifact_manifest: Vec<FaultArtifactManifestEntry>,
    expected: FaultExpected,
}

#[derive(Debug, Deserialize)]
struct FaultRequest {
    slot_id: String,
    command_class: String,
    #[serde(default)]
    runner: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FaultInputs {
    rch: String,
    cargo_headroom: String,
    scratch_headroom: String,
    doctor: String,
    git: String,
    beads: String,
    agent_mail: String,
}

#[derive(Debug, Default, Deserialize)]
struct FaultPolicy {
    allow_narrow_scope: Option<bool>,
    reuse_required: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct FaultSlot {
    slot_id: String,
    state: String,
    equivalence: String,
    expires_at_utc: String,
}

#[derive(Debug, Deserialize)]
struct FaultArtifactManifestEntry {
    path: String,
    artifact_schema: String,
    evidence_kind: String,
}

#[derive(Debug, Deserialize)]
struct FaultExpected {
    decision: String,
    confidence: String,
    #[serde(default)]
    reasons: Vec<String>,
    #[serde(default)]
    required_actions: Vec<String>,
    #[serde(default)]
    reusable_slot: Option<String>,
    #[serde(default)]
    coalesced_artifacts: Option<usize>,
    #[serde(default)]
    source_statuses: BTreeMap<String, String>,
    #[serde(default)]
    policy: BTreeMap<String, Value>,
    #[serde(default)]
    rejected_reusable_slots: Vec<FaultRejectedReusableSlot>,
}

#[derive(Debug, Deserialize)]
struct FaultRejectedReusableSlot {
    slot_id: String,
    reasons: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct StressProfileCorpus {
    schema: String,
    generated_at_utc: String,
    budgets: ValidationBrokerStressBudgets,
    profiles: Vec<ValidationBrokerStressProfile>,
    caveats: Vec<String>,
}

fn fault_corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden_corpus/validation_broker/fault_corpus.json")
}

fn stress_profile_corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden_corpus/validation_broker/stress_profiles.json")
}

fn repo_fixture_path(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn load_fault_corpus() -> Result<FaultCorpus, String> {
    let raw = std::fs::read_to_string(fault_corpus_path())
        .map_err(|err| format!("read fault corpus: {err}"))?;
    serde_json::from_str(&raw).map_err(|err| format!("parse fault corpus: {err}"))
}

fn load_stress_profile_corpus() -> Result<StressProfileCorpus, String> {
    let raw = std::fs::read_to_string(stress_profile_corpus_path())
        .map_err(|err| format!("read stress profile corpus: {err}"))?;
    serde_json::from_str(&raw).map_err(|err| format!("parse stress profile corpus: {err}"))
}

fn stress_profile_by_id(
    corpus: &StressProfileCorpus,
    profile_id: &str,
) -> Result<ValidationBrokerStressProfile, String> {
    corpus
        .profiles
        .iter()
        .find(|profile| profile.profile_id == profile_id)
        .cloned()
        .ok_or_else(|| format!("missing stress profile {profile_id}"))
}

fn fault_request(request: &FaultRequest) -> ValidationSlotRequest {
    let mut slot_request = base_request(&request.slot_id);
    slot_request.owner_agent = "Codex".to_string();
    slot_request.bead_id = "bd-gusp4.6".to_string();
    slot_request
        .command_class
        .clone_from(&request.command_class);
    slot_request.command = fault_command(&request.command_class);
    if let Some(runner) = &request.runner {
        slot_request.runner.clone_from(runner);
    }
    slot_request
}

fn fault_command(command_class: &str) -> Vec<String> {
    match command_class {
        "cargo_clippy" => vec![
            "rch".to_string(),
            "exec".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "clippy".to_string(),
            "--all-targets".to_string(),
            "--".to_string(),
            "-D".to_string(),
            "warnings".to_string(),
        ],
        "ubs_staged" => vec![
            "ubs".to_string(),
            "--staged".to_string(),
            "--only=rust".to_string(),
            ".".to_string(),
        ],
        _ => vec![
            "rch".to_string(),
            "exec".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "check".to_string(),
            "--all-targets".to_string(),
        ],
    }
}

fn fault_inputs(inputs: &FaultInputs) -> Result<ValidationBrokerInputSnapshot, String> {
    let rch = normalize_rch_queue_text(provenance("rch")?, fault_rch_text(&inputs.rch))
        .map_err(to_string)?;
    let cargo_headroom = normalize_headroom_json(
        provenance("cargo_headroom")?,
        &headroom_value(&inputs.cargo_headroom),
    )
    .map_err(to_string)?;
    let scratch_headroom = normalize_headroom_json(
        provenance("scratch_headroom")?,
        &headroom_value(&inputs.scratch_headroom),
    )
    .map_err(to_string)?;
    let doctor = normalize_doctor_json(provenance("doctor")?, &doctor_value(&inputs.doctor))
        .map_err(to_string)?;
    let git = normalize_git_status_text(
        provenance("git")?,
        "3048e53f3",
        fault_git_status(&inputs.git),
    )
    .map_err(to_string)?;
    let beads = normalize_beads_json(
        provenance("beads")?,
        &beads_value(&inputs.beads),
        STALE_AT,
        3600,
    )
    .map_err(to_string)?;
    let agent_mail = match inputs.agent_mail.as_str() {
        "available" => normalize_available_source(provenance("agent_mail")?).map_err(to_string)?,
        "unavailable" => {
            normalize_unavailable_source(provenance("agent_mail")?, "agent_mail_schema_missing")
                .map_err(to_string)?
        }
        other => return Err(format!("unknown agent_mail fixture state: {other}")),
    };

    ValidationBrokerInputSnapshot::from_parts(ValidationBrokerInputParts {
        captured_at_utc: STALE_AT.to_string(),
        rch,
        cargo_headroom,
        doctor,
        git,
        beads,
        scratch_headroom,
        agent_mail,
    })
    .map_err(to_string)
}

fn fault_rch_text(state: &str) -> &'static str {
    match state {
        "healthy" => {
            "Build Queue\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\nWorker Availability\n  -> 4 / 18 slots free\n"
        }
        "saturated" => {
            "Build Queue\n  - 5 Active Build(s)\n  - 2 Queued Build(s)\nWorker Availability\n  -> 0 / 18 slots free\n"
        }
        "local_fallback" => {
            "Build Queue\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\nWorker Availability\n  -> 4 / 18 slots free\nRCH fails open; command may run with local fallback\n"
        }
        _ => "",
    }
}

fn headroom_value(state: &str) -> Value {
    match state {
        "low" => json!({"available_bytes": 5_000_u64, "required_bytes": 10_000_u64}),
        _ => json!({"available_bytes": 50_000_u64, "required_bytes": 10_000_u64}),
    }
}

fn doctor_value(state: &str) -> Value {
    match state {
        "failed" => json!({"checks": [{"name": "scratch", "status": "fail"}]}),
        _ => json!({"checks": [{"name": "scratch", "status": "ok"}]}),
    }
}

fn fault_git_status(state: &str) -> &'static str {
    match state {
        "dirty" => "## main...origin/main\n M src/validation_broker.rs\n",
        _ => "## main...origin/main\n",
    }
}

fn beads_value(state: &str) -> Value {
    match state {
        "stale_in_progress" => json!({"issues": [
            {"id": "bd-stale", "status": "in_progress", "assignee": "AbsentAgent", "updated_at": START}
        ]}),
        _ => json!({"issues": []}),
    }
}

fn fault_policy(policy: &FaultPolicy) -> ValidationAdmissionPolicy {
    let mut result = ValidationAdmissionPolicy::default();
    if let Some(allow_narrow_scope) = policy.allow_narrow_scope {
        result.allow_narrow_scope = allow_narrow_scope;
    }
    if let Some(reuse_required) = policy.reuse_required {
        result.reuse_required = reuse_required;
    }
    result
}

fn fault_slot_snapshot(
    slots: &[FaultSlot],
    request: &ValidationSlotRequest,
) -> Result<ValidationSlotStoreSnapshot, String> {
    let mut leases = Vec::new();
    for slot in slots {
        let slot_request = fault_slot_request(slot, request)?;
        let mut lease = ValidationSlotLease::acquire(slot_request, START, &slot.expires_at_utc)
            .map_err(to_string)?;
        match slot.state.as_str() {
            "active" => {}
            "reusable" => {
                #[cfg(unix)]
                lease
                    .mark_reusable(&request.owner_agent, HEARTBEAT, reusable_fault_artifacts())
                    .map_err(to_string)?;
                #[cfg(not(unix))]
                {
                    require(
                        lease.input_identity.is_none(),
                        "unsupported fixture capture is unavailable",
                    )?;
                    require(
                        lease
                            .mark_reusable(
                                &request.owner_agent,
                                HEARTBEAT,
                                reusable_fault_artifacts(),
                            )
                            .is_err(),
                        "unsupported fixture cannot publish new reusable evidence",
                    )?;
                    // Model a historical record so rejection and provenance
                    // tests remain active even without native capture support.
                    lease.state = ValidationSlotState::Reusable;
                    lease.heartbeat_at_utc = HEARTBEAT.into();
                    lease.artifacts = reusable_fault_artifacts();
                }
            }
            other => return Err(unknown_fault_slot_state(other)),
        }
        leases.push(lease);
    }
    Ok(slot_snapshot(leases))
}

fn unknown_fault_slot_state(state: &str) -> String {
    format!("unknown fault slot state: {state}")
}

fn reusable_fault_artifacts() -> Vec<ValidationSlotArtifact> {
    vec![ValidationSlotArtifact {
        path: "target/debug/deps/pi.d".to_string(),
        sha256: Some("artifact-hash-1".to_string()),
        schema: Some("cargo_check_result.v1".to_string()),
    }]
}

fn fault_slot_request(
    slot: &FaultSlot,
    request: &ValidationSlotRequest,
) -> Result<ValidationSlotRequest, String> {
    let mut slot_request = ValidationSlotRequest {
        slot_id: slot.slot_id.clone(),
        owner_agent: request.owner_agent.clone(),
        bead_id: request.bead_id.clone(),
        command: request.command.clone(),
        command_class: request.command_class.clone(),
        cwd: request.cwd.clone(),
        git_head: request.git_head.clone(),
        feature_flags: request.feature_flags.clone(),
        target_dir: request.target_dir.clone(),
        tmpdir: request.tmpdir.clone(),
        runner: request.runner.clone(),
        rust_toolchain: request.rust_toolchain.clone(),
        rch_job_id: request.rch_job_id.clone(),
        environment: request.environment.clone(),
        expected_artifacts: request.expected_artifacts.clone(),
        artifact_schema: request.artifact_schema.clone(),
        artifact_hash: request.artifact_hash.clone(),
    };
    apply_slot_equivalence(&mut slot_request, &slot.equivalence)?;
    Ok(slot_request)
}

fn apply_slot_equivalence(
    request: &mut ValidationSlotRequest,
    equivalence: &str,
) -> Result<(), String> {
    match equivalence {
        "matching" => Ok(()),
        "target_dir_mismatch" => {
            request.target_dir = "/data/tmp/pi_agent_rust_cargo/other/target".to_string();
            Ok(())
        }
        "git_mismatch" => {
            request.git_head = "different-head".to_string();
            Ok(())
        }
        other => Err(format!("unknown slot equivalence fixture: {other}")),
    }
}

fn fault_event_scenario_ids(event_log_path: &str) -> Result<BTreeSet<String>, String> {
    let raw = std::fs::read_to_string(repo_fixture_path(event_log_path))
        .map_err(|err| format!("read fault event log: {err}"))?;
    let mut ids = BTreeSet::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        ids.insert(fault_event_scenario_id(line, index + 1)?);
    }
    Ok(ids)
}

fn fault_event_scenario_id(line: &str, line_number: usize) -> Result<String, String> {
    let event: Value = serde_json::from_str(line)
        .map_err(|err| format!("parse fault event line {line_number}: {err}"))?;
    require(
        event.get("schema").and_then(Value::as_str) == Some("pi.validation_broker.fault_event.v1"),
        "fault event schema",
    )?;
    event
        .get("scenario_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("fault event line {line_number} missing scenario_id"))
}

fn validate_fault_manifest(scenario: &FaultScenario) -> TestResult {
    require(
        !scenario.faults.is_empty(),
        "fault scenario names at least one fault",
    )?;
    require(
        !scenario.artifact_manifest.is_empty(),
        "fault scenario artifact manifest is not empty",
    )?;
    for artifact in &scenario.artifact_manifest {
        require(
            repo_fixture_path(&artifact.path).exists(),
            "fault artifact path exists",
        )?;
        require(
            !artifact.artifact_schema.trim().is_empty(),
            "fault artifact schema present",
        )?;
        require(
            !artifact.evidence_kind.trim().is_empty(),
            "fault artifact evidence kind present",
        )?;
    }
    Ok(())
}

const fn decision_key_for_test(decision: &ValidationAdmissionDecision) -> &'static str {
    match decision {
        ValidationAdmissionDecision::Allow => "allow",
        ValidationAdmissionDecision::Wait => "wait",
        ValidationAdmissionDecision::Coalesce => "coalesce",
        ValidationAdmissionDecision::Narrow => "narrow",
        ValidationAdmissionDecision::DenyLocalFallback => "deny_local_fallback",
        ValidationAdmissionDecision::StaleRecover => "stale_recover",
        ValidationAdmissionDecision::DegradedBlock => "degraded_block",
    }
}

const fn source_state_key(state: &ValidationSourceState) -> &'static str {
    match state {
        ValidationSourceState::Available => "available",
        ValidationSourceState::Unavailable => "unavailable",
        ValidationSourceState::Degraded => "degraded",
    }
}

fn assert_expected_policy(
    decision: &pi::validation_broker::ValidationAdmissionDecisionRecord,
    expected: &BTreeMap<String, Value>,
) -> TestResult {
    for (field, expected_value) in expected {
        let Some(actual) = policy_field_value(decision, field) else {
            return Err(unknown_expected_policy_field(field));
        };
        if &actual != expected_value {
            return Err(policy_field_mismatch(field));
        }
    }
    Ok(())
}

fn unknown_expected_policy_field(field: &str) -> String {
    format!("unknown expected policy field: {field}")
}

fn policy_field_mismatch(field: &str) -> String {
    format!("policy field {field} did not match expected value")
}

fn policy_field_value(
    decision: &pi::validation_broker::ValidationAdmissionDecisionRecord,
    field: &str,
) -> Option<Value> {
    match field {
        "active_equivalent_slots" => Some(json!(decision.policy.active_equivalent_slots)),
        "reusable_equivalent_slots" => Some(json!(decision.policy.reusable_equivalent_slots)),
        "stale_equivalent_slots" => Some(json!(decision.policy.stale_equivalent_slots)),
        "active_broad_gates" => Some(json!(decision.policy.active_broad_gates)),
        "stale_in_progress_beads" => Some(json!(decision.policy.stale_in_progress_beads)),
        "rch_saturated" => Some(json!(decision.policy.rch_saturated)),
        "rch_local_fallback" => Some(json!(decision.policy.rch_local_fallback)),
        "low_cargo_headroom" => Some(json!(decision.policy.low_cargo_headroom)),
        "low_scratch_headroom" => Some(json!(decision.policy.low_scratch_headroom)),
        "reuse_required" => Some(json!(decision.policy.reuse_required)),
        _ => None,
    }
}

fn assert_expected_rejections(
    actual: &[ValidationRejectedReusableSlot],
    expected: &[FaultRejectedReusableSlot],
) -> TestResult {
    require(
        actual.len() == expected.len(),
        "rejected reusable slot count matched",
    )?;
    for expected_slot in expected {
        let Some(actual_slot) = actual
            .iter()
            .find(|slot| slot.slot_id == expected_slot.slot_id)
        else {
            return Err(missing_rejected_reusable_slot(&expected_slot.slot_id));
        };
        for reason in &expected_slot.reasons {
            if !actual_slot
                .reasons
                .iter()
                .any(|actual_reason| actual_reason == reason)
            {
                return Err(missing_rejected_reusable_reason(reason));
            }
        }
    }
    Ok(())
}

fn missing_rejected_reusable_slot(slot_id: &str) -> String {
    format!("missing rejected reusable slot {slot_id}")
}

fn missing_rejected_reusable_reason(reason: &str) -> String {
    format!("rejected reusable slot reason {reason} missing")
}

#[test]
fn validation_broker_fault_corpus_covers_build_storm_and_stale_recovery() -> TestResult {
    let corpus = load_fault_corpus()?;
    require(
        corpus.schema == "pi.validation_broker.fault_corpus.v1",
        "fault corpus schema",
    )?;
    let event_scenario_ids = fault_event_scenario_ids(&corpus.event_log_path)?;

    let mut seen_decisions = BTreeSet::new();
    for scenario in &corpus.scenarios {
        require(
            event_scenario_ids.contains(&scenario.scenario_id),
            "scenario has JSONL event evidence",
        )?;
        validate_fault_manifest(scenario)?;

        let request = fault_request(&scenario.request);
        let inputs = fault_inputs(&scenario.inputs)?;
        let slot_store = fault_slot_snapshot(&scenario.slot_store, &request)?;
        let policy = fault_policy(&scenario.policy);
        let request_id = scenario_request_id(&scenario.scenario_id);
        let decision = decide_validation_admission(
            admission_context_for(&request_id, request, START, 4),
            &inputs,
            &slot_store,
            &policy,
            STALE_AT,
        )
        .map_err(to_string)?;

        let decision_key = decision_key_for_test(&decision.decision);
        seen_decisions.insert(decision_key);
        #[cfg(not(unix))]
        if scenario.expected.decision == "coalesce" {
            require(
                decision.decision == ValidationAdmissionDecision::Allow,
                "legacy result requires fresh work on an unsupported platform",
            )?;
            require(
                decision.reusable_slot.is_none() && decision.coalesced_artifacts.is_empty(),
                "legacy result never supplies artifacts without native identity",
            )?;
            require(
                decision.policy.reusable_equivalent_slots == 0,
                "unsupported capture has no reusable candidate",
            )?;
            require(
                decision.rejected_reusable_slots.iter().any(|slot| {
                    slot.reasons
                        .iter()
                        .any(|reason| reason == "input_identity_unavailable_or_changed")
                }),
                "unsupported reuse refusal retains provenance diagnostic",
            )?;
            continue;
        }
        if decision_key != scenario.expected.decision {
            return Err(scenario_decision_mismatch(&scenario.scenario_id));
        }
        if decision.confidence != scenario.expected.confidence {
            return Err(scenario_confidence_mismatch(&scenario.scenario_id));
        }
        for reason in &scenario.expected.reasons {
            if !decision.reasons.iter().any(|actual| actual == reason) {
                return Err(scenario_reason_missing(&scenario.scenario_id, reason));
            }
        }
        for action in &scenario.expected.required_actions {
            if !decision
                .required_actions
                .iter()
                .any(|actual| actual == action)
            {
                return Err(scenario_action_missing(&scenario.scenario_id, action));
            }
        }
        if decision.reusable_slot != scenario.expected.reusable_slot {
            return Err(scenario_reusable_slot_mismatch(&scenario.scenario_id));
        }
        if let Some(expected_artifacts) = scenario.expected.coalesced_artifacts
            && decision.coalesced_artifacts.len() != expected_artifacts
        {
            return Err(scenario_coalesced_artifact_count_mismatch(
                &scenario.scenario_id,
            ));
        }
        for (source_id, expected_state) in &scenario.expected.source_statuses {
            let Some(actual_status) = decision
                .source_statuses
                .iter()
                .find(|status| &status.source_id == source_id)
            else {
                return Err(scenario_source_status_missing(
                    &scenario.scenario_id,
                    source_id,
                ));
            };
            if source_state_key(&actual_status.state) != expected_state {
                return Err(scenario_source_state_mismatch(
                    &scenario.scenario_id,
                    source_id,
                ));
            }
        }
        assert_expected_policy(&decision, &scenario.expected.policy)?;
        assert_expected_rejections(
            &decision.rejected_reusable_slots,
            &scenario.expected.rejected_reusable_slots,
        )?;
    }

    for required_decision in [
        "wait",
        "narrow",
        "deny_local_fallback",
        "stale_recover",
        "degraded_block",
    ] {
        if !seen_decisions.contains(required_decision) {
            return Err(fault_corpus_missing_decision(required_decision));
        }
    }
    #[cfg(unix)]
    require(
        seen_decisions.contains("coalesce"),
        "supported corpus covers coalescing",
    )?;
    #[cfg(not(unix))]
    require(
        !seen_decisions.contains("coalesce"),
        "unsupported corpus never claims coalescing",
    )?;

    Ok(())
}

fn scenario_request_id(scenario_id: &str) -> String {
    format!("request-{scenario_id}")
}

fn scenario_decision_mismatch(scenario_id: &str) -> String {
    format!("{scenario_id} decision did not match")
}

fn scenario_confidence_mismatch(scenario_id: &str) -> String {
    format!("{scenario_id} confidence did not match")
}

fn scenario_reason_missing(scenario_id: &str, reason: &str) -> String {
    format!("{scenario_id} reason {reason} missing")
}

fn scenario_action_missing(scenario_id: &str, action: &str) -> String {
    format!("{scenario_id} action {action} missing")
}

fn scenario_reusable_slot_mismatch(scenario_id: &str) -> String {
    format!("{scenario_id} reusable slot did not match")
}

fn scenario_coalesced_artifact_count_mismatch(scenario_id: &str) -> String {
    format!("{scenario_id} coalesced artifact count did not match")
}

fn scenario_source_status_missing(scenario_id: &str, source_id: &str) -> String {
    format!("{scenario_id} missing source status {source_id}")
}

fn scenario_source_state_mismatch(scenario_id: &str, source_id: &str) -> String {
    format!("{scenario_id} source {source_id} state did not match")
}

fn fault_corpus_missing_decision(required_decision: &str) -> String {
    format!("fault corpus missing {required_decision} decision")
}

#[test]
fn lease_store_acquires_renews_releases_and_appends_records() -> TestResult {
    let root = temp_root("append")?;
    let store = ValidationSlotStore::new(root.join("validation-slots.jsonl"));
    let mut lease = acquire("slot-append")?;

    require(lease.schema == VALIDATION_BROKER_SLOT_SCHEMA, "slot schema")?;
    require(lease.state == ValidationSlotState::Active, "initial state")?;
    require(!lease.command_fingerprint.is_empty(), "command fingerprint")?;
    require(
        !lease.environment_fingerprint.is_empty(),
        "environment fingerprint",
    )?;

    store
        .append_lease("acquired", START, &lease)
        .map_err(|err| format!("append acquired: {err}"))?;

    lease
        .renew("SilentReef", HEARTBEAT, RENEWED_EXPIRES)
        .map_err(|err| format!("renew lease: {err}"))?;
    store
        .append_lease("renewed", HEARTBEAT, &lease)
        .map_err(|err| format!("append renewed: {err}"))?;

    lease
        .release(
            "SilentReef",
            "2026-05-14T07:10:00Z",
            "finished focused gate",
        )
        .map_err(|err| format!("release lease: {err}"))?;
    store
        .append_lease("released", "2026-05-14T07:10:00Z", &lease)
        .map_err(|err| format!("append released: {err}"))?;

    let snapshot = store.load_snapshot();
    require(
        snapshot.schema == VALIDATION_BROKER_SLOT_STORE_SCHEMA,
        "store schema",
    )?;
    require(
        snapshot.status == ValidationSlotStoreStatus::Available,
        "snapshot available",
    )?;
    require(snapshot.leases.len() == 3, "append-only history length")?;
    let latest = snapshot
        .latest_by_slot_id
        .get("slot-append")
        .ok_or_else(|| "latest slot missing".to_string())?;
    require(
        latest.state == ValidationSlotState::Released,
        "latest released state",
    )?;
    require(
        latest.release_reason.as_deref() == Some("finished focused gate"),
        "release reason preserved",
    )
}

#[test]
fn stale_detection_requires_expiry_and_explicit_reason() -> TestResult {
    let mut lease = acquire("slot-stale")?;

    require(
        !lease.is_stale_at(HEARTBEAT).map_err(to_string)?,
        "not stale",
    )?;
    require(lease.is_stale_at(STALE_AT).map_err(to_string)?, "stale")?;
    require(
        !lease
            .is_stale_at("2026-05-14T07:29:59.999999999Z")
            .map_err(to_string)?,
        "lease remains active immediately before expiry",
    )?;
    require(
        lease.is_stale_at(EXPIRES).map_err(to_string)?,
        "lease expires at its exact deadline",
    )?;
    require(
        lease.mark_stale(STALE_AT, "   ").is_err(),
        "blank stale reason rejected",
    )?;
    require(
        ValidationSlotLease::acquire(
            base_request("slot-non-utc"),
            "2026-05-14T07:00:00+01:00",
            EXPIRES,
        )
        .is_err(),
        "non-UTC timestamp rejected",
    )?;

    lease
        .mark_stale(STALE_AT, "owner heartbeat expired")
        .map_err(|err| format!("mark stale: {err}"))?;
    require(lease.state == ValidationSlotState::Stale, "stale state")?;
    require(
        lease.state_reason.as_deref() == Some("owner heartbeat expired"),
        "stale reason recorded",
    )
}

#[test]
fn rejected_lease_mutations_preserve_state_and_cannot_backdate_evidence() -> TestResult {
    let mut original = acquire("slot-atomic-failure")?;
    original
        .renew("SilentReef", HEARTBEAT, EXPIRES)
        .map_err(to_string)?;
    for invalid in ["invalid", "2026-05-14T06:59:59Z", START] {
        let mut lease = original.clone(); // ubs:ignore[rust.collections.clone-in-loop] Three invalid timestamps must be applied to fresh copies of the baseline lease.
        require(
            lease.renew("SilentReef", invalid, EXPIRES).is_err(),
            "invalid renewal heartbeat rejected",
        )?;
        require(lease == original, "failed renewal leaves lease unchanged")?;

        require(
            lease
                .mark_reusable("SilentReef", invalid, reusable_fault_artifacts())
                .is_err(),
            "invalid reusable heartbeat rejected",
        )?;
        require(lease == original, "failed reuse leaves lease unchanged")?;

        require(
            lease.release("SilentReef", invalid, "done").is_err(),
            "invalid release heartbeat rejected",
        )?;
        require(lease == original, "failed release leaves lease unchanged")?;

        require(
            lease.fail("SilentReef", invalid, "failed").is_err(),
            "invalid failure heartbeat rejected",
        )?;
        require(
            lease == original,
            "failed failure update leaves lease unchanged",
        )?;

        require(
            lease.mark_stale(invalid, "expired").is_err(),
            "invalid stale heartbeat rejected",
        )?;
        require(
            lease == original,
            "failed stale update leaves lease unchanged",
        )?;
    }
    let mut expired = original.clone();
    require(
        expired
            .mark_reusable("SilentReef", EXPIRES, reusable_fault_artifacts())
            .is_err(),
        "expired lease cannot publish reusable evidence without renewal",
    )?;
    require(expired == original, "expired reuse leaves lease unchanged")?;
    // Terminal observations may legitimately arrive after a lease expires.
    expired
        .release("SilentReef", STALE_AT, "owner completed after expiry")
        .map_err(to_string)?;
    let root = temp_root("late_release")?;
    let store = ValidationSlotStore::new(root.join("slots.jsonl"));
    store
        .append_lease("released", STALE_AT, &expired)
        .map_err(to_string)?;
    require(
        !store.load_snapshot().is_degraded(),
        "late release remains valid",
    )
}

#[test]
fn invalid_lease_timestamp_order_degrades_persisted_history() -> TestResult {
    for (field, value) in [
        ("started_at_utc", STALE_AT),
        ("expires_at_utc", START),
        ("heartbeat_at_utc", "2026-05-14T06:59:59Z"),
        ("heartbeat_at_utc", EXPIRES),
        ("heartbeat_at_utc", STALE_AT),
    ] {
        let root = temp_root("invalid_lease_time")?;
        let store = ValidationSlotStore::new(root.join("slots.jsonl"));
        let lease = acquire("slot-time-history")?;
        store
            .append_lease("active", START, &lease)
            .map_err(to_string)?;
        let mut raw = std::fs::read_to_string(store.path()).map_err(to_string)?;
        let mut corrupted: Value = serde_json::from_str(raw.trim()).map_err(to_string)?;
        corrupted["lease"][field] = json!(value);
        raw.push_str(&serde_json::to_string(&corrupted).map_err(to_string)?);
        raw.push('\n');
        std::fs::write(store.path(), raw).map_err(to_string)?;

        let snapshot = ValidationSlotStore::new(store.path()).load_snapshot();
        require(
            snapshot.is_degraded(),
            "invalid timestamp order degrades store",
        )?;
        require(snapshot.leases.len() == 1, "earlier valid history retained")?;
        let decision = decide_validation_admission(
            admission_context("invalid-time-record"),
            &healthy_inputs()?,
            &snapshot,
            &ValidationAdmissionPolicy::default(),
            HEARTBEAT,
        )
        .map_err(to_string)?;
        require(
            decision.decision == ValidationAdmissionDecision::DegradedBlock,
            "invalid later timestamp blocks admission from earlier history",
        )?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn persisted_reusable_evidence_obeys_exact_freshness_boundaries() -> TestResult {
    let root = temp_root("freshness_time")?;
    let store = ValidationSlotStore::new(root.join("slots.jsonl"));
    let mut lease = acquire("slot-freshness-time")?;
    lease
        .mark_reusable("SilentReef", HEARTBEAT, reusable_fault_artifacts())
        .map_err(to_string)?;
    store
        .append_lease("reusable", HEARTBEAT, &lease)
        .map_err(to_string)?;
    let snapshot = ValidationSlotStore::new(store.path()).load_snapshot();
    require(!snapshot.is_degraded(), "timestamp fixture reloads")?;
    let policy = ValidationAdmissionPolicy {
        reusable_artifact_freshness_seconds: 60,
        ..ValidationAdmissionPolicy::default()
    };
    for (now, coalesce) in [
        ("2026-05-14T07:04:59.999999999Z", false),
        (HEARTBEAT, true),
        ("2026-05-14T07:06:00Z", true),
        ("2026-05-14T07:06:00.000000001Z", false),
        (EXPIRES, false),
    ] {
        let decision = decide_validation_admission(
            admission_context("freshness-boundary"),
            &healthy_inputs()?,
            &snapshot,
            &policy,
            now,
        )
        .map_err(to_string)?;
        require(
            (decision.decision == ValidationAdmissionDecision::Coalesce) == coalesce,
            "persisted evidence respects future, fractional, and expiry boundaries",
        )?;
    }
    Ok(())
}

#[test]
fn malformed_records_degrade_snapshot_but_keep_valid_history() -> TestResult {
    let root = temp_root("malformed")?;
    let store = ValidationSlotStore::new(root.join("validation-slots.jsonl"));
    let lease = acquire("slot-valid")?;
    store
        .append_lease("acquired", START, &lease)
        .map_err(|err| format!("append acquired: {err}"))?;

    let path = store.path();
    let mut raw = std::fs::read_to_string(path).map_err(|err| format!("read store: {err}"))?;
    let wrong_schema_record = raw
        .lines()
        .next()
        .ok_or_else(|| "valid record missing".to_string())?
        .replacen(
            VALIDATION_BROKER_SLOT_RECORD_SCHEMA,
            "wrong.validation_record_schema",
            1,
        );
    raw.push_str("{not-json}\n");
    raw.push_str(&wrong_schema_record);
    raw.push('\n');
    std::fs::write(path, raw).map_err(|err| format!("write malformed store: {err}"))?;

    let snapshot = store.load_snapshot();
    require(snapshot.is_degraded(), "snapshot degraded")?;
    require(snapshot.leases.len() == 1, "valid history retained")?;
    require(
        snapshot
            .degraded_reasons
            .iter()
            .any(|reason| reason.contains("malformed record")),
        "malformed reason recorded",
    )?;
    require(
        snapshot
            .degraded_reasons
            .iter()
            .any(|reason| reason.contains("unexpected schema")),
        "schema reason recorded",
    )
}

#[test]
fn unavailable_store_loads_as_read_only_degraded_snapshot() -> TestResult {
    let root = temp_root("unavailable")?;
    let store_path = root.join("validation-slots.jsonl");
    std::fs::create_dir_all(&store_path).map_err(|err| format!("create dir store: {err}"))?;
    let store = ValidationSlotStore::new(&store_path);

    let snapshot = store.load_snapshot();
    require(snapshot.is_degraded(), "directory path is degraded")?;
    require(snapshot.leases.is_empty(), "no invented leases")?;
    require(
        snapshot
            .degraded_reasons
            .iter()
            .any(|reason| reason.contains("store_unavailable")),
        "unavailable reason recorded",
    )
}

#[cfg(unix)]
#[test]
fn reusable_slots_require_matching_provenance_for_coalescing() -> TestResult {
    let mut lease = acquire("slot-reusable")?;
    lease
        .mark_reusable(
            "SilentReef",
            HEARTBEAT,
            vec![ValidationSlotArtifact {
                path: "target/debug/deps/pi.d".to_string(),
                sha256: Some("artifact-hash-1".to_string()),
                schema: Some("cargo_check_result.v1".to_string()),
            }],
        )
        .map_err(|err| format!("mark reusable: {err}"))?;

    let matching = base_request("slot-reusable");
    require(
        lease
            .matches_request_equivalence(&matching)
            .map_err(to_string)?,
        "matching request should coalesce",
    )?;

    let mut different_git = base_request("slot-reusable");
    different_git.git_head = "different-head".to_string();
    require(
        !lease
            .matches_request_equivalence(&different_git)
            .map_err(to_string)?,
        "git mismatch must not coalesce",
    )?;

    let mut different_target = base_request("slot-reusable");
    different_target.target_dir = "/data/tmp/other-agent/target".to_string();
    require(
        !lease
            .matches_request_equivalence(&different_target)
            .map_err(to_string)?,
        "target mismatch must not coalesce",
    )
}

#[test]
fn source_normalizers_build_available_input_snapshot() -> TestResult {
    let rch = normalize_rch_queue_text(
        provenance("rch")?,
        "Build Queue\n\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\n\nWorker Availability\n  -> 4 / 18 slots free\n",
    )
    .map_err(to_string)?;
    require(
        rch.health.state == ValidationSourceState::Available,
        "rch available",
    )?;
    require(rch.active_builds == Some(1), "active builds parsed")?;
    require(rch.queued_builds == Some(0), "queued builds parsed")?;
    require(rch.free_slots == Some(4), "free slots parsed")?;
    require(!rch.saturated, "rch not saturated")?;

    let cargo_headroom = normalize_headroom_json(
        provenance("cargo_headroom")?,
        &json!({"available_bytes": 50_000_u64, "required_bytes": 10_000_u64}),
    )
    .map_err(to_string)?;
    require(!cargo_headroom.low_headroom, "cargo headroom sufficient")?;

    let doctor = normalize_doctor_json(
        provenance("doctor")?,
        &json!({"checks": [
            {"name": "scratch", "status": "ok"},
            {"name": "rch", "status": "pass"}
        ]}),
    )
    .map_err(to_string)?;
    require(!doctor.has_failures, "doctor checks pass")?;

    let git = normalize_git_status_text(
        provenance("git")?,
        "3048e53f3",
        "## main...origin/main\nM  src/lib.rs\n M tests/validation_broker_store.rs\n?? scratch.txt\n",
    )
    .map_err(to_string)?;
    require(git.branch.as_deref() == Some("main"), "git branch parsed")?;
    require(git.dirty, "git dirty detected")?;
    require(
        git.staged_paths.iter().any(|path| path == "src/lib.rs"),
        "staged path parsed",
    )?;
    require(
        git.unstaged_paths
            .iter()
            .any(|path| path == "tests/validation_broker_store.rs"),
        "unstaged path parsed",
    )?;
    require(
        git.untracked_paths.iter().any(|path| path == "scratch.txt"),
        "untracked path parsed",
    )?;

    let beads = normalize_beads_json(
        provenance("beads")?,
        &json!({"issues": [
            {"id": "bd-ready", "status": "open", "updated_at": HEARTBEAT},
            {"id": "bd-active", "status": "in_progress", "assignee": "Codex", "updated_at": HEARTBEAT}
        ]}),
        STALE_AT,
        10_000,
    )
    .map_err(to_string)?;
    require(beads.ready_count == 1, "ready bead counted")?;
    require(beads.in_progress.len() == 1, "in-progress bead counted")?;
    require(
        beads.stale_in_progress_ids.is_empty(),
        "fresh in-progress bead not stale",
    )?;

    let scratch_headroom = normalize_headroom_json(
        provenance("scratch_headroom")?,
        &json!({"free_bytes": "60000", "min_required_bytes": "10000"}),
    )
    .map_err(to_string)?;
    let agent_mail = normalize_available_source(provenance("agent_mail")?).map_err(to_string)?;

    let snapshot = ValidationBrokerInputSnapshot::from_parts(ValidationBrokerInputParts {
        captured_at_utc: STALE_AT.to_string(),
        rch,
        cargo_headroom,
        doctor,
        git,
        beads,
        scratch_headroom,
        agent_mail,
    })
    .map_err(to_string)?;
    require(
        snapshot.schema == VALIDATION_BROKER_INPUT_SCHEMA,
        "input snapshot schema",
    )?;
    require(!snapshot.is_degraded(), "all available inputs not degraded")
}

#[test]
fn source_normalizers_make_missing_and_unavailable_inputs_degraded() -> TestResult {
    let rch = normalize_rch_queue_text(provenance("rch")?, "").map_err(to_string)?;
    require(rch.health.is_degraded(), "missing rch degraded")?;

    let cargo_headroom =
        normalize_headroom_json(provenance("cargo_headroom")?, &json!({"free_bytes": 1_u64}))
            .map_err(to_string)?;
    require(
        cargo_headroom.health.is_degraded(),
        "partial headroom degraded",
    )?;

    let doctor = normalize_doctor_json(provenance("doctor")?, &json!({})).map_err(to_string)?;
    require(
        doctor.health.is_degraded(),
        "missing doctor checks degraded",
    )?;

    let git = normalize_git_status_text(provenance("git")?, "3048e53f3", "M malformed")
        .map_err(to_string)?;
    require(git.health.is_degraded(), "missing git branch degraded")?;

    let git = normalize_git_status_text(provenance("git")?, "3048e53f3", "## main\né malformed")
        .map_err(to_string)?;
    require(
        git.health.is_degraded(),
        "unicode malformed git line degraded",
    )?;

    let beads = normalize_beads_json(
        provenance("beads")?,
        &json!({"unexpected": []}),
        STALE_AT,
        3600,
    )
    .map_err(to_string)?;
    require(beads.health.is_degraded(), "missing bead array degraded")?;

    let scratch_headroom = normalize_headroom_json(
        provenance("scratch_headroom")?,
        &json!({"available_bytes": 5_u64, "required_bytes": 10_u64}),
    )
    .map_err(to_string)?;
    require(
        scratch_headroom.low_headroom,
        "low scratch headroom explicit",
    )?;
    require(
        !scratch_headroom.health.is_degraded(),
        "known low scratch headroom remains available source fact",
    )?;

    let mut invalid_mail_provenance = provenance("agent_mail")?;
    invalid_mail_provenance.schema = "wrong.source.provenance".to_string();
    require(
        normalize_unavailable_source(invalid_mail_provenance, "schema missing").is_err(),
        "unavailable source validates provenance",
    )?;

    let agent_mail = normalize_unavailable_source(provenance("agent_mail")?, "schema missing")
        .map_err(to_string)?;
    require(
        agent_mail.state == ValidationSourceState::Unavailable,
        "agent mail unavailable",
    )?;

    let snapshot = ValidationBrokerInputSnapshot::from_parts(ValidationBrokerInputParts {
        captured_at_utc: STALE_AT.to_string(),
        rch,
        cargo_headroom,
        doctor,
        git,
        beads,
        scratch_headroom,
        agent_mail,
    })
    .map_err(to_string)?;
    require(snapshot.is_degraded(), "snapshot degraded")?;
    require(
        snapshot
            .degraded_reasons
            .iter()
            .any(|reason| reason.contains("agent_mail: schema missing")),
        "agent mail degraded reason preserved",
    )
}

#[test]
fn rch_saturation_and_local_fallback_are_explicit_inputs() -> TestResult {
    let rch = normalize_rch_queue_text(
        provenance("rch")?,
        "Build Queue\n  - 3 Active Build(s)\n  - 2 Queued Build(s)\nWorker Availability\n  -> 0 / 18 slots free\nRCH fails open; command may run with local fallback\n",
    )
    .map_err(to_string)?;

    require(rch.saturated, "queued work and zero slots saturate rch")?;
    require(rch.local_fallback, "local fallback detected")?;
    require(rch.health.is_degraded(), "local fallback degrades source")?;
    require(
        rch.health
            .degraded_reasons
            .iter()
            .any(|reason| reason == "rch_local_fallback_detected"),
        "local fallback reason recorded",
    )
}

#[test]
fn validation_broker_large_host_stress_budget_evidence_is_fail_closed() -> TestResult {
    let corpus = load_stress_profile_corpus()?;
    require(
        corpus.schema == "pi.validation_broker.stress_profile_corpus.v1",
        "stress profile corpus schema",
    )?;
    require(
        corpus.caveats.iter().any(|caveat| {
            caveat == "synthetic_large_host_profile_not_release_performance_evidence"
        }),
        "stress corpus carries synthetic evidence caveat",
    )?;
    require(
        corpus.generated_at_utc == STALE_AT,
        "stress corpus timestamp matches deterministic test clock",
    )?;
    let budgets = corpus.budgets.clone();
    let nominal = evaluate_validation_broker_stress_budget(
        stress_profile_by_id(&corpus, "synthetic_64c_256gb_nominal")?,
        budgets.clone(),
        provenance("validation_broker_stress")?,
        STALE_AT,
    )
    .map_err(to_string)?;

    require(
        nominal.schema == VALIDATION_BROKER_STRESS_EVIDENCE_SCHEMA,
        "stress evidence schema",
    )?;
    require(
        nominal.verdict == ValidationBrokerStressVerdict::Pass,
        "nominal large-host profile stays within budgets",
    )?;
    require(nominal.missing_data.is_empty(), "nominal data complete")?;
    require(
        nominal.measurements.plan_latency_ms == Some(8),
        "nominal plan latency estimate is deterministic",
    )?;
    require(
        nominal.measurements.request_throughput_per_minute == Some(1_440),
        "nominal throughput estimate is deterministic",
    )?;
    require(
        nominal
            .no_claims
            .iter()
            .any(|claim| claim == "not_release_performance_evidence"),
        "stress evidence remains non-release evidence",
    )?;
    require(
        !nominal.cache.input_fingerprint.is_empty(),
        "stress evidence carries cache/provenance fingerprint",
    )?;
    require(
        nominal.guards.no_live_rch_mutation
            && nominal.guards.provider_calls == 0
            && nominal.guards.live_mutations == 0
            && !nominal.guards.release_claim_allowed,
        "stress evidence carries no-live-mutation and no-release-claim guards",
    )?;

    let saturated = evaluate_validation_broker_stress_budget(
        stress_profile_by_id(&corpus, "synthetic_64c_256gb_saturated")?,
        budgets.clone(),
        provenance("validation_broker_stress")?,
        STALE_AT,
    )
    .map_err(to_string)?;
    require(
        saturated.verdict == ValidationBrokerStressVerdict::Fail,
        "saturated large-host profile violates budgets",
    )?;
    for expected_failure in [
        "plan_latency_ms_exceeded",
        "stale_scan_ms_exceeded",
        "slot_store_bytes_exceeded",
        "memory_growth_bytes_exceeded",
        "request_throughput_per_minute_below_minimum",
    ] {
        require(
            saturated
                .budget_failures
                .iter()
                .any(|failure| failure == expected_failure),
            format!("saturated profile records {expected_failure}"),
        )?;
    }

    let missing_evidence = evaluate_validation_broker_stress_budget(
        stress_profile_by_id(&corpus, "synthetic_64c_256gb_missing_store_bytes")?,
        budgets,
        provenance("validation_broker_stress")?,
        STALE_AT,
    )
    .map_err(to_string)?;
    require(
        missing_evidence.verdict == ValidationBrokerStressVerdict::Blocked,
        "missing stress inputs block the evidence",
    )?;
    require(
        missing_evidence
            .missing_data
            .iter()
            .any(|field| field == "slot_store_bytes"),
        "missing data names the absent field",
    )?;
    require(
        missing_evidence.measurements.plan_latency_ms.is_none(),
        "blocked evidence does not invent measurements",
    )
}

#[test]
fn beads_normalizer_detects_stale_in_progress_work() -> TestResult {
    let beads = normalize_beads_json(
        provenance("beads")?,
        &json!({"issues": [
            {"id": "bd-fresh", "status": "in_progress", "assignee": "Codex", "updated_at": RENEWED_EXPIRES},
            {"id": "bd-stale", "status": "in_progress", "assignee": "Other", "updated_at": START}
        ]}),
        STALE_AT,
        3600,
    )
    .map_err(to_string)?;

    require(beads.in_progress.len() == 2, "in-progress beads retained")?;
    require(
        beads
            .stale_in_progress_ids
            .iter()
            .any(|id| id == "bd-stale"),
        "stale bead detected",
    )?;
    require(
        !beads
            .stale_in_progress_ids
            .iter()
            .any(|id| id == "bd-fresh"),
        "fresh bead not stale",
    )
}

#[test]
fn admission_allows_when_capacity_and_sources_are_healthy() -> TestResult {
    let decision = decide_validation_admission(
        admission_context("request-allow"),
        &healthy_inputs()?,
        &slot_snapshot(Vec::new()),
        &ValidationAdmissionPolicy::default(),
        STALE_AT,
    )
    .map_err(to_string)?;

    require(
        decision.schema == VALIDATION_BROKER_DECISION_SCHEMA,
        "decision schema",
    )?;
    require(
        matches!(&decision.decision, ValidationAdmissionDecision::Allow),
        "healthy source admission allowed",
    )?;
    require(
        decision
            .no_claims
            .iter()
            .any(|claim| claim == "not_permission_to_skip_required_gates"),
        "decision preserves no-claims",
    )
}

#[cfg(unix)]
#[test]
fn admission_coalesces_reusable_slots() -> TestResult {
    let mut reusable = acquire("slot-reusable-decision")?;
    reusable
        .mark_reusable(
            "SilentReef",
            HEARTBEAT,
            vec![ValidationSlotArtifact {
                path: "target/debug/deps/pi.d".to_string(),
                sha256: Some("artifact-hash-1".to_string()),
                schema: Some("cargo_check_result.v1".to_string()),
            }],
        )
        .map_err(to_string)?;
    let reusable_decision = decide_validation_admission(
        admission_context("request-reusable"),
        &healthy_inputs()?,
        &slot_snapshot(vec![reusable]),
        &ValidationAdmissionPolicy::default(),
        HEARTBEAT,
    )
    .map_err(to_string)?;
    require(
        matches!(
            &reusable_decision.decision,
            ValidationAdmissionDecision::Coalesce
        ),
        "equivalent reusable slot coalesces",
    )?;
    require(
        reusable_decision.reusable_slot.as_deref() == Some("slot-reusable-decision"),
        "reusable slot id recorded",
    )?;
    require(
        reusable_decision.coalesced_artifacts.len() == 1,
        "reusable artifacts carried",
    )?;
    Ok(())
}

#[test]
fn admission_waits_for_equivalent_active_slots() -> TestResult {
    let active = acquire("slot-active-decision")?;
    let active_decision = decide_validation_admission(
        admission_context("request-active"),
        &saturated_inputs()?,
        &slot_snapshot(vec![active]),
        &ValidationAdmissionPolicy::default(),
        HEARTBEAT,
    )
    .map_err(to_string)?;
    require(
        matches!(&active_decision.decision, ValidationAdmissionDecision::Wait),
        "equivalent active slot waits instead of starting duplicate gate",
    )?;
    require(
        active_decision.policy.active_equivalent_slots == 1,
        "equivalent active slot counted",
    )
}

#[test]
fn admission_does_not_coalesce_non_equivalent_git_or_target() -> TestResult {
    let mut different_git = acquire("slot-different-git")?;
    different_git.git_head = "different-head".to_string();
    let decision = decide_validation_admission(
        admission_context("request-different-git"),
        &healthy_inputs()?,
        &slot_snapshot(vec![different_git]),
        &ValidationAdmissionPolicy::default(),
        HEARTBEAT,
    )
    .map_err(to_string)?;

    require(
        !matches!(&decision.decision, ValidationAdmissionDecision::Coalesce),
        "git mismatch does not coalesce",
    )?;
    require(
        matches!(&decision.decision, ValidationAdmissionDecision::Narrow),
        "non-equivalent broad gate narrows under active broad-gate pressure",
    )?;
    require(
        decision.policy.active_equivalent_slots == 0
            && decision.policy.reusable_equivalent_slots == 0,
        "non-equivalent slot not counted",
    )
}

#[test]
fn admission_recovers_stale_slots_and_stale_beads() -> TestResult {
    let stale_slot = acquire("slot-stale-decision")?;
    let slot_decision = decide_validation_admission(
        admission_context("request-stale-slot"),
        &healthy_inputs()?,
        &slot_snapshot(vec![stale_slot]),
        &ValidationAdmissionPolicy::default(),
        STALE_AT,
    )
    .map_err(to_string)?;
    require(
        matches!(
            &slot_decision.decision,
            ValidationAdmissionDecision::StaleRecover
        ),
        "stale slot triggers recovery",
    )?;
    require(
        slot_decision.policy.stale_equivalent_slots == 1,
        "stale equivalent counted",
    )?;

    let bead_decision = decide_validation_admission(
        admission_context("request-stale-bead"),
        &stale_bead_inputs()?,
        &slot_snapshot(Vec::new()),
        &ValidationAdmissionPolicy::default(),
        STALE_AT,
    )
    .map_err(to_string)?;
    require(
        matches!(
            &bead_decision.decision,
            ValidationAdmissionDecision::StaleRecover
        ),
        "stale bead triggers recovery",
    )?;
    require(
        bead_decision.policy.stale_in_progress_beads == 1,
        "stale bead policy field recorded",
    )
}

#[test]
fn admission_waits_or_narrows_under_rch_and_scratch_backpressure() -> TestResult {
    let wait_decision = decide_validation_admission(
        admission_context("request-saturated"),
        &saturated_inputs()?,
        &slot_snapshot(Vec::new()),
        &ValidationAdmissionPolicy {
            allow_narrow_scope: false,
            ..ValidationAdmissionPolicy::default()
        },
        HEARTBEAT,
    )
    .map_err(to_string)?;
    require(
        matches!(&wait_decision.decision, ValidationAdmissionDecision::Wait),
        "saturated rch waits when narrowing disabled",
    )?;

    let narrow_decision = decide_validation_admission(
        admission_context("request-low-scratch"),
        &low_scratch_inputs()?,
        &slot_snapshot(Vec::new()),
        &ValidationAdmissionPolicy::default(),
        HEARTBEAT,
    )
    .map_err(to_string)?;
    require(
        matches!(
            &narrow_decision.decision,
            ValidationAdmissionDecision::Narrow
        ),
        "low scratch headroom narrows broad gate",
    )?;
    require(
        narrow_decision.policy.low_scratch_headroom,
        "low scratch policy field recorded",
    )
}

#[test]
fn admission_priority_or_age_overrides_soft_broad_gate_backpressure() -> TestResult {
    let mut active_request = base_request("slot-other-broad");
    active_request.git_head = "different-head".to_string();
    let active = ValidationSlotLease::acquire(active_request, START, "2026-05-14T10:00:00Z")
        .map_err(to_string)?;
    let store = slot_snapshot(vec![active]);

    let priority_decision = decide_validation_admission(
        admission_context_for("request-high-priority", base_request("slot-high"), START, 1),
        &healthy_inputs()?,
        &store,
        &ValidationAdmissionPolicy::default(),
        HEARTBEAT,
    )
    .map_err(to_string)?;
    require(
        matches!(
            &priority_decision.decision,
            ValidationAdmissionDecision::Allow
        ),
        "high priority broad gate can proceed under soft pressure",
    )?;
    require(
        priority_decision.policy.age_priority_boosted,
        "priority boost recorded",
    )?;

    let aged_decision = decide_validation_admission(
        admission_context_for("request-aged", base_request("slot-aged"), START, 4),
        &healthy_inputs()?,
        &store,
        &ValidationAdmissionPolicy::default(),
        STALE_AT,
    )
    .map_err(to_string)?;
    require(
        matches!(&aged_decision.decision, ValidationAdmissionDecision::Allow),
        "aged broad gate can proceed under soft pressure",
    )?;
    require(
        aged_decision
            .reasons
            .iter()
            .any(|reason| reason == "age_or_priority_boost_overrides_soft_backpressure"),
        "soft pressure override reason recorded",
    )
}

#[test]
fn admission_denies_required_rch_gate_on_local_fallback() -> TestResult {
    let decision = decide_validation_admission(
        admission_context("request-local-fallback"),
        &local_fallback_inputs()?,
        &slot_snapshot(Vec::new()),
        &ValidationAdmissionPolicy::default(),
        HEARTBEAT,
    )
    .map_err(to_string)?;

    require(
        matches!(
            &decision.decision,
            ValidationAdmissionDecision::DenyLocalFallback
        ),
        "rch-required gate denies local fallback",
    )?;
    require(
        decision.source_statuses.iter().any(|status| {
            status.source_id == "rch" && status.state == ValidationSourceState::Degraded
        }),
        "degraded rch source status recorded",
    )
}

#[test]
fn admission_refuses_required_reuse_when_no_valid_artifact_exists() -> TestResult {
    let decision = decide_validation_admission(
        admission_context("request-reuse-required"),
        &healthy_inputs()?,
        &slot_snapshot(Vec::new()),
        &ValidationAdmissionPolicy {
            reuse_required: true,
            ..ValidationAdmissionPolicy::default()
        },
        HEARTBEAT,
    )
    .map_err(to_string)?;

    require(
        matches!(
            &decision.decision,
            ValidationAdmissionDecision::DegradedBlock
        ),
        "required reusable evidence fails closed when absent",
    )?;
    require(
        decision
            .reasons
            .iter()
            .any(|reason| reason == "reuse_required_but_no_valid_reusable_slot"),
        "reuse-required reason recorded",
    )
}

fn require(condition: bool, message: impl Into<String>) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn to_string(err: impl std::fmt::Display) -> String {
    err.to_string()
}
