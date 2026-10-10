//! Atomic memory mutations across the primary row, FTS index, and audit log.
//!
//! Begin before reading for deduplication or checking a superseded row. Engines
//! using optimistic snapshots can still reject COMMIT; restart those conflicts
//! so the action rechecks its invariants against the winning transaction.

use crate::error::{Error, Result};
use crate::session_sqlite::SqliteConnection;
use fsqlite::FrankenError as SqliteError;

struct PendingTransaction<'a> {
    conn: &'a SqliteConnection,
    committed: bool,
}

impl Drop for PendingTransaction<'_> {
    fn drop(&mut self) {
        if !self.committed
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.conn.execute_raw("ROLLBACK")
            }))
            .map_or(true, |result| result.is_err())
        {
            // Cleanup must not replace the original failure or double-panic.
            // Do not log SQL or retained content from an engine diagnostic.
            tracing::warn!("memory transaction rollback failed");
        }
    }
}
/// Publish a mutation only after all its statements have committed.
///
/// Keep the transaction guard outside the unwind boundary: engine cleanup must
/// run after the action's unwind has stopped, not from a destructor on that
/// unwind. Resume the original panic only after rollback has been attempted.
/// This does not claim to repair an engine already poisoned by its own panic.
///
/// Restart commit-time snapshot conflicts at most twice. The action must keep
/// its effects inside the transaction; each retry rechecks reads on a fresh
/// snapshot. Action errors and non-conflict commit errors are never retried.
pub(super) fn run<T>(
    conn: &SqliteConnection,
    mut action: impl FnMut(&SqliteConnection) -> Result<T>,
) -> Result<T> {
    const MAX_ATTEMPTS: usize = 3;
    let mut attempt = 0;
    loop {
        attempt += 1;
        conn.execute_raw("BEGIN IMMEDIATE")
            .map_err(|error| Error::tool("memory", format!("begin transaction failed: {error}")))?;
        let mut pending = PendingTransaction {
            conn,
            committed: false,
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let result = action(conn)?;
            Ok::<_, Error>(conn.execute_raw("COMMIT").map(|()| {
                pending.committed = true;
                result
            }))
        }));
        // Deliberately before resume_unwind, on success as well as failure.
        // Rolling back inside a catch nested in Drop would still run on the
        // outer unwind.
        drop(pending);
        match outcome {
            Ok(Ok(Ok(result))) => return Ok(result),
            Ok(Ok(Err(error)))
                if matches!(
                    error,
                    SqliteError::BusySnapshot { .. } | SqliteError::SerializationFailure { .. }
                ) && attempt < MAX_ATTEMPTS => {}
            Ok(Ok(Err(error))) => {
                return Err(Error::tool(
                    "memory",
                    format!("commit transaction failed: {error}"),
                ));
            }
            Ok(Err(error)) => return Err(error),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_sqlite::run_on_sqlite_thread;

    fn count(conn: &SqliteConnection) -> i64 {
        let rows = conn.query_sync("SELECT COUNT(*) FROM facts", &[]).unwrap();
        match &rows[0].values()[0] {
            fsqlite::SqliteValue::Integer(count) => *count,
            value => panic!("expected count, got {value:?}"),
        }
    }

    #[test]
    fn committed_mutation_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bank.sqlite");
        run_on_sqlite_thread(|| {
            let conn = SqliteConnection::open_read_write(&path).unwrap();
            conn.execute_raw("CREATE TABLE facts (id INTEGER PRIMARY KEY, content TEXT)")
                .unwrap();
            let result = run(&conn, |conn| {
                conn.execute_raw("INSERT INTO facts VALUES (1, 'durable')")
                    .unwrap();
                Ok(17)
            })?;
            assert_eq!(result, 17);
            conn.close().unwrap();
            let reopened = SqliteConnection::open_read_write(&path).unwrap();
            assert_eq!(count(&reopened), 1);
            reopened.close().unwrap();
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn late_statement_failure_rolls_back_earlier_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bank.sqlite");
        run_on_sqlite_thread(|| {
            let conn = SqliteConnection::open_read_write(&path).unwrap();
            conn.execute_raw("CREATE TABLE facts (id INTEGER PRIMARY KEY, content TEXT)")
                .unwrap();
            let result: Result<()> = run(&conn, |conn| {
                conn.execute_raw("INSERT INTO facts VALUES (1, 'must roll back')")
                    .unwrap();
                conn.execute_raw("INSERT INTO missing_audit_table VALUES (1)")
                    .map_err(|error| Error::tool("memory", format!("audit failed: {error}")))?;
                Ok(())
            });
            assert!(result.unwrap_err().to_string().contains("audit failed"));
            assert_eq!(count(&conn), 0);
            run(&conn, |conn| {
                conn.execute_raw("INSERT INTO facts VALUES (2, 'retry')")
                    .unwrap();
                Ok(())
            })?;
            conn.close().unwrap();
            let reopened = SqliteConnection::open_read_write(&path).unwrap();
            assert_eq!(count(&reopened), 1);
            reopened.close().unwrap();
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn unwinding_a_mutation_rolls_back_and_releases_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bank.sqlite");
        run_on_sqlite_thread(|| {
            let conn = SqliteConnection::open_read_write(&path).unwrap();
            conn.execute_raw("CREATE TABLE facts (id INTEGER PRIMARY KEY, content TEXT)")
                .unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _panic_guard = crate::crash::SuppressPanicHook::new();
                let _: Result<()> = run(&conn, |conn| {
                    conn.execute_raw("INSERT INTO facts VALUES (1, 'uncommitted')")
                        .unwrap();
                    panic!("intentional memory mutation failure");
                });
            }));
            let payload = result.expect_err("the original panic must be propagated");
            assert_eq!(
                payload.downcast_ref::<&str>(),
                Some(&"intentional memory mutation failure")
            );
            assert!(!std::thread::panicking());
            assert_eq!(count(&conn), 0);
            run(&conn, |conn| {
                conn.execute_raw("INSERT INTO facts VALUES (2, 'retry after panic')")
                    .unwrap();
                Ok(())
            })?;
            conn.close().unwrap();
            let reopened = SqliteConnection::open_read_write(&path).unwrap();
            assert_eq!(count(&reopened), 1);
            let rows = reopened.query_sync("SELECT id FROM facts", &[]).unwrap();
            assert_eq!(crate::memory::row_i64(&rows[0], 0)?, 2);
            reopened.close().unwrap();
            Ok(())
        })
        .unwrap();
    }
}

