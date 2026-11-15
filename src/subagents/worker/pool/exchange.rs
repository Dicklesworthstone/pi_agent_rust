//! Admit a bounded pool batch, optionally seal it, and observe one result page.
//!
//! The mutation is committed once before the read phase. Unlike pool_drain,
//! this operation must never be replayed after a lost or unusable response.

use super::{
    AgentCx, AuxiliaryPrivacySnapshot, ContentBlock, Deadline, DrainRequest, Duration, Instant,
    MAX_ITEMS, MAX_SNAPSHOT_BYTES, POLL_INTERVAL, Pool, Result, SubagentTool, TextContent,
    ToolOutput, Value, background, check_owner, commit_batch, drain_snapshot, hold_results,
    invalid, lookup, manifest, optional_integer, prepare_batch, project_drain_snapshot,
    projected_snapshot_with, recheck_owner, required_text, result_dependencies, seal, stopped,
    tasks, validate_fields,
};
use serde_json::json;

pub(super) async fn execute(
    tool: &SubagentTool,
    input: Value,
    privacy: AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    validate_fields(
        &input,
        &[
            "operation",
            "poolId",
            "tasks",
            "seal",
            "cursor",
            "timeoutMs",
            "maxItems",
        ],
    )?;
    let id = required_text(&input, "poolId", 128)?;
    let assignments = tasks(&input, true)?;
    let seal_requested = match input.get("seal") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid("seal must be a boolean")),
    };
    let request = DrainRequest::parse(&input, id)?;
    let timeout_ms = optional_integer(&input, "timeoutMs", 0, 600_000)?.unwrap_or(0);
    let timeout = Duration::from_millis(timeout_ms);
    // Admission, owner resolution and every subsequent wait share this one
    // clock. Neither a slow admission nor a page poll renews the read window.
    let started = Instant::now();
    let budget = Deadline::for_request(Some(Duration::from_millis(timeout_ms + 30_000)), None)?;
    let owner = AgentCx::for_current_or_request();
    check_owner(&owner)?;
    let owner_id = background::resolve_session(&tool.job_session_scope, &owner, budget).await?;
    let pool = lookup(&owner_id, id)?;

    // Parse-time cursor validation alone is insufficient: a future item ID
    // could become valid after this operation's admission. Validate membership,
    // settled state, privacy and the response shape before any native mutation.
    response(&pool, &request, &privacy, true, &[])?;
    let admitted = if assignments.is_empty() {
        if seal_requested {
            seal(tool, &pool, &privacy, &owner, &owner_id, budget).await?;
        }
        Vec::new()
    } else {
        admit_and_seal(
            tool,
            &pool,
            &assignments,
            seal_requested,
            &privacy,
            &owner,
            &owner_id,
            budget,
        )
        .await?
    };

    let mut wait_admitted = false;
    // RAII: a foreground dependency hold cannot outlive this command.
    #[allow(clippy::collection_is_never_read)]
    let mut _waiting = None;
    loop {
        recheck_owner(tool, &owner, &owner_id, budget).await?;
        let expired = started.elapsed() >= timeout;
        let output = response(&pool, &request, &privacy, expired, &admitted)?;
        let details = output.details.as_ref().ok_or_else(stopped)?;
        let ready = details
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
            || details.get("done").and_then(Value::as_bool) == Some(true);
        if ready || expired {
            return Ok(output);
        }
        let remaining = timeout
            .saturating_sub(started.elapsed())
            .min(budget.remaining());
        if !wait_admitted {
            _waiting = hold_results(
                result_dependencies(&pool, None)?,
                request.acknowledged.clone(),
                false,
                remaining,
            )?;
            wait_admitted = true;
        }
        owner.time().sleep(POLL_INTERVAL.min(remaining)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn admit_and_seal(
    tool: &SubagentTool,
    pool: &Pool,
    assignments: &[String],
    seal_requested: bool,
    privacy: &AuxiliaryPrivacySnapshot,
    owner: &AgentCx,
    owner_id: &str,
    budget: Deadline,
) -> Result<Vec<u64>> {
    for _ in 0..MAX_ITEMS {
        let batch = prepare_batch(pool, assignments.len(), privacy, false)?;
        if seal_requested {
            // Admission has already screened the projected new identities.
            // Screen the additional lifecycle change before committing it too.
            let mut value = batch.acknowledgment.details.clone().ok_or_else(stopped)?;
            let fields = value.as_object_mut().ok_or_else(stopped)?;
            fields.insert("sealed".to_string(), json!(true));
            fields.insert("status".to_string(), json!("draining"));
            let policies = pool.lock().map_err(|_| stopped())?.privacy.clone();
            projected_snapshot_with(&value, &policies, privacy)?;
        }
        let admitted = (batch.base + 1..=batch.base + assignments.len())
            .map(|id| u64::try_from(id).map_err(|_| stopped()))
            .collect::<Result<Vec<_>>>()?;
        recheck_owner(tool, owner, owner_id, budget).await?;
        let job_id = pool.lock().map_err(|_| stopped())?.job_id.clone();
        let accepted = crate::jobs::with_owned_task_progress(owner_id, &job_id, || {
            let mut state = pool.lock().map_err(|_| stopped())?;
            let accepted = commit_batch(
                &mut state,
                &batch,
                assignments,
                privacy,
                false,
                seal_requested,
            )?;
            Ok((accepted, None))
        })?;
        if accepted {
            return Ok(admitted);
        }
    }
    Err(invalid(
        "concurrent admissions exhausted the pool's item budget",
    ))
}

fn response(
    pool: &Pool,
    request: &DrainRequest,
    privacy: &AuxiliaryPrivacySnapshot,
    expired: bool,
    admitted: &[u64],
) -> Result<ToolOutput> {
    // The manifest may precede the page, but it always includes this command's
    // admitted identities. Capturing it first ensures page.admittedItems is
    // never smaller than snapshot.admittedItems under concurrent pushes.
    let manifest = {
        let state = pool.lock().map_err(|_| stopped())?;
        manifest(&state, None, false)?
    };
    let page = drain_snapshot(pool, request, privacy, expired)?;
    // New discoveries while the page was built protect BOTH observations,
    // including earlier result summaries retained in the manifest.
    let policies = pool.lock().map_err(|_| stopped())?.privacy.clone();
    let snapshot = projected_snapshot_with(&manifest, &policies, privacy)?
        .details
        .ok_or_else(stopped)?;
    let mut output = project_drain_snapshot(
        page.details.as_ref().ok_or_else(stopped)?,
        &policies,
        privacy,
    )?;
    let fields = output
        .details
        .as_mut()
        .and_then(Value::as_object_mut)
        .ok_or_else(stopped)?;
    fields.insert("admittedItemIds".to_string(), json!(admitted));
    fields.insert("snapshot".to_string(), snapshot);
    output.content = vec![ContentBlock::Text(TextContent::new(
        "Pool exchange accepted once. Process items before using nextCursor. Never replay this mutation after a lost response; use pool_status or pool_drain to recover.",
    ))];
    if serde_json::to_vec(&output).map_err(|_| stopped())?.len() > MAX_SNAPSHOT_BYTES {
        return Err(crate::error::Error::tool(
            "subagent",
            "PI_SUBAGENT_POOL_RESULT_LIMIT: projected pool exchange exceeded its bounded result budget",
        ));
    }
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::drain_cursor;
    use super::super::tests::{Fixture, RPC_CHILD, field, private_projection, runtime};
    use super::*;
    use crate::tools::Tool as _;

    #[test]
    fn malformed_exchange_fields_and_cursors_never_admit_or_seal() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        runtime().block_on(async {
            let started = fixture
                .call(json!({
                    "operation":"pool_start", "agent":"worker", "concurrency":1,
                    "tasks":["hold"],
                }))
                .await;
            let pool = field(&started, "poolId");
            fixture.await_file("launches").await;
            for (key, value) in [
                ("cursor", drain_cursor(&pool, &[1])),
                ("cursor", drain_cursor(&pool, &[2])),
                ("cursor", drain_cursor("foreign-pool", &[])),
                ("cursor", drain_cursor(&pool, &[1, 1])),
                ("tasks", json!(["valid prefix", ""])),
                ("tasks", Value::Null),
                ("tasks", json!(vec!["overflow"; 32])),
                ("seal", json!("true")),
                ("maxItems", json!(0)),
                ("maxItems", json!(5)),
                ("timeoutMs", json!(-1)),
                ("unsupported", json!(true)),
            ] {
                let mut input = json!({
                    "operation":"pool_exchange", "poolId":pool,
                    "tasks":["must not execute"], "seal":true,
                });
                input[key] = value;
                assert!(
                    fixture
                        .tool
                        .execute("invalid-exchange", input, None)
                        .await
                        .is_err()
                ); // ubs:ignore[rust.panic.assert-macros] -- Invalid tails and acknowledgment state must reject the complete mutation.
                let observed = fixture
                    .call(json!({"operation":"pool_status","poolId":pool}))
                    .await;
                assert_eq!(observed["admittedItems"], 1); // ubs:ignore[rust.panic.assert-macros] -- No rejected mutation may consume a turn or lifetime capacity.
                assert_eq!(observed["sealed"], false); // ubs:ignore[rust.panic.assert-macros] -- An invalid read request cannot close otherwise valid admission.
            }
            fixture
                .call(json!({"operation":"pool_close","poolId":pool}))
                .await;
            fixture.assert_reaped();
        });
    }

    #[test]
    fn exchanges_run_thirty_two_incremental_turns_and_seal_the_last_batch() {
        let _guard = crate::jobs::process_test_guard();
        let mut fixture = Fixture::new(RPC_CHILD);
        fixture.tool = fixture.tool.with_timeout(Duration::from_secs(180));
        fixture.release();
        runtime().block_on(async {
            let started = fixture
                .call(json!({"operation":"pool_start","agent":"worker","concurrency":1}))
                .await;
            let pool = field(&started, "poolId");
            let mut cursor = drain_cursor(&pool, &[]);
            for id in 1_u64..=32 {
                let page = fixture
                    .call(json!({
                        "operation":"pool_exchange", "poolId":pool,
                        "tasks":["recall"], "seal":id == 32, "cursor":cursor,
                        "timeoutMs":5000, "maxItems":1,
                    }))
                    .await;
                assert_eq!(page["schema"], "pi.subagent.pool.drain.v1"); // ubs:ignore[rust.panic.assert-macros] -- The native page remains consumable by ordinary drain readers.
                assert_eq!(page["admittedItemIds"], json!([id])); // ubs:ignore[rust.panic.assert-macros] -- One batch creates one stable global identity.
                assert_eq!(page["snapshot"]["admittedItems"], id); // ubs:ignore[rust.panic.assert-macros] -- Guest handles recover from the genuine native manifest.
                assert_eq!(page["items"][0]["id"], id); // ubs:ignore[rust.panic.assert-macros] -- The same call observes the actual accepted child's result.
                assert_eq!(page["items"][0]["status"], "completed"); // ubs:ignore[rust.panic.assert-macros] -- An admission acknowledgment alone is not an execution result.
                assert_eq!(page["sealed"], id == 32); // ubs:ignore[rust.panic.assert-macros] -- The last admission seals without a separate host call.
                cursor = page["nextCursor"].clone();
            }
            let terminal = fixture
                .call(json!({
                    "operation":"pool_drain","poolId":pool,"cursor":cursor,
                    "timeoutMs":5000,
                }))
                .await;
            assert_eq!(terminal["done"], true); // ubs:ignore[rust.panic.assert-macros] -- Terminal delivery waits for every retained child to be reaped.
            fixture.assert_reaped();
            let prompts = std::fs::read_to_string(fixture.root.path().join("prompts"))
                .expect("recorded native prompts"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The child records every real prompt independently of its native manifest.
            assert_eq!(prompts.lines().count(), 32); // ubs:ignore[rust.panic.assert-macros] -- Compound observations cannot duplicate a task.
            let repeated = fixture
                .call(json!({"operation":"pool_exchange","poolId":pool,"maxItems":1}))
                .await;
            assert_eq!(repeated["admittedItemIds"], json!([])); // ubs:ignore[rust.panic.assert-macros] -- Empty exchange remains a bounded observation after completion.
            assert_eq!(repeated["items"][0]["id"], 1); // ubs:ignore[rust.panic.assert-macros] -- Reads never silently advance the caller-owned cursor.
            assert_eq!(repeated["snapshot"]["admittedItems"], 32); // ubs:ignore[rust.panic.assert-macros] -- Result recovery preserves the complete admitted membership.
        });
    }

    #[test]
    fn exchange_screens_seal_identity_and_retains_batch_privacy_for_the_whole_response() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        fixture.release();
        runtime().block_on(async {
            let started = fixture
                .call(json!({
                    "operation":"pool_start","agent":"worker","concurrency":1,
                    "tasks":["remember silver"],
                }))
                .await;
            let pool = field(&started, "poolId");
            fixture
                .call(json!({"operation":"pool_wait","poolId":pool,"itemId":1,"timeoutMs":5000}))
                .await;
            let rejected = execute(
                &fixture.tool,
                json!({"operation":"pool_exchange","poolId":pool,"tasks":["recall"],"seal":true}),
                private_projection("draining"),
            )
            .await;
            assert!(rejected.is_err()); // ubs:ignore[rust.panic.assert-macros] -- An undisclosable sealed acknowledgment cannot commit either mutation.
            let unchanged = fixture
                .call(json!({"operation":"pool_status","poolId":pool}))
                .await;
            assert_eq!(unchanged["admittedItems"], 1); // ubs:ignore[rust.panic.assert-macros] -- Privacy refusal must not enqueue the proposed batch.
            assert_eq!(unchanged["sealed"], false); // ubs:ignore[rust.panic.assert-macros] -- Privacy refusal must not seal the existing pool.
            let output = execute(
                &fixture.tool,
                json!({
                    "operation":"pool_exchange","poolId":pool,"tasks":["recall"],
                    "seal":true,"maxItems":1,"timeoutMs":5000,
                }),
                private_projection("silver"),
            )
            .await
            .expect("privacy-screened exchange"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Only the native admitted operation supplies these identities.
            let encoded = serde_json::to_string(&output).expect("complete exchange JSON"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Inspect both result content and attached manifest summaries.
            assert!(!encoded.contains("silver")); // ubs:ignore[rust.panic.assert-macros] -- Later batch policy protects earlier results in every envelope field.
            assert!(encoded.contains("<pi-secret:redacted>")); // ubs:ignore[rust.panic.assert-macros] -- Privacy projection retains usable completed output.
            let retained = fixture
                .call(json!({"operation":"pool_wait","poolId":pool,"timeoutMs":5000}))
                .await;
            assert!(
                !serde_json::to_string(&retained)
                    .expect("retained aggregate")
                    .contains("silver")
            ); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- A weaker later observer cannot undo an accepted batch's privacy floor.
            fixture.assert_reaped();
        });
    }
}
