#![forbid(unsafe_code)]

use jsonschema::Validator;
use pi::session_store_v2::{MigrationEvent, MigrationVerification, SessionStoreV2};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn schema_path() -> PathBuf {
    repo_root().join("docs/schema/session_store_v2_contract.json")
}

fn compiled_contract_schema() -> Validator {
    let path = schema_path();
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("Failed to read schema {}: {err}", path.display()));
    let schema: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("Failed to parse schema {}: {err}", path.display()));

    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|err| panic!("Failed to compile schema {}: {err}", path.display()))
}

fn read_jsonl_values(path: &std::path::Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("Failed to read JSONL {}: {err}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("Failed to parse JSONL {}: {err}", path.display()))
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
fn canonical_contract_bundle() -> Value {
    json!({
        "schema": "pi.session_store_v2.contract.v1",
        "manifest": {
            "schema": "pi.session_store_v2.manifest.v1",
            "storeVersion": 2,
            "sessionId": "f4c03c8c-cf0a-4c90-9535-e95f2d02b393",
            "sourceFormat": "jsonl_v3",
            "createdAt": "2026-02-15T18:10:00Z",
            "updatedAt": "2026-02-15T18:10:10Z",
            "head": {
                "segmentSeq": 2,
                "entrySeq": 2,
                "entryId": "entry_00000002"
            },
            "counters": {
                "entriesTotal": 2,
                "messagesTotal": 1,
                "branchesTotal": 0,
                "compactionsTotal": 0,
                "bytesTotal": 448
            },
            "files": {
                "segmentDir": "segments/",
                "segmentCount": 2,
                "indexPath": "index/offsets.jsonl",
                "checkpointDir": "checkpoints/",
                "migrationLedgerPath": "migrations/ledger.jsonl"
            },
            "integrity": {
                "chainHash": "f7a3b79f9d1b84444c34f6f1393ba55fba8c4d0868ac8f80a7b951907e8095",
                "manifestHash": "7d6cf8f3ad3f8bc9f5a4191efebda5b2236a3a4387fa31dd619f024997f30871",
                "lastCrc32c": "1A2B3C4D"
            },
            "invariants": {
                "parentLinksClosed": true,
                "monotonicEntrySeq": true,
                "monotonicSegmentSeq": true,
                "indexWithinSegmentBounds": true,
                "branchHeadsIndexed": true,
                "checkpointsMonotonic": true,
                "hashChainValid": true
            }
        },
        "segments": [
            {
                "schema": "pi.session_store_v2.segment_frame.v1",
                "segmentSeq": 1,
                "frameSeq": 1,
                "entrySeq": 1,
                "entryId": "entry_00000001",
                "parentEntryId": null,
                "entryType": "message",
                "timestamp": "2026-02-15T18:10:00Z",
                "payloadSha256": "2d66234f0f7a6f4fcf5b37ab54fef9cb79373ca4ac75734f84f3f1a8ac26bf58",
                "payloadBytes": 30,
                "payload": {
                    "role": "user",
                    "text": "hello"
                }
            },
            {
                "schema": "pi.session_store_v2.segment_frame.v1",
                "segmentSeq": 2,
                "frameSeq": 1,
                "entrySeq": 2,
                "entryId": "entry_00000002",
                "parentEntryId": "entry_00000001",
                "entryType": "session_info",
                "timestamp": "2026-02-15T18:10:10Z",
                "payloadSha256": "4d94e98ec87dbb5e7fb5952a322f6303f65895d15fd8ff81a9f65ee31c6db331",
                "payloadBytes": 21,
                "payload": {
                    "name": "v2-session"
                }
            }
        ],
        "offsetIndex": [
            {
                "schema": "pi.session_store_v2.offset_index.v1",
                "entrySeq": 1,
                "entryId": "entry_00000001",
                "segmentSeq": 1,
                "frameSeq": 1,
                "byteOffset": 0,
                "byteLength": 256,
                "crc32c": "1A2B3C4D",
                "state": "active"
            },
            {
                "schema": "pi.session_store_v2.offset_index.v1",
                "entrySeq": 2,
                "entryId": "entry_00000002",
                "segmentSeq": 2,
                "frameSeq": 1,
                "byteOffset": 0,
                "byteLength": 192,
                "crc32c": "9ABCDEFF",
                "state": "active"
            }
        ],
        "checkpoints": [
            {
                "schema": "pi.session_store_v2.checkpoint.v1",
                "checkpointSeq": 1,
                "at": "2026-02-15T18:10:10Z",
                "headEntrySeq": 2,
                "headEntryId": "entry_00000002",
                "snapshotRef": "checkpoints/0000000000000001.json",
                "compactedBeforeEntrySeq": 0,
                "chainHash": "f7a3b79f9d1b84444c34f6f6f1393ba55fba8c4d0868ac8f80a7b951907e8095",
                "reason": "pre_migration"
            }
        ],
        "migrationEvents": [
            {
                "schema": "pi.session_store_v2.migration_event.v1",
                "migrationId": "4dbf9c6b-c165-4f28-a69a-91f8a8e388e2",
                "phase": "completed",
                "at": "2026-02-15T18:11:00Z",
                "sourcePath": "sessions/legacy.jsonl",
                "targetPath": "sessions/f4c03c8c.v2/",
                "sourceFormat": "jsonl_v3",
                "targetFormat": "native_v2",
                "verification": {
                    "entryCountMatch": true,
                    "hashChainMatch": true,
                    "indexConsistent": true
                },
                "outcome": "ok",
                "errorClass": null,
                "correlationId": "mig_20260215_181100_f4c03c8c"
            }
        ],
        "stateTransitions": [
            {
                "fromState": "CLEAN",
                "toState": "MIGRATION_STAGING",
                "reason": "begin migration",
                "at": "2026-02-15T18:10:59Z"
            },
            {
                "fromState": "MIGRATION_STAGING",
                "toState": "MIGRATED",
                "reason": "cutover commit",
                "at": "2026-02-15T18:11:00Z"
            },
            {
                "fromState": "MIGRATED",
                "toState": "DIRTY",
                "reason": "new append",
                "at": "2026-02-15T18:11:01Z"
            }
        ]
    })
}

#[test]
fn session_store_v2_contract_bundle_validates() {
    let validator = compiled_contract_schema();
    let bundle = canonical_contract_bundle();

    if let Err(err) = validator.validate(&bundle) {
        panic!("Canonical session store V2 contract bundle must validate: {err}");
    }
}

#[test]
fn empty_store_contract_uses_a_coupled_zero_head_and_empty_artifact_sets() {
    let validator = compiled_contract_schema();
    let mut bundle = canonical_contract_bundle();
    bundle["manifest"]["head"] = json!({
        "segmentSeq": 0,
        "entrySeq": 0,
        "entryId": ""
    });
    bundle["manifest"]["counters"] = json!({
        "entriesTotal": 0,
        "messagesTotal": 0,
        "branchesTotal": 0,
        "compactionsTotal": 0,
        "bytesTotal": 0
    });
    bundle["manifest"]["files"]["segmentCount"] = json!(0);
    bundle["segments"] = json!([]);
    bundle["offsetIndex"] = json!([]);
    bundle["checkpoints"] = json!([]);
    bundle["migrationEvents"] = json!([]);
    bundle["stateTransitions"] = json!([]);
    assert!(
        validator.validate(&bundle).is_ok(),
        "the explicit empty-store representation must validate"
    );

    bundle["manifest"]["head"]["entryId"] = json!("entry_00000001");
    assert!(
        validator.validate(&bundle).is_err(),
        "an empty store must not carry a non-empty head"
    );
}

#[test]
fn serialized_runtime_artifacts_validate_against_contract() {
    let temp_dir = tempfile::tempdir().expect("create V2 contract tempdir");
    let root = temp_dir.path().join("runtime.v2");
    let mut store = SessionStoreV2::create(&root, 4096).expect("create V2 store");
    store
        .append_entry(
            "entry_00000001",
            None,
            "message",
            json!({"role": "user", "text": "runtime wire contract"}),
        )
        .expect("append runtime frame");
    store
        .create_checkpoint(1, "manual")
        .expect("create runtime checkpoint");
    store
        .append_migration_event(MigrationEvent {
            schema: "pi.session_store_v2.migration_event.v1".to_string(),
            migration_id: "4dbf9c6b-c165-4f28-a69a-91f8a8e388e2".to_string(),
            phase: "completed".to_string(),
            at: "2026-02-15T18:11:00Z".to_string(),
            source_path: "sessions/runtime.jsonl".to_string(),
            target_path: "sessions/runtime.v2/".to_string(),
            source_format: "jsonl_v3".to_string(),
            target_format: "native_v2".to_string(),
            verification: MigrationVerification {
                entry_count_match: true,
                hash_chain_match: true,
                index_consistent: true,
            },
            outcome: "ok".to_string(),
            error_class: None,
            correlation_id: "mig_runtime_contract_0001".to_string(),
        })
        .expect("append runtime migration event");
    store
        .write_manifest("f4c03c8c-cf0a-4c90-9535-e95f2d02b393", "native_v2")
        .expect("write runtime manifest");

    let bundle = json!({
        "schema": "pi.session_store_v2.contract.v1",
        "manifest": serde_json::from_slice::<Value>(
            &fs::read(root.join("manifest.json")).expect("read runtime manifest")
        )
        .expect("parse runtime manifest"),
        "segments": read_jsonl_values(&root.join("segments/0000000000000001.seg")),
        "offsetIndex": read_jsonl_values(&root.join("index/offsets.jsonl")),
        "checkpoints": [serde_json::from_slice::<Value>(
            &fs::read(root.join("checkpoints/0000000000000001.json"))
                .expect("read runtime checkpoint")
        )
        .expect("parse runtime checkpoint")],
        "migrationEvents": read_jsonl_values(&root.join("migrations/ledger.jsonl")),
        "stateTransitions": [{
            "fromState": "CLEAN",
            "toState": "MIGRATION_STAGING",
            "reason": "runtime artifact schema validation",
            "at": "2026-02-15T18:10:59Z"
        }]
    });

    if let Err(err) = compiled_contract_schema().validate(&bundle) {
        panic!("Serialized runtime V2 artifacts must validate against the contract: {err}");
    }
}

#[test]
fn contract_fails_closed_when_required_section_missing() {
    let validator = compiled_contract_schema();
    let mut bundle = canonical_contract_bundle();
    bundle
        .as_object_mut()
        .expect("bundle object")
        .remove("migrationEvents");

    assert!(
        validator.validate(&bundle).is_err(),
        "missing migrationEvents must fail validation"
    );
}

#[test]
fn invalid_transition_is_rejected() {
    let validator = compiled_contract_schema();
    let mut bundle = canonical_contract_bundle();
    let transitions = bundle["stateTransitions"]
        .as_array_mut()
        .expect("state_transitions array");
    transitions[0]["fromState"] = json!("DIRTY");
    transitions[0]["toState"] = json!("MIGRATED");

    assert!(
        validator.validate(&bundle).is_err(),
        "DIRTY -> MIGRATED must be rejected by transition rules"
    );
}

#[test]
fn manifest_store_version_must_remain_v2() {
    let validator = compiled_contract_schema();
    let mut bundle = canonical_contract_bundle();
    bundle["manifest"]["storeVersion"] = json!(3);

    assert!(
        validator.validate(&bundle).is_err(),
        "manifest.storeVersion != 2 must fail validation"
    );
}