#[cfg(test)]
mod store_tests {
    use crate::memory::{MemoryEditOp, MemoryEditTool, MemoryKind, MemoryStore, RetainTool};
    use crate::tools::Tool as _;
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    fn store(root: &Path) -> MemoryStore {
        // The entire real bank stays in a unique test directory, not global state.
        MemoryStore {
            db_path: root.join("bank.sqlite"),
            project_key: "transaction-fixture".to_string(),
            project_root: root.to_path_buf(),
            secret_patterns: Vec::new(),
        }
    }

    fn break_audit_schema(bank: &MemoryStore) {
        bank.with_conn(|conn| {
            conn.execute_raw(
                "ALTER TABLE memory_audit RENAME TO previous_audit; \
                 CREATE TABLE memory_audit (wrong_column INTEGER)",
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
    }

    fn audit_ops(bank: &MemoryStore, id: i64) -> Vec<String> {
        bank.with_conn(|conn| {
            let rows = conn
                .query_sync(
                    "SELECT op FROM memory_audit WHERE memory_id = ?1 ORDER BY id",
                    &[fsqlite::SqliteValue::Integer(id)],
                )
                .unwrap();
            rows.iter()
                .map(|row| crate::memory::row_text(row, 0))
                .collect()
        })
        .unwrap()
    }

    #[test]
    fn project_mutations_screen_rows_tags_and_fts_before_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let settings = crate::secrets::SecretsSettings {
            mode: Some("off".to_string()),
            extra_patterns: Some(vec![r"^ACME-\d{6}$".to_string()]),
        };
        let bank = Arc::new(store(dir.path()).with_secrets_settings(Some(&settings)));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let retained = runtime
            .block_on(RetainTool::new(Arc::clone(&bank)).execute(
                "retain-screen",
                serde_json::json!({
                    "content": "parser api_key=opaqueCredential1234567",
                    "tags": ["ACME-123456", "parser"]
                }),
                None,
            ))
            .unwrap();
        assert!(!retained.is_error);
        let details = retained.details.unwrap();
        assert_eq!(details["content"], "parser api_key=[REDACTED_GENERIC_SECRET]");
        assert_eq!(details["tags"][0], "[REDACTED_USER_PATTERN]");
        assert_eq!(details["tags"][1], "parser");
        let id = details["id"].as_i64().unwrap();

        let edited = runtime
            .block_on(MemoryEditTool::new(Arc::clone(&bank)).execute(
                "edit-screen",
                serde_json::json!({
                    "id": id,
                    "op": "update",
                    "content": "parser credential\n-----BEGIN PRIVATE KEY-----\nPRIVATE_BODY_CANARY\n-----END PRIVATE KEY-----\nkeep the surrounding guidance"
                }),
                None,
            ))
            .unwrap();
        assert!(!edited.is_error);
        let replacement = bank
            .supersede(
                id,
                MemoryKind::Decision,
                "parser connects via postgres://fixture:dsnPassword123@db.example/app",
                &["ACME-654321".to_string()],
                None,
            )
            .unwrap();
        assert_eq!(
            replacement.content,
            "parser connects via [REDACTED_DSN]db.example/app"
        );
        assert_eq!(replacement.tags, vec!["[REDACTED_USER_PATTERN]".to_string()]);

        // Read the real tables directly: screened read projections must not
        // disguise a write that already persisted a raw credential.
        bank.with_conn(|conn| {
            for sql in [
                "SELECT content, tags FROM memories",
                "SELECT content, '' FROM memories_fts",
            ] {
                for row in conn.query_sync(sql, &[]).unwrap() {
                    for index in 0..2 {
                        let stored = crate::memory::row_text(&row, index)?;
                        for secret in [
                            "opaqueCredential1234567",
                            "PRIVATE_BODY_CANARY",
                            "dsnPassword123",
                            "ACME-123456",
                            "ACME-654321",
                        ] {
                            assert!(!stored.contains(secret), "raw credential reached storage");
                        }
                    }
                }
            }
            Ok(())
        })
        .unwrap();
        let reopened = store(dir.path()).with_secrets_settings(Some(&settings));
        let listed = reopened.list(10).unwrap();
        let previous = listed.iter().find(|memory| memory.id == id).unwrap();
        assert!(previous.content.contains("[REDACTED_PRIVATE_KEY]"));
        assert!(previous.content.ends_with("keep the surrounding guidance"));
        assert_eq!(previous.status, "superseded");
        let hits = reopened.recall("parser", None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, replacement.id);
    }

    #[test]
    fn legacy_project_reads_screen_before_budgets_without_rewriting_storage() {
        let dir = tempfile::tempdir().unwrap();
        let bank = store(dir.path());
        let original = bank
            .retain(MemoryKind::Fact, "legacy parser guidance", &[], None)
            .unwrap();
        let legacy = format!(
            "legacy parser before\n-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\nuse the parser safely",
            "PRIVATE_BODY_CANARY".repeat(400)
        );
        bank.with_conn(|conn| {
            conn.execute_sync(
                "UPDATE memories SET content = ?1, tags = ?2 WHERE id = ?3",
                &[
                    fsqlite::SqliteValue::Text(legacy.clone().into()),
                    fsqlite::SqliteValue::Text("[\"ACME-123456\"]".to_string().into()),
                    fsqlite::SqliteValue::Integer(original.id),
                ],
            )
            .unwrap();
            conn.execute_sync(
                "UPDATE memories_fts SET content = ?1 WHERE rowid = ?2",
                &[
                    fsqlite::SqliteValue::Text(legacy.clone().into()),
                    fsqlite::SqliteValue::Integer(original.id),
                ],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        let settings = crate::secrets::SecretsSettings {
            mode: Some("block".to_string()),
            extra_patterns: Some(vec![r"^ACME-\d{6}$".to_string()]),
        };
        let reopened = store(dir.path()).with_secrets_settings(Some(&settings));
        for memories in [
            reopened.list(10).unwrap(),
            reopened.recall("parser", None).unwrap(),
        ] {
            assert_eq!(memories.len(), 1);
            assert_eq!(
                memories[0].content,
                "legacy parser before\n[REDACTED_PRIVATE_KEY]\nuse the parser safely"
            );
            assert_eq!(memories[0].tags, vec!["[REDACTED_USER_PATTERN]".to_string()]);
        }
        let model = reopened.mental_model().unwrap();
        assert!(model.contains("use the parser safely"));
        assert!(model.contains("[REDACTED_PRIVATE_KEY]"));
        assert!(!model.contains("PRIVATE_BODY_CANARY"));
        assert!(model.len() < crate::memory::MENTAL_MODEL_BUDGET);
        reopened
            .with_conn(|conn| {
                let rows = conn
                    .query_sync(
                        "SELECT content FROM memories WHERE id = ?1",
                        &[fsqlite::SqliteValue::Integer(original.id)],
                    )
                    .unwrap();
                assert_eq!(crate::memory::row_text(&rows[0], 0)?, legacy);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn failed_retain_audit_rolls_back_primary_and_fts_rows() {
        let dir = tempfile::tempdir().unwrap();
        let bank = store(dir.path());
        break_audit_schema(&bank);
        let error = bank
            .retain(MemoryKind::Fact, "uncommitted discovery", &[], None)
            .unwrap_err();
        assert!(error.to_string().contains("audit write failed"));
        let reopened = store(dir.path());
        assert!(reopened.list(10).unwrap().is_empty());
        assert!(reopened.recall("discovery", None).unwrap().is_empty());
        reopened
            .with_conn(|conn| {
                let rows = conn
                    .query_sync("SELECT COUNT(*) FROM memories_fts", &[])
                    .unwrap();
                assert_eq!(crate::memory::row_i64(&rows[0], 0)?, 0);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn failed_edit_audit_preserves_original_record_and_search_index() {
        for operation in [
            MemoryEditOp::Update,
            MemoryEditOp::Invalidate,
            MemoryEditOp::Forget,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let bank = store(dir.path());
            let original = bank
                .retain(MemoryKind::Fact, "original searchable fact", &[], None)
                .unwrap();
            break_audit_schema(&bank);
            let error = bank
                .edit(original.id, operation, Some("replacement wording"))
                .unwrap_err();
            assert!(error.to_string().contains("audit write failed"), "{error}");
            let reopened = store(dir.path());
            let listed = reopened.list(10).unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].content, original.content);
            assert_eq!(listed[0].status, "active");
            assert_eq!(listed[0].updated_at_ms, original.updated_at_ms);
            assert_eq!(
                reopened.recall("original", None).unwrap()[0].id,
                original.id
            );
            assert!(reopened.recall("replacement", None).unwrap().is_empty());
        }
    }

    #[test]
    fn supersession_commits_a_linked_active_fact_and_keeps_auditable_history() {
        let dir = tempfile::tempdir().unwrap();
        let bank = store(dir.path());
        let old = bank
            .retain(MemoryKind::Fact, "parser uses legacy grammar", &[], None)
            .unwrap();
        let new = bank
            .supersede(
                old.id,
                MemoryKind::Decision,
                "parser uses modern grammar",
                &[],
                Some("session-b"),
            )
            .unwrap();
        assert_ne!(new.id, old.id);
        assert_eq!(new.supersedes, Some(old.id));
        assert_eq!(new.session_id.as_deref(), Some("session-b"));
        let reopened = store(dir.path());
        let hits = reopened.recall("parser", None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, new.id);
        assert!(reopened.mental_model().unwrap().contains("modern grammar"));
        assert!(!reopened.mental_model().unwrap().contains("legacy grammar"));
        let history = reopened.list(10).unwrap();
        assert!(
            history
                .iter()
                .any(|row| row.id == old.id && row.status == "superseded")
        );
        assert_eq!(audit_ops(&reopened, old.id), ["retain", "supersede"]);
        assert_eq!(audit_ops(&reopened, new.id), ["retain"]);
        let error = reopened
            .supersede(
                old.id,
                MemoryKind::Fact,
                "stale competing replacement",
                &[],
                None,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("PI_MEMORY_SUPERSESSION_CONFLICT")
        );
        assert_eq!(reopened.list(10).unwrap().len(), 2);
    }

    #[test]
    fn failed_supersession_does_not_tombstone_the_predecessor() {
        let dir = tempfile::tempdir().unwrap();
        let bank = store(dir.path());
        let old = bank
            .retain(MemoryKind::Fact, "original parser", &[], None)
            .unwrap();
        break_audit_schema(&bank);
        assert!(
            bank.supersede(old.id, MemoryKind::Fact, "new parser", &[], None)
                .is_err()
        );
        let reopened = store(dir.path());
        let history = reopened.list(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "active");
        assert_eq!(reopened.recall("original", None).unwrap()[0].id, old.id);
        assert!(reopened.recall("new", None).unwrap().is_empty());
    }

    #[test]
    fn concurrent_store_instances_cannot_commit_duplicate_active_facts() {
        let dir = tempfile::tempdir().unwrap();
        let bank = store(dir.path());
        bank.list(1).unwrap();
        let barrier = Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let other = store(dir.path());
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        other.retain(MemoryKind::Fact, "one durable fact", &[], None)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let error = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .unwrap();
        assert!(error.to_string().contains("PI_MEMORY_DUPLICATE"), "{error}");
        assert_eq!(bank.recall("durable", None).unwrap().len(), 1);
        assert_eq!(bank.list(10).unwrap().len(), 1);
    }

    #[test]
    fn update_repairs_a_missing_legacy_index_without_creating_duplicate_content() {
        let dir = tempfile::tempdir().unwrap();
        let bank = store(dir.path());
        let first = bank
            .retain(MemoryKind::Fact, "first fact", &[], None)
            .unwrap();
        bank.retain(MemoryKind::Fact, "second fact", &[], None)
            .unwrap();
        bank.with_conn(|conn| {
            conn.execute_sync(
                "DELETE FROM memories_fts WHERE rowid = ?1",
                &[fsqlite::SqliteValue::Integer(first.id)],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        bank.edit(first.id, MemoryEditOp::Update, Some("repaired fact"))
            .unwrap();
        assert_eq!(bank.recall("repaired", None).unwrap()[0].id, first.id);
        assert!(
            bank.edit(first.id, MemoryEditOp::Update, Some("second fact"))
                .is_err()
        );
        assert_eq!(bank.recall("repaired", None).unwrap()[0].id, first.id);
        assert!(
            bank.edit(first.id, MemoryEditOp::Update, Some("   "))
                .is_err()
        );
        assert_eq!(audit_ops(&bank, first.id), ["retain", "update"]);
    }

    #[test]
    fn retain_tool_supersedes_and_screens_tags_before_storage() {
        let dir = tempfile::tempdir().unwrap();
        let bank = Arc::new(store(dir.path()));
        let old = bank
            .retain(MemoryKind::Fact, "old endpoint", &[], None)
            .unwrap();
        let tool = RetainTool::new(Arc::clone(&bank));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let output = runtime
            .block_on(tool.execute(
                "call-a",
                serde_json::json!({
                    "content": "new endpoint", "supersedes": old.id,
                    "tags": ["sk-abcdefghijklmnopqrstuvwxyz"]
                }),
                None,
            ))
            .unwrap();
        assert!(!output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["supersedes"], old.id);
        assert_eq!(details["tags"][0], "[REDACTED_OPENAI_KEY]");
        assert!(
            !serde_json::to_string(&bank.list(10).unwrap())
                .unwrap()
                .contains("sk-abcdef")
        );
    }

    #[test]
    fn oversized_latest_fact_does_not_hide_smaller_startup_memories() {
        let dir = tempfile::tempdir().unwrap();
        let bank = store(dir.path());
        bank.retain(MemoryKind::Fact, "small useful fact", &[], None)
            .unwrap();
        bank.retain(MemoryKind::Fact, &"large".repeat(1000), &[], None)
            .unwrap();
        let model = bank.mental_model().unwrap();
        assert!(model.contains("small useful fact"));
        assert!(model.len() <= crate::memory::MENTAL_MODEL_BUDGET);
    }

    #[test]
    fn writer_panic_rolls_back_primary_index_and_audit_before_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let bank = store(dir.path());
        bank.with_conn(|conn| {
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _panic_guard = crate::crash::SuppressPanicHook::new();
                let _: crate::error::Result<()> = super::run(conn, |conn| {
                    conn.execute_raw(
                        "INSERT INTO memories (kind, content, tags, created_at_ms, updated_at_ms, status) \
                         VALUES ('fact', 'uncommitted evidence', '[]', 1, 1, 'active')",
                    )
                    .unwrap();
                    conn.execute_raw(
                        "INSERT INTO memories_fts (rowid, content) VALUES (1, 'uncommitted evidence'); \
                         INSERT INTO memory_audit (memory_id, op, at_ms) VALUES (1, 'retain', 1)",
                    )
                    .unwrap();
                    std::panic::panic_any(731_u32);
                });
            }))
            .expect_err("writer panic is not swallowed");
            assert_eq!(panic.downcast_ref::<u32>(), Some(&731));
            for table in ["memories", "memories_fts", "memory_audit"] {
                let rows = conn.query_sync(&format!("SELECT COUNT(*) FROM {table}"), &[]).unwrap();
                assert_eq!(crate::memory::row_i64(&rows[0], 0)?, 0, "{table}");
            }
            super::run(conn, |_| Ok(()))?;
            Ok(())
        })
        .unwrap();
        let reopened = store(dir.path());
        assert!(reopened.list(10).unwrap().is_empty());
        assert!(reopened.recall("uncommitted", None).unwrap().is_empty());
        let retained = reopened
            .retain(MemoryKind::Fact, "durable evidence", &[], None)
            .unwrap();
        assert_eq!(audit_ops(&reopened, retained.id), ["retain"]);
        assert_eq!(reopened.recall("durable", None).unwrap()[0].id, retained.id);
    }
}
