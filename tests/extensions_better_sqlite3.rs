//! Imported better-sqlite3 uses the real, capability-bound `PiJS` SQLite engine.

use pi::extensions::{ExtensionPolicy, ExtensionPolicyMode, safe_canonicalize};
use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::DeterministicClock;
use serde_json::{Value, json};
use std::path::Path;

// Keep independent fixtures below the native runtime's worker admission bound.
static SQLITE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[allow(clippy::future_not_send)]
async fn runtime(
    root: &Path,
    owner: &str,
    policy: Option<ExtensionPolicy>,
) -> PiJsRuntime<DeterministicClock> {
    PiJsRuntime::with_clock_and_config_with_policy_for_extension(
        DeterministicClock::new(0),
        PiJsRuntimeConfig {
            cwd: root.to_string_lossy().into_owned(),
            ..Default::default()
        },
        policy,
        owner.to_string(),
    )
    .await
    .expect("runtime")
}

#[allow(clippy::future_not_send)]
async fn evaluate(runtime: &PiJsRuntime<DeterministicClock>, source: &str) -> Value {
    runtime
        .eval(&format!(
            r"
            globalThis.betterResult = null;
            globalThis.betterError = null;
            globalThis.betterDone = false;
            Promise.all([import('better-sqlite3'), import('node:module'), import('node:buffer')])
              .then(([better, module, buffer]) => {{
                const Database = better.default;
                const Buffer = buffer.Buffer;
                {source}
              }})
              .catch(error => {{ globalThis.betterError = String(error.stack || error); }})
              .finally(() => {{ globalThis.betterDone = true; }});
            "
        ))
        .await
        .expect("evaluate");
    runtime.drain_microtasks().await.expect("drain");
    assert_eq!(
        runtime.read_global_json("betterDone").await.unwrap(),
        json!(true),
        "the real module initialization must finish"
    );
    assert_eq!(
        runtime.read_global_json("betterError").await.unwrap(),
        Value::Null
    );
    runtime.read_global_json("betterResult").await.unwrap()
}

#[test]
#[allow(clippy::too_many_lines)]
fn better_sqlite3_imports_execute_statements_and_preserve_binding_and_close_semantics() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "better-api", None).await;
        let result = evaluate(&runtime, r"
            const required = module.createRequire('file:///extension/index.mjs')('better-sqlite3');
            if (required !== Database || better.Database !== Database) {
              throw new Error('ESM and createRequire must share the real constructor');
            }
            const db = required(':memory:', { timeout: 0 });
            const chained = db.exec('CREATE TABLE notes(id INTEGER PRIMARY KEY, name TEXT, payload BLOB)') === db;
            const insert = db.prepare('INSERT INTO notes VALUES (?, ?, ?)');
            const inserted = insert.run(1, 'first', Buffer.from([0, 255, 128]));
            insert.run([2], ['second', Buffer.from([7])]);
            db.prepare('INSERT INTO notes VALUES ($id, :name, @payload)').run({
              id: 3, name: 'third', payload: Buffer.from([9]),
            });
            const query = db.prepare('SELECT id, name, payload FROM notes WHERE id = ?');
            const bound = db.prepare('SELECT name FROM notes WHERE id = ?').bind(2);
            const rejected = [];
            for (const action of [
              () => bound.get(1),
              () => bound.bind(3),
              () => query.get(),
              () => insert.get(4, 'must not insert', Buffer.from([])),
              () => db.prepare('INSERT INTO notes VALUES ($id, :name, @payload)').run({id: 4}),
            ]) {
              try { action(); rejected.push(false); } catch { rejected.push(true); }
            }
            const first = query.get(1);
            const rows = db.prepare('SELECT id, name FROM notes ORDER BY id');
            const snapshot = {
              sameConstructor: db instanceof Database && db.constructor === Database,
              metadata: [db.name, db.memory, db.readonly, db.open, db.inTransaction],
              chained, inserted, rejected,
              first: {id: first.id, name: first.name, payload: Array.from(first.payload)},
              blobIsBuffer: Buffer.isBuffer(first.payload),
              bound: bound.get(), missing: query.get(99) === undefined,
              rows: rows.all(), iterated: [...rows.iterate()],
              statement: [query.database === db, query.reader, insert.reader, query.busy],
              source: query.source,
              busyTimeout: db.pragma('busy_timeout', {simple: true}),
            };
            snapshot.closed = db.close() === db && !db.open && !db.inTransaction;
            try { query.get(1); snapshot.closedStatementDenied = false; }
            catch { snapshot.closedStatementDenied = true; }
            try { db.prepare('SELECT 1'); snapshot.closedPrepareDenied = false; }
            catch { snapshot.closedPrepareDenied = true; }
            snapshot.closeIsIdempotent = db.close() === db;
            globalThis.betterResult = snapshot;
        ").await;
        assert_eq!(
            result,
            json!({
                "sameConstructor": true,
                "metadata": [":memory:", true, false, true, false],
                "chained": true,
                "inserted": {"changes": 1, "lastInsertRowid": 1},
                "rejected": [true, true, true, true, true],
                "first": {"id": 1, "name": "first", "payload": [0, 255, 128]},
                "blobIsBuffer": true, "bound": {"name": "second"}, "missing": true,
                "rows": [
                    {"id": 1, "name": "first"},
                    {"id": 2, "name": "second"},
                    {"id": 3, "name": "third"}
                ],
                "iterated": [
                    {"id": 1, "name": "first"},
                    {"id": 2, "name": "second"},
                    {"id": 3, "name": "third"}
                ],
                "statement": [true, true, false, false],
                "source": "SELECT id, name, payload FROM notes WHERE id = ?",
                "busyTimeout": 0, "closed": true,
                "closedStatementDenied": true, "closedPrepareDenied": true,
                "closeIsIdempotent": true,
            })
        );
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn better_sqlite3_transactions_rollback_and_integer_modes_use_real_values() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "better-transactions", None).await;
        let result = evaluate(&runtime, r"
            const db = new Database(':memory:');
            db.exec('CREATE TABLE ledger(id INTEGER PRIMARY KEY, value TEXT)');
            const insert = db.prepare('INSERT INTO ledger VALUES (?, ?)');
            let observedTransaction = false;
            const save = db.transaction(function (value) {
              observedTransaction = db.inTransaction;
              insert.run(1, value);
              try { db.transaction(() => {
                insert.run(2, 'inner rollback');
                throw new Error('inner rollback');
              })(); } catch (error) { if (error.message !== 'inner rollback') throw error; }
              insert.run(3, 'committed peer');
              return this.label;
            });
            const returned = save.immediate.call({label: 'transaction return'}, 'committed');
            let outerRollback = false;
            try { db.transaction(() => {
              insert.run(4, 'outer rollback');
              throw new Error('outer rollback');
            }).exclusive(); } catch (error) { outerRollback = error.message === 'outer rollback'; }
            let asyncRejected = false;
            try { db.transaction(() => {
              insert.run(5, 'async rollback');
              return Promise.resolve('not a synchronous transaction');
            })(); } catch (error) { asyncRejected = error instanceof TypeError; }
            const big = 9007199254740993n;
            const ordinary = db.prepare('SELECT ? AS value');
            const safe = db.prepare('SELECT ? AS value').safeIntegers();
            db.defaultSafeIntegers();
            const inherited = db.prepare('SELECT ? AS value');
            db.defaultSafeIntegers(false);
            const nativeGuards = [];
            for (const action of [
              () => db.exec('ATTACH DATABASE ' + String.fromCharCode(39) + ':memory:' + String.fromCharCode(39) + ' AS forbidden'),
              () => db.loadExtension('/must-not-load/native-addon'),
              () => new Database(':memory:', {nativeBinding: '/must-not-load/native-addon'}),
              () => new Database(':memory:', {timeout: 30001}),
            ]) {
              try { action(); nativeGuards.push(false); } catch { nativeGuards.push(true); }
            }
            db.pragma('busy_timeout = 17');
            globalThis.betterResult = {
              returned, observedTransaction, outerRollback, asyncRejected,
              transactionOwner: save.database === db && save.immediate.database === db,
              rows: db.prepare('SELECT id, value FROM ledger ORDER BY id').all(),
              inTransaction: db.inTransaction,
              ordinaryType: typeof ordinary.get(big).value,
              safe: String(safe.get(big).value), safeType: typeof safe.get(big).value,
              inheritedType: typeof inherited.get(7).value,
              disabledType: typeof safe.safeIntegers(false).get(7).value,
              namedBound: db.prepare('SELECT $value AS value').bind({value: 'bound'}).get(),
              nativeGuards, pragma: db.pragma('busy_timeout', {simple: true}),
            };
            db.close();
        ").await;
        assert_eq!(
            result,
            json!({
                "returned": "transaction return", "observedTransaction": true,
                "outerRollback": true, "asyncRejected": true, "transactionOwner": true,
                "rows": [
                    {"id": 1, "value": "committed"},
                    {"id": 3, "value": "committed peer"}
                ],
                "inTransaction": false, "ordinaryType": "number",
                "safe": "9007199254740993", "safeType": "bigint",
                "inheritedType": "bigint", "disabledType": "number",
                "namedBound": {"value": "bound"}, "nativeGuards": [true, true, true, true],
                "pragma": 17,
            })
        );
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn better_sqlite3_persists_across_realms_without_widening_storage_or_write_authority() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("owner");
        std::fs::create_dir(&storage).expect("storage directory");
        let database = storage.join("state.db");
        let missing = storage.join("missing.db");
        let outside = root.join("outside.db");
        let path = serde_json::to_string(&database.to_string_lossy()).unwrap();
        let missing_path = serde_json::to_string(&missing.to_string_lossy()).unwrap();
        let outside_path = serde_json::to_string(&outside.to_string_lossy()).unwrap();
        let first = runtime(&root, "owner", None).await;
        first
            .add_persistent_storage_root(storage.clone(), "owner")
            .expect("owner grant");
        let saved = evaluate(&first, &format!(r"
            const db = new Database({path});
            db.exec('CREATE TABLE state(value TEXT)');
            const changed = db.prepare('INSERT INTO state VALUES (?)').bind('durable λ').run().changes;
            db.close();
            const denied = [{outside_path}, {missing_path}].map((path, index) => {{
              try {{ new Database(path, {{fileMustExist: index === 1}}); return false; }}
              catch {{ return true; }}
            }});
            globalThis.betterResult = {{changed, denied}};
        ")).await;
        assert_eq!(saved, json!({"changed": 1, "denied": [true, true]}));
        drop(first);
        assert!(database.metadata().expect("native database").len() > 0);
        assert!(
            !outside.exists(),
            "a storage grant must not cover its parent"
        );
        assert!(
            !missing.exists(),
            "fileMustExist must not create a database"
        );

        let peer = runtime(&root, "peer", None).await;
        peer.add_persistent_storage_root(storage.clone(), "owner")
            .expect("foreign owner grant");
        assert_eq!(
            evaluate(
                &peer,
                &format!(
                    r"
                try {{ new Database({path}); globalThis.betterResult = false; }}
                catch {{ globalThis.betterResult = true; }}
            "
                )
            )
            .await,
            json!(true),
            "the adapter must retain the native extension owner check"
        );
        drop(peer);

        let mut policy = ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            ..Default::default()
        };
        policy.deny_caps.push("write".into());
        let restricted = runtime(&root, "owner", Some(policy)).await;
        restricted
            .add_persistent_storage_root(storage.clone(), "owner")
            .expect("restricted grant");
        assert_eq!(
            evaluate(
                &restricted,
                &format!(
                    r"
                try {{ new Database({path}); globalThis.betterResult = false; }}
                catch {{ globalThis.betterResult = true; }}
            "
                )
            )
            .await,
            json!(true),
            "a persistent root must not bypass the write capability"
        );
        drop(restricted);

        let second = runtime(&root, "owner", None).await;
        second
            .add_persistent_storage_root(storage, "owner")
            .expect("reopened owner grant");
        let reopened = evaluate(
            &second,
            &format!(
                r"
            const db = new Database({path}, {{readonly: true, fileMustExist: true}});
            let writeDenied = false;
            try {{ db.prepare('INSERT INTO state VALUES (?)').run('must not write'); }}
            catch {{ writeDenied = true; }}
            globalThis.betterResult = {{
              rows: db.prepare('SELECT value FROM state').all(),
              writeDenied, readonly: db.readonly, memory: db.memory,
            }};
            db.close();
        "
            ),
        )
        .await;
        assert_eq!(
            reopened,
            json!({
                "rows": [{"value": "durable λ"}],
                "writeDenied": true, "readonly": true, "memory": false,
            })
        );
    });
}
