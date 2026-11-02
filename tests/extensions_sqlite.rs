//! Real SQLite initialization through the `PiJS` ESM and createRequire loaders.
//! The fixtures exercise the API shape used by context-mode's Bun adapter,
//! including durable session data, migrations, transactions and RETURNING.

use pi::extensions::{ExtensionPolicy, ExtensionPolicyMode, safe_canonicalize};
use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::DeterministicClock;
use serde_json::{Value, json};
use std::io::{BufRead as _, Write as _};
use std::path::Path;

// A process intentionally has at most eight SQLite workers. Keep these
// independent runtime fixtures from competing for that production budget.
static SQLITE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A separate stock SQLite process is essential here: POSIX file locks are
/// process-wide, so loading a second SQLite implementation in this test process
/// would not exercise the cross-process WAL locking contract.
struct StockSqlitePeer {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::sync::mpsc::Receiver<String>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl StockSqlitePeer {
    fn start(path: &Path) -> Option<Self> {
        let executable = std::env::var_os("PI_TEST_NODE").unwrap_or_else(|| "node".into());
        let script = r"
            let DatabaseSync;
            try { ({ DatabaseSync } = await import('node:sqlite')); }
            catch (error) {
              if (error.code !== 'ERR_UNKNOWN_BUILTIN_MODULE') throw error;
              console.log(JSON.stringify({unavailable:String(error)}));
              process.exit(0);
            }
            const { createInterface } = await import('node:readline');
            const database = new DatabaseSync(process.argv[1]);
            database.exec('PRAGMA journal_mode=WAL; PRAGMA busy_timeout=0');
            console.log(JSON.stringify({ready:true}));
            for await (const line of createInterface({input:process.stdin, terminal:false})) {
              try {
                const { sql, mode = 'exec', bindings = [] } = JSON.parse(line);
                if (mode === 'close') {
                  database.close();
                  console.log(JSON.stringify({ok:null}));
                  break;
                }
                const result = mode === 'exec' ? database.exec(sql)
                  : database.prepare(sql)[mode](...bindings);
                console.log(JSON.stringify({ok:result ?? null}));
              } catch (error) {
                console.log(JSON.stringify({error:{
                  message:error.message, code:error.code, errno:error.errcode,
                }}));
              }
            }
            // stdin EOF is also a normal close path for this trusted fixture.
            try { database.close(); } catch {}
        ";
        let mut child = match std::process::Command::new(executable)
            .args(["--no-warnings", "--input-type=module", "--eval", script])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "SKIPPED SQLite interoperability: Node.js is unavailable (set PI_TEST_NODE)"
                );
                return None;
            }
            Err(error) => panic!("spawn stock SQLite peer: {error}"),
        };
        let input = child.stdin.take().expect("stock SQLite stdin");
        let stdout = child.stdout.take().expect("stock SQLite stdout");
        let (sender, output) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let peer = Self {
            child,
            input,
            output,
            reader: Some(reader),
        };
        let ready = peer.read();
        if let Some(reason) = ready.get("unavailable") {
            eprintln!(
                "SKIPPED SQLite interoperability: Node's native node:sqlite is unavailable: {reason}"
            );
            return None;
        }
        assert_eq!(ready, json!({"ready":true}), "native peer startup");
        Some(peer)
    }

    fn read(&self) -> Value {
        let line = self
            .output
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("stock SQLite peer must answer within 10 seconds");
        serde_json::from_str(&line).expect("stock SQLite protocol JSON")
    }

    fn request(&mut self, sql: &str, mode: &str, bindings: Value) -> Value {
        writeln!(
            self.input,
            "{}",
            json!({"sql":sql, "mode":mode, "bindings":bindings})
        )
        .expect("write stock SQLite request");
        self.input.flush().expect("flush stock SQLite request");
        self.read()
    }

    fn execute(&mut self, sql: &str) {
        assert_eq!(self.request(sql, "exec", json!([])), json!({"ok":null}));
    }
}

impl Drop for StockSqlitePeer {
    fn drop(&mut self) {
        // Also reap on failed assertions; the child never launches descendants.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

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
        globalThis.sqliteResult = null;
        globalThis.sqliteError = null;
        Promise.all([import('bun:sqlite'), import('node:module'), import('node:fs')])
          .then(([sqlite, module, fs]) => {{ {source} }})
          .catch(error => {{ globalThis.sqliteError = String(error.stack || error); }});
    "
        ))
        .await
        .expect("evaluate");
    runtime.drain_microtasks().await.expect("drain");
    assert_eq!(
        runtime
            .read_global_json("sqliteError")
            .await
            .expect("error value"),
        Value::Null
    );
    runtime
        .read_global_json("sqliteResult")
        .await
        .expect("result")
}

#[test]
#[allow(clippy::too_many_lines)]
fn sqlite_imports_initialize_a_persistent_session_database_and_reopen_it() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join(".pi/context-mode");
        let database = storage.join("sessions/project.db");
        let path = serde_json::to_string(&database.to_string_lossy()).expect("path");
        let dir = serde_json::to_string(&database.parent().unwrap().to_string_lossy())
            .expect("directory");
        let first = runtime(&root, "context-mode", None).await;
        first
            .add_persistent_storage_root(storage.clone(), "context-mode")
            .expect("storage grant");
        let result = evaluate(&first, &format!(r"
            const {{ Database }} = module.createRequire('file:///extension/index.mjs')('bun:sqlite');
            if (Database !== sqlite.Database) throw new Error('ESM and require must share the constructor');
            fs.mkdirSync({dir}, {{ recursive: true }});
            const db = new Database({path}, {{ create: true }});
            db.prepare('PRAGMA journal_mode = WAL').all();
            db.prepare('PRAGMA busy_timeout = 30000').all();
            db.prepare('PRAGMA synchronous = NORMAL').all();
            // context-mode's adapter runs schema DDL through prepare().run().
            db.prepare(`CREATE TABLE IF NOT EXISTS session_meta (
              session_id TEXT PRIMARY KEY, project_dir TEXT NOT NULL,
              event_count INTEGER NOT NULL DEFAULT 0
            )`).run();
            db.prepare(`CREATE TABLE IF NOT EXISTS session_events (
              id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
              data TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (datetime('now'))
            )`).run();
            const columns = db.prepare('PRAGMA table_xinfo(session_meta)').all();
            if (!columns.some(column => column.name === 'usage_cursor')) {{
              db.prepare('ALTER TABLE session_meta ADD COLUMN usage_cursor TEXT').run();
            }}
            const save = db.transaction((session, project, data) => {{
              db.prepare('INSERT OR IGNORE INTO session_meta(session_id, project_dir) VALUES (?, ?)').run(session, project);
              db.prepare('INSERT INTO session_events(session_id, data) VALUES (?, ?)').run(session, data);
              db.prepare('UPDATE session_meta SET event_count = event_count + 1 WHERE session_id = ?').run(session);
            }});
            save('session-1', 'C:\\work\\project', 'durable; unicode λ');
            const statement = db.prepare('SELECT data FROM session_events WHERE session_id = ?');
            globalThis.sqliteResult = {{
              first: statement.get('session-1'),
              rows: [...statement.iterate('session-1')],
              count: db.query('SELECT event_count FROM session_meta WHERE session_id = ?').get('session-1'),
              inTransaction: db.inTransaction,
            }};
            db.close();
        ")).await;
        assert_eq!(
            result,
            json!({
                "first": {"data": "durable; unicode λ"},
                "rows": [{"data": "durable; unicode λ"}],
                "count": {"event_count": 1}, "inTransaction": false,
            })
        );
        drop(first);
        assert!(database.metadata().expect("real database file").len() > 0);
        let marker = storage.join("sessions/project.db.fsqlite-migration-state");
        let marker_before = std::fs::read(&marker).expect("native marker at database birth");
        let marker_value: Value =
            serde_json::from_slice(&marker_before).expect("native migration marker JSON");
        assert!(marker_value["last_upgrade_version"].as_u64().unwrap() >= 1);
        let second = runtime(&root, "context-mode", None).await;
        second
            .add_persistent_storage_root(storage, "context-mode")
            .expect("storage grant");
        let persisted = evaluate(&second, &format!(r#"
            const db = new sqlite.Database({path}, {{ readonly: true, create: true }});
            let writeDenied = false;
            try {{ db.run("INSERT INTO session_events(session_id,data) VALUES ('x','must fail')"); }}
            catch {{ writeDenied = true; }}
            globalThis.sqliteResult = {{
              row: db.query('SELECT data FROM session_events').get(), writeDenied,
              migration: db.query('PRAGMA table_xinfo(session_meta)').all().some(column => column.name === 'usage_cursor'),
            }};
            db.close();
        "#)).await;
        assert_eq!(
            persisted,
            json!({"row": {"data": "durable; unicode λ"}, "writeDenied": true, "migration": true})
        );
        assert_eq!(
            std::fs::read(marker).expect("marker after cold read-only reopen"),
            marker_before,
            "normal native initialization must retain the valid migration marker"
        );
    });
}

#[test]
fn sqlite_busy_error_reaches_context_mode_retry_and_preserves_persistent_state() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let path = serde_json::to_string(&storage.join("retry.db")).unwrap();
        let runtime = runtime(&root, "retry", None).await;
        runtime
            .add_persistent_storage_root(storage, "retry")
            .expect("storage grant");
        let result = evaluate(&runtime, &format!(r"
            const locker = new sqlite.Database({path});
            locker.exec('PRAGMA journal_mode = WAL');
            locker.exec('CREATE TABLE events (id INTEGER PRIMARY KEY, data TEXT)');
            const db = new sqlite.Database({path}, {{ create: false }});
            db.exec('PRAGMA busy_timeout = 150');
            const configuredWait = db.query('PRAGMA busy_timeout').get().timeout;
            const insert = db.prepare('INSERT INTO events VALUES (?, ?)');
            const save = db.transaction(() => insert.run(1, 'persisted after retry'));
            locker.exec('BEGIN IMMEDIATE');
            locker.prepare('INSERT INTO events VALUES (?, ?)').run(99, 'uncommitted owner');
            const failures = [];
            let attempts = 0;
            // Match context-mode 1.0.169 db-base.withRetry's message test.
            // Release the genuine competing writer only after that test
            // recognizes the first failure, then retry the same transaction.
            for (;;) {{
              attempts++;
              try {{ save(); break; }}
              catch (error) {{
                if (!error.message.includes('SQLITE_BUSY') && !error.message.includes('database is locked')) throw error;
                if (attempts !== 1) throw error;
                failures.push({{code:error.code, errno:error.errno, primary:error.primaryCode,
                  message:error.message, rolledBack:!db.inTransaction}});
                locker.exec('ROLLBACK');
              }}
            }}
            const inTransaction = db.inTransaction;
            db.close();
            locker.close();
            const reopened = new sqlite.Database({path}, {{ readonly: true }});
            globalThis.sqliteResult = {{ attempts, failures, inTransaction, configuredWait,
              rows: reopened.query('SELECT id, data FROM events ORDER BY id').values() }};
            reopened.close();
        ")).await;
        assert_eq!(result["attempts"], 2, "{result}");
        assert_eq!(
            result["configuredWait"], 150,
            "native timeout setting must remain visible: {result}"
        );
        assert_eq!(result["inTransaction"], false, "{result}");
        assert_eq!(
            result["rows"],
            json!([[1, "persisted after retry"]]),
            "{result}"
        );
        let failure = &result["failures"][0];
        assert_eq!(failure["primary"], 5, "{result}");
        assert_eq!(failure["errno"].as_i64().unwrap() & 0xff, 5);
        assert!(failure["code"].as_str().unwrap().starts_with("SQLITE_BUSY"));
        assert!(failure["message"].as_str().unwrap().contains("SQLITE_BUSY"));
        assert_eq!(failure["rolledBack"], true, "{result}");
    });
}

#[test]
fn sqlite_default_transactions_serialize_writers_and_publish_autocommit_results() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let path = serde_json::to_string(&storage.join("single-writer.db")).unwrap();
        let runtime = runtime(&root, "single-writer", None).await;
        runtime
            .add_persistent_storage_root(storage, "single-writer")
            .expect("storage grant");
        let result = evaluate(&runtime, &format!(r"
            const writer = new sqlite.Database({path});
            writer.exec('PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 0');
            writer.exec('CREATE TABLE first_writer(id INTEGER PRIMARY KEY, data TEXT); CREATE TABLE second_writer(id INTEGER PRIMARY KEY, data TEXT)');
            const peer = new sqlite.Database({path}, {{create:false}});
            peer.exec('PRAGMA busy_timeout = 0');
            const observePeer = writer.prepare('SELECT id, data FROM second_writer ORDER BY id');
            const busy = [];
            for (const [index, begin, finish] of [
              [1, 'BEGIN', 'ROLLBACK'],
              [2, 'SAVEPOINT held', 'ROLLBACK TO held; RELEASE held'],
            ]) {{
              writer.exec(begin);
              writer.prepare('INSERT INTO first_writer VALUES (?, ?)').run(index, 'uncommitted');
              try {{ peer.prepare('INSERT INTO second_writer VALUES (?, ?)').run(index, 'must wait'); }}
              catch (error) {{ busy.push({{code:error.code, primaryCode:error.primaryCode}}); }}
              if (busy.length !== index) throw new Error('a second writer entered an owned SQLite write transaction');
              writer.exec(finish);
              if (writer.inTransaction) throw new Error('transaction did not finish');
              peer.prepare('INSERT INTO second_writer VALUES (?, ?)').run(index, 'committed');
            }}
            // Autocommit writes are visible through an already-prepared peer
            // statement, without a close/checkpoint to make the data appear.
            globalThis.sqliteResult = {{
              busy, published: observePeer.all(),
              rolledBack: writer.query('SELECT COUNT(*) AS n FROM first_writer').get().n,
            }};
            peer.close(); writer.close();
        ")).await;
        assert_eq!(
            result,
            json!({
                "busy": [
                    {"code":"SQLITE_BUSY", "primaryCode":5},
                    {"code":"SQLITE_BUSY", "primaryCode":5},
                ],
                "published": [
                    {"id":1, "data":"committed"},
                    {"id":2, "data":"committed"},
                ],
                "rolledBack":0,
            })
        );
    });
}

#[test]
fn sqlite_guest_sql_cannot_restore_engine_specific_concurrent_transactions() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "concurrency-admission", None).await;
        let result = evaluate(
            &runtime,
            r"
            const db = new sqlite.Database();
            db.exec('CREATE TABLE evidence(value)');
            const denied = [];
            for (const sql of [
              'BEGIN CONCURRENT',
              'BEGIN /* engine extension */ CONCURRENT TRANSACTION',
              'PRAGMA concurrent_mode = ON',
              'PRAGMA fsqlite.concurrent_mode = ON',
              'INSERT INTO evidence VALUES (1); BEGIN CONCURRENT',
            ]) {
              try { db.exec(sql); denied.push(false); }
              catch (error) { denied.push(error.code === 'SQLITE_ERROR'); }
            }
            let prepareDenied = false;
            try { db.prepare('BEGIN CONCURRENT'); }
            catch (error) { prepareDenied = error.code === 'SQLITE_ERROR'; }
            globalThis.sqliteResult = {
              denied, prepareDenied, inTransaction:db.inTransaction,
              changes:db.query('SELECT COUNT(*) AS n FROM evidence').get().n,
            };
            db.close();
        ",
        )
        .await;
        assert_eq!(
            result,
            json!({
                "denied":[true, true, true, true, true], "prepareDenied":true,
                "inTransaction":false, "changes":0,
            })
        );
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn sqlite_native_wal_interoperates_with_a_live_stock_node_connection() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    let temp = tempfile::tempdir().expect("tempdir");
    let root = safe_canonicalize(temp.path());
    let storage = root.join("storage");
    std::fs::create_dir(&storage).expect("storage");
    let database = storage.join("mixed-engines.db");
    let Some(mut stock) = StockSqlitePeer::start(&database) else {
        return;
    };
    futures::executor::block_on(async {
        let runtime = runtime(&root, "mixed-engines", None).await;
        runtime
            .add_persistent_storage_root(storage, "mixed-engines")
            .expect("storage grant");
        let path = serde_json::to_string(&database).unwrap();
        let claim = "UPDATE session_resume SET consumed = 1 WHERE id = (SELECT id FROM session_resume WHERE consumed = 0 AND session_id != ? ORDER BY created_at DESC, id DESC LIMIT 1) RETURNING session_id, snapshot";
        let claim_literal = serde_json::to_string(claim).unwrap();
        evaluate(&runtime, &format!(r#"
            globalThis.sharedDb = new sqlite.Database({path}, {{create:false}});
            sharedDb.exec('PRAGMA journal_mode=WAL; PRAGMA busy_timeout=0');
            sharedDb.exec(`CREATE TABLE session_resume (
              id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL UNIQUE,
              snapshot TEXT NOT NULL, event_count INTEGER NOT NULL,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              consumed INTEGER NOT NULL DEFAULT 0
            )`);
            globalThis.saveSnapshot = sharedDb.prepare("INSERT INTO session_resume(session_id,snapshot,event_count,created_at) VALUES(?,?,?,'2026-10-31 00:00:00')");
            globalThis.claimSnapshot = sharedDb.prepare({claim_literal});
            saveSnapshot.run('session-1', 'first λ snapshot', 1);
        "#)).await;

        stock.execute("BEGIN");
        assert_eq!(
            stock.request("SELECT COUNT(*) AS n FROM session_resume", "get", json!([])),
            json!({"ok":{"n":1}})
        );
        evaluate(
            &runtime,
            "saveSnapshot.run('session-2', 'second snapshot', 2);",
        )
        .await;
        assert_eq!(
            stock.request("SELECT COUNT(*) AS n FROM session_resume", "get", json!([])),
            json!({"ok":{"n":1}}),
            "the existing stock read transaction keeps its snapshot"
        );
        stock.execute("ROLLBACK");
        assert_eq!(
            stock.request("SELECT COUNT(*) AS n FROM session_resume", "get", json!([])),
            json!({"ok":{"n":2}}),
            "the same stock connection must see an acknowledged PiJS WAL commit"
        );

        stock.execute("BEGIN IMMEDIATE; UPDATE session_resume SET snapshot='uncommitted stock value' WHERE session_id='session-1'");
        let blocked = evaluate(&runtime, r"
            try { saveSnapshot.run('blocked', 'must not commit', 0); }
            catch (error) { globalThis.sqliteResult = {code:error.code, primaryCode:error.primaryCode}; }
        ").await;
        assert_eq!(blocked, json!({"code":"SQLITE_BUSY", "primaryCode":5}));
        stock.execute("ROLLBACK");
        evaluate(
            &runtime,
            "saveSnapshot.run('session-3', 'third snapshot', 3);",
        )
        .await;

        evaluate(
            &runtime,
            "sharedDb.exec('BEGIN'); saveSnapshot.run('session-4', 'fourth snapshot', 4);",
        )
        .await;
        let blocked = stock.request(
            "INSERT INTO session_resume(session_id,snapshot,event_count) VALUES('stock-blocked','must wait',0)",
            "run", json!([]),
        );
        assert_eq!(
            blocked["error"]["errno"], 5,
            "stock writer must observe the native write lock: {blocked}"
        );
        assert_eq!(
            stock.request("SELECT COUNT(*) AS n FROM session_resume", "get", json!([])),
            json!({"ok":{"n":3}}),
            "stock reads must not observe the native uncommitted row"
        );
        evaluate(&runtime, "sharedDb.exec('COMMIT');").await;

        // Both engines execute context-mode's atomic resume claim against the
        // same live file. Each must observe the other engine's consumed flag.
        assert_eq!(
            stock.request(claim, "get", json!(["session-1"])),
            json!({"ok":{"session_id":"session-4", "snapshot":"fourth snapshot"}})
        );
        let claimed = evaluate(
            &runtime,
            "globalThis.sqliteResult = claimSnapshot.get('session-1');",
        )
        .await;
        assert_eq!(
            claimed,
            json!({"session_id":"session-3", "snapshot":"third snapshot"})
        );
        assert_eq!(
            stock.request(claim, "get", json!(["session-1"])),
            json!({"ok":{"session_id":"session-2", "snapshot":"second snapshot"}})
        );
        let remaining = evaluate(
            &runtime,
            "globalThis.sqliteResult = claimSnapshot.get('session-1'); sharedDb.close();",
        )
        .await;
        assert_eq!(
            remaining,
            Value::Null,
            "a consumed snapshot must not be claimed twice"
        );
        assert_eq!(
            stock.request("PRAGMA integrity_check", "get", json!([])),
            json!({"ok":{"integrity_check":"ok"}})
        );
        assert_eq!(stock.request("", "close", json!([])), json!({"ok":null}));

        let reopened = evaluate(&runtime, &format!(r"
            const db = new sqlite.Database({path}, {{readonly:true, create:false}});
            globalThis.sqliteResult = db.prepare('SELECT session_id, snapshot, consumed FROM session_resume ORDER BY id').all();
            db.close();
        ")).await;
        assert_eq!(
            reopened,
            json!([
                {"session_id":"session-1", "snapshot":"first λ snapshot", "consumed":0},
                {"session_id":"session-2", "snapshot":"second snapshot", "consumed":1},
                {"session_id":"session-3", "snapshot":"third snapshot", "consumed":1},
                {"session_id":"session-4", "snapshot":"fourth snapshot", "consumed":1},
            ])
        );
    });
}

#[test]
fn sqlite_engine_errors_remain_distinct_from_admission_and_nested_rollback_errors() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let invalid = storage.join("invalid.db");
        std::fs::write(&invalid, [0x61_u8; 4096]).expect("invalid database fixture");
        let invalid_path = serde_json::to_string(&invalid).unwrap();
        let missing_path = serde_json::to_string(&storage.join("missing.db")).unwrap();
        let runtime = runtime(&root, "errors", None).await;
        runtime
            .add_persistent_storage_root(storage, "errors")
            .expect("storage grant");
        let result = evaluate(
            &runtime,
            &format!(
                r#"
            const db = new sqlite.Database(':memory:');
            const errors = {{}};
            function capture(name, operation) {{
              try {{ operation(); throw new Error('expected SQLite rejection'); }}
              catch (error) {{
                errors[name] = {{code:error.code, errno:error.errno ?? null,
                  primary:error.primaryCode ?? null, message:error.message}};
              }}
            }}
            // A table name that looks like lock prose must not alter its
            // actual native SQLITE_ERROR classification.
            capture('missingTable', () => db.prepare('SELECT * FROM "database is locked"'));
            capture('admission', () => db.exec("ATTACH ':memory:' AS other"));
            capture('missingFile', () => new sqlite.Database({missing_path}, {{create:false}}));
            capture('invalidFile', () => new sqlite.Database({invalid_path}, {{create:false}}));
            const readonly = new sqlite.Database(':memory:', {{readonly:true}});
            capture('readonly', () => readonly.exec('CREATE TABLE denied(value)'));
            readonly.close();
            db.exec('CREATE TABLE entries(id INTEGER PRIMARY KEY)');
            db.run('INSERT INTO entries VALUES (1)');
            capture('nestedRollback', () => db.transaction(() => {{
              db.run('INSERT INTO entries VALUES (2)');
              db.transaction(() => db.run('INSERT OR ROLLBACK INTO entries VALUES (1)'))();
            }})());
            const ended = !db.inTransaction;
            db.transaction(() => db.run('INSERT INTO entries VALUES (3)'))();
            globalThis.sqliteResult = {{ errors, ended,
              rows:db.query('SELECT id FROM entries ORDER BY id').values() }};
            db.close();
        "#
            ),
        )
        .await;
        assert_eq!(result["ended"], true, "{result}");
        assert_eq!(result["rows"], json!([[1], [3]]), "{result}");
        for (name, code, primary) in [
            ("missingTable", "SQLITE_ERROR", 1),
            ("missingFile", "SQLITE_CANTOPEN", 14),
            ("readonly", "SQLITE_READONLY", 8),
            ("nestedRollback", "SQLITE_CONSTRAINT", 19),
        ] {
            let error = &result["errors"][name];
            assert_eq!(error["code"], code, "{name}: {result}");
            assert_eq!(error["primary"], primary, "{name}: {result}");
            assert_eq!(error["errno"].as_i64().unwrap() & 0xff, primary);
            assert!(error["message"].as_str().unwrap().starts_with(code));
        }
        let admission = &result["errors"]["admission"];
        assert_eq!(admission["code"], "SQLITE_ERROR", "{result}");
        assert_eq!(admission["errno"], Value::Null, "{result}");
        assert_eq!(admission["primary"], Value::Null, "{result}");
        let invalid = &result["errors"]["invalidFile"];
        assert!(
            matches!(
                invalid["code"].as_str(),
                Some("SQLITE_CORRUPT" | "SQLITE_NOTADB")
            ),
            "native corruption/open failure must keep its actual code: {result}"
        );
        assert!(
            invalid["message"]
                .as_str()
                .unwrap()
                .starts_with(invalid["code"].as_str().unwrap())
        );
    });
}

#[test]
fn sqlite_transactions_returning_bindings_and_binary_values_are_real() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "sqlite-api", None).await;
        let result = evaluate(&runtime, r#"
            const db = new sqlite.Database(':memory:', { strict: true });
            db.exec('CREATE TABLE entries(id INTEGER PRIMARY KEY, name TEXT, payload BLOB); CREATE TABLE resumed(id INTEGER PRIMARY KEY, consumed INTEGER)');
            const pendingSchema = db.prepare('CREATE TABLE deferred(value TEXT)');
            const beforeStep = db.query("SELECT count(*) AS count FROM sqlite_master WHERE name = 'deferred'").get().count;
            pendingSchema.run();
            let malformedRejected = false;
            try { db.prepare('CREATE TABLE malformed('); } catch { malformedRejected = true; }
            const insert = db.prepare('INSERT INTO entries VALUES ($id, $name, $blob)');
            insert.run({id: 1, name: 'first', blob: new Uint8Array([0, 255, 128])});
            const outer = db.transaction(() => {
              insert.run({id: 2, name: 'second', blob: new Uint8Array([5])});
              try { db.transaction(() => {
                insert.run({id: 3, name: 'rolled back', blob: new Uint8Array([])});
                throw new Error('rollback inner');
              })(); } catch (error) { if (error.message !== 'rollback inner') throw error; }
            });
            outer.immediate();
            try { db.transaction(() => {
              insert.run({id: 4, name: 'rolled back outer', blob: new Uint8Array([])});
              throw new Error('rollback outer');
            })(); } catch (error) { if (error.message !== 'rollback outer') throw error; }
            db.run('INSERT INTO resumed VALUES(1,0), (2,0)');
            const claimed = db.prepare('UPDATE resumed SET consumed = 1 WHERE id = (SELECT id FROM resumed WHERE consumed = 0 ORDER BY id LIMIT 1) RETURNING id').get();
            let missingRejected = false;
            try { insert.run({id: 5}); } catch { missingRejected = true; }
            const first = db.query('SELECT * FROM entries ORDER BY id').get();
            globalThis.sqliteResult = {
              ids: db.query('SELECT id FROM entries ORDER BY id').values(),
              first: {id: first.id, name: first.name, payload: Array.from(first.payload)},
              claimed, consumed: db.query('SELECT SUM(consumed) AS count FROM resumed').get().count,
              missingRejected, empty: db.query('SELECT id FROM entries WHERE id = 99').get(),
              aliases: db.prepare('SELECT :same AS first, ?1 AS second').get({same: 7}),
              beforeStep, malformedRejected,
            };
            db.close();
        "#).await;
        assert_eq!(
            result,
            json!({
                "ids": [[1], [2]], "first": {"id": 1, "name": "first", "payload": [0,255,128]},
                "claimed": {"id": 1}, "consumed": 1, "missingRejected": true, "empty": null,
            "aliases": {"first": 7, "second": 7}, "beforeStep": 0, "malformedRejected": true,
            })
        );
    });
}

#[test]
fn sqlite_dml_row_methods_execute_returning_and_commit_once() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "returning", None).await;
        let result = evaluate(&runtime, r"
            const db = new sqlite.Database(':memory:');
            db.prepare('CREATE TABLE claims(id INTEGER PRIMARY KEY, consumed INTEGER DEFAULT 0)').run();
            const inserted = db.prepare('INSERT INTO claims(id) VALUES (1), (2), (3) RETURNING id, consumed').all();
            const updated = db.prepare('UPDATE claims SET consumed = consumed + 1 WHERE id <= ? RETURNING id, consumed').values(2);
            const removed = [...db.prepare('DELETE FROM claims WHERE id = ? RETURNING id').iterate(3)];
            const first = db.prepare('UPDATE claims SET consumed = consumed + 1 RETURNING id, consumed').get();
            const ignoredRows = db.prepare('UPDATE claims SET consumed = consumed + 1 RETURNING id').run();
            const noProjection = db.prepare('INSERT INTO claims(id, consumed) VALUES (?, ?)').all(4, 7);
            const noMatch = db.prepare('UPDATE claims SET consumed = 99 WHERE id = 99 RETURNING id').get();
            globalThis.sqliteResult = {
              inserted: inserted.sort((a,b) => a.id-b.id), updated: updated.sort((a,b) => a[0]-b[0]),
              removed, firstConsumed: first.consumed, firstIsOneRow: [1,2].includes(first.id),
              changes: ignoredRows.changes, noProjection, noMatch,
              rows: db.query('SELECT id, consumed FROM claims ORDER BY id').all(), inTransaction: db.inTransaction,
            };
            db.close();
        ").await;
        assert_eq!(
            result,
            json!({
                "inserted": [{"id":1,"consumed":0},{"id":2,"consumed":0},{"id":3,"consumed":0}],
                "updated": [[1,1],[2,1]], "removed": [{"id":3}], "firstConsumed":2, "firstIsOneRow":true,
                "changes":2, "noProjection":[], "noMatch":null,
                "rows":[{"id":1,"consumed":3},{"id":2,"consumed":3},{"id":4,"consumed":7}],
                "inTransaction":false,
            })
        );
    });
}

#[test]
fn sqlite_returning_encoding_failure_rolls_back_only_its_own_statement() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "returning-limit", None).await;
        let result = evaluate(&runtime, r"
            const db = new sqlite.Database(':memory:');
            db.exec('CREATE TABLE state(value INTEGER, payload TEXT); CREATE TABLE audit(value TEXT)');
            db.prepare('INSERT INTO state VALUES(0, ?)').run('x'.repeat(600 * 1024));
            const tooLarge = db.prepare('UPDATE state SET value = value + 1 RETURNING payload || payload AS expanded');
            let autocommitRejected = false;
            try { tooLarge.all(); } catch (error) { autocommitRejected = /cell exceeds/.test(error.message); }
            const afterAutocommit = db.query('SELECT value FROM state').get().value;
            const noLeakedTransaction = !db.inTransaction;
            db.prepare('BEGIN IMMEDIATE').run();
            db.prepare('INSERT INTO audit VALUES(?)').run('before');
            let nestedRejected = false;
            try { tooLarge.get(); } catch (error) { nestedRejected = /cell exceeds/.test(error.message); }
            const enclosingTransactionSurvived = db.inTransaction;
            db.prepare('INSERT INTO audit VALUES(?)').run('after');
            db.prepare('COMMIT').run();
            globalThis.sqliteResult = {
              autocommitRejected, afterAutocommit, noLeakedTransaction,
              nestedRejected, enclosingTransactionSurvived,
              finalValue: db.query('SELECT value FROM state').get().value,
              audit: db.query('SELECT value FROM audit ORDER BY rowid').values(),
            };
            db.close();
        ").await;
        assert_eq!(
            result,
            json!({
                "autocommitRejected":true, "afterAutocommit":0, "noLeakedTransaction":true,
                "nestedRejected":true, "enclosingTransactionSurvived":true,
                "finalValue":0, "audit":[["before"],["after"]],
            })
        );
    });
}

#[test]
fn sqlite_returning_row_limit_rolls_back_and_select_get_stops_after_one_row() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "returning-rows", None).await;
        let result = evaluate(
            &runtime,
            r"
            const db = new sqlite.Database(':memory:');
            db.exec('CREATE TABLE many_rows(value INTEGER)');
            // Each seed batch remains below the SQL token limit. Two copies
            // reach 10,004 rows without 10,004 separate native calls.
            db.exec('INSERT INTO many_rows(value) VALUES ' + Array(2501).fill('(0)').join(','));
            db.exec('INSERT INTO many_rows SELECT value FROM many_rows');
            db.exec('INSERT INTO many_rows SELECT value FROM many_rows');
            const first = db.query('SELECT value FROM many_rows').get();
            let rejected = false;
            try { db.prepare('UPDATE many_rows SET value = 1 RETURNING value').all(); }
            catch (error) { rejected = /row or time limit/.test(error.message); }
            globalThis.sqliteResult = {
              first, rejected, inTransaction: db.inTransaction,
              state: db.query('SELECT count(*) AS count, sum(value) AS sum FROM many_rows').get(),
            };
            db.close();
        ",
        )
        .await;
        assert_eq!(
            result,
            json!({"first":{"value":0}, "rejected":true, "inTransaction":false,
            "state":{"count":10004,"sum":0}})
        );
    });
}

#[test]
fn sqlite_prepared_ddl_preserves_defaults_and_checks_from_its_original_source() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "ddl-source", None).await;
        let result = evaluate(&runtime, r#"
            const db = new sqlite.Database(':memory:');
            db.prepare("/* original source offsets */ CREATE TABLE quoted(value TEXT DEFAULT ('semi;''quote λ'), n INTEGER CHECK(n > 3))").run();
            db.prepare('INSERT INTO quoted(n) VALUES(?)').run(4);
            let constraintRejected = false;
            try { db.prepare('INSERT INTO quoted(n) VALUES(?)').run(2); } catch { constraintRejected = true; }
            globalThis.sqliteResult = {constraintRejected, rows: db.query('SELECT value, n FROM quoted').all()};
            db.close();
        "#).await;
        assert_eq!(
            result,
            json!({"constraintRejected":true, "rows":[{"value":"semi;'quote λ","n":4}]})
        );
    });
}

#[test]
fn sqlite_returning_preserves_native_conflict_policies_and_dqs_literals() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "conflict-policy", None).await;
        let result = evaluate(&runtime, r#"
            const db = new sqlite.Database(':memory:');
            db.exec('CREATE TABLE conflicts(id INTEGER PRIMARY KEY, value TEXT)');
            const quoted = db.prepare('INSERT INTO conflicts VALUES (9, "DQS literal") RETURNING id, value').get();
            let failRejected = false;
            try { db.prepare("INSERT OR FAIL INTO conflicts VALUES (1,'kept'), (9,'duplicate'), (2,'unreached') RETURNING id").all(); }
            catch { failRejected = true; }
            const afterFail = db.query('SELECT id FROM conflicts ORDER BY id').values();
            db.exec('BEGIN IMMEDIATE');
            db.run("INSERT INTO conflicts VALUES (5,'outer transaction')");
            try { db.prepare("INSERT OR FAIL INTO conflicts VALUES (3,'kept in transaction'), (9,'duplicate') RETURNING id").all(); } catch {}
            try { db.prepare("INSERT OR ABORT INTO conflicts VALUES (6,'must roll back'), (9,'duplicate') RETURNING id").all(); } catch {}
            const duringTransaction = db.query('SELECT id FROM conflicts ORDER BY id').values();
            const stillInTransaction = db.inTransaction;
            let rollbackRejected = false;
            try { db.prepare("INSERT OR ROLLBACK INTO conflicts VALUES (7,'must roll back'), (9,'duplicate') RETURNING id").all(); }
            catch { rollbackRejected = true; }
            db.exec('CREATE TABLE schema_fail(id INTEGER UNIQUE ON CONFLICT FAIL)');
            db.run('INSERT INTO schema_fail VALUES (9)');
            try { db.prepare('INSERT INTO schema_fail VALUES (1), (9), (2) RETURNING id').all(); } catch {}
            globalThis.sqliteResult = {
              quoted, failRejected, afterFail, duringTransaction, stillInTransaction,
              rollbackRejected, transactionEnded: !db.inTransaction,
              finalRows: db.query('SELECT id FROM conflicts ORDER BY id').values(),
              schemaFailRows: db.query('SELECT id FROM schema_fail ORDER BY id').values(),
            };
            db.close();
        "#).await;
        assert_eq!(
            result,
            json!({
                "quoted":{"id":9,"value":"DQS literal"}, "failRejected":true,
                "afterFail":[[1],[9]], "duringTransaction":[[1],[3],[5],[9]], "stillInTransaction":true,
                "rollbackRejected":true, "transactionEnded":true, "finalRows":[[1],[9]],
                "schemaFailRows":[[1],[9]],
            })
        );
    });
}

#[test]
fn sqlite_schema_and_sql_cannot_bypass_file_capabilities() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "sqlite-security", None).await;
        let result = evaluate(&runtime, r#"
            const db = new sqlite.Database(':memory:');
            db.exec('CREATE TABLE protected(value TEXT)');
            const attacks = [
              `ATTACH DATABASE '/tmp/escape.db' AS outside`,
              `/* prefix */ VaCuUm INTO '/tmp/escape.db'`,
              `PRAGMA temp_store_directory = '/tmp'`,
              `PRAGMA main.journal_mode = WAL`,
              `PRAGMA writable_schema = ON`,
              `SELECT "load_extension"('/tmp/native.so')`,
              "SELECT `readfile`('/tmp/private')",
              `SELECT [writefile]('/tmp/private','bad')`,
              `SELECT 'load_extension'('/tmp/native.so')`,
              `SELECT * FROM pragma_temp_store_directory('/tmp')`,
              `CREATE VIRTUAL TABLE files USING csv(filename='/tmp/private')`,
              `CREATE TRIGGER bad AFTER INSERT ON protected BEGIN SELECT 'writefile'('/tmp/private','bad'); END`,
              `INSERT INTO protected VALUES('must not commit'); /* tail */ ATTACH '/tmp/escape.db' AS x`,
            ];
            const denied = attacks.map(sql => { try { db.exec(sql); return false; } catch { return true; } });
            db.prepare("INSERT INTO protected VALUES (?)").run("ATTACH; readfile('text only')");
            globalThis.sqliteResult = {
              denied, rows: db.query('SELECT value FROM protected').all(),
              aliases: db.query('PRAGMA database_list').all().map(row => row.name),
            };
            db.close();
        "#).await;
        assert!(
            result["denied"]
                .as_array()
                .unwrap()
                .iter()
                .all(|value| value == &json!(true))
        );
        assert_eq!(
            result["rows"],
            json!([{"value": "ATTACH; readfile('text only')"}])
        );
        assert!(
            result["aliases"]
                .as_array()
                .unwrap()
                .iter()
                .all(|name| name == "main" || name == "temp")
        );
    });
}

#[test]
fn sqlite_file_open_requires_its_own_storage_grant_and_write_capability() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("owner");
        std::fs::create_dir(&storage).expect("storage");
        let own = serde_json::to_string(&storage.join("own.db").to_string_lossy()).unwrap();
        let outside = serde_json::to_string(&root.join("outside.db").to_string_lossy()).unwrap();
        let traversal =
            serde_json::to_string(&storage.join("../escape.db").to_string_lossy()).unwrap();
        let runtime = runtime(&root, "owner", None).await;
        runtime
            .add_persistent_storage_root(storage.clone(), "owner")
            .expect("owner grant");
        let result = evaluate(&runtime, &format!(r"
            const paths = [{outside}, {traversal}, 'file:{own}'];
            const denied = paths.map(path => {{ try {{ new sqlite.Database(path); return false; }} catch {{ return true; }} }});
            const db = new sqlite.Database({own});
            db.exec('CREATE TABLE data(value TEXT)');
            db.close();
            globalThis.sqliteResult = denied;
        ")).await;
        assert_eq!(result, json!([true, true, true]));
        assert!(!root.join("outside.db").exists());
        assert!(!root.join("escape.db").exists());
        let peer = self::runtime(&root, "peer", None).await;
        peer.add_persistent_storage_root(storage.clone(), "owner")
            .expect("foreign owner grant");
        assert_eq!(
            evaluate(
                &peer,
                &format!(
                    r"
            try {{ new sqlite.Database({own}); globalThis.sqliteResult = false; }}
            catch {{ globalThis.sqliteResult = true; }}
        "
                )
            )
            .await,
            json!(true)
        );
        let mut policy = ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            ..Default::default()
        };
        policy.deny_caps.push("write".into());
        let restricted = self::runtime(&root, "owner", Some(policy)).await;
        restricted
            .add_persistent_storage_root(storage, "owner")
            .expect("restricted grant");
        assert_eq!(
            evaluate(
                &restricted,
                &format!(
                    r"
            try {{ new sqlite.Database({own}); globalThis.sqliteResult = false; }}
            catch {{ globalThis.sqliteResult = true; }}
        "
                )
            )
            .await,
            json!(true)
        );
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn sqlite_readonly_open_requires_write_authority_before_native_namespace_repair() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let source = storage.join("source.db");
        let copied = storage.join("copied.db");
        let main_only = storage.join("main-only.db");
        let source_json = serde_json::to_string(&source.to_string_lossy()).unwrap();
        let copied_json = serde_json::to_string(&copied.to_string_lossy()).unwrap();
        let main_only_json = serde_json::to_string(&main_only.to_string_lossy()).unwrap();
        let seed = runtime(&root, "owner", None).await;
        seed.add_persistent_storage_root(storage.clone(), "owner")
            .expect("seed storage grant");
        assert_eq!(
            evaluate(
                &seed,
                &format!(
                    r#"
                const db = new sqlite.Database({source_json});
                db.exec("CREATE TABLE data(value TEXT); INSERT INTO data VALUES ('durable')");
                db.close();
                globalThis.sqliteResult = true;
            "#
                )
            )
            .await,
            json!(true)
        );
        drop(seed);

        // Both are ordinary database copies, not adversarial symlinks. The
        // first keeps stale native file-identity records; the second has no
        // namespace records. The engine repairs the stale records during open
        // even when its main pager is read-only. File capability admission is
        // conservative and does not depend on which companions exist today.
        std::fs::copy(&source, &copied).expect("copy main database");
        std::fs::copy(&source, &main_only).expect("copy without namespace");
        for suffix in ["-fsqlite-ns-gate", "-fsqlite-ns-use"] {
            std::fs::copy(
                storage.join(format!("source.db{suffix}")),
                storage.join(format!("copied.db{suffix}")),
            )
            .expect("copy native namespace sidecar");
        }
        let snapshot = || {
            std::fs::read_dir(&storage)
                .expect("storage entries")
                .map(|entry| {
                    let entry = entry.expect("storage entry");
                    (
                        entry.file_name(),
                        std::fs::read(entry.path()).expect("database family bytes"),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = snapshot();
        let mut policy = ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            ..Default::default()
        };
        policy.deny_caps.push("write".into());
        let restricted = runtime(&root, "owner", Some(policy)).await;
        restricted
            .add_persistent_storage_root(storage.clone(), "owner")
            .expect("restricted storage grant");
        let rejected = evaluate(
            &restricted,
            &format!(
                r"
            const results = [];
            for (const path of [{copied_json}, {main_only_json}]) {{
              for (const readonly of [true, false]) {{
                try {{
                  const db = new sqlite.Database(path, {{readonly, create:true}});
                  db.close();
                  results.push(false);
                }} catch (error) {{
                  const message = String(error);
                  results.push(message.includes('SQLite write capability denied') &&
                    (!readonly || message.includes('native coordination files')));
                }}
              }}
            }}
            globalThis.sqliteResult = results;
        "
            ),
        )
        .await;
        assert_eq!(rejected, json!([true, true, true, true]));
        assert_eq!(
            snapshot(),
            before,
            "denial must precede all native file I/O"
        );
        drop(restricted);

        // Positive control: the very same copied database opens with the
        // appropriate capability, repairs its identity, and remains SQL
        // read-only. This proves the fixture reaches a real native repair.
        let namespace = storage.join("copied.db-fsqlite-ns-use");
        let copied_identity = std::fs::read(&namespace).expect("copied identity");
        let allowed = runtime(&root, "owner", None).await;
        allowed
            .add_persistent_storage_root(storage, "owner")
            .expect("allowed storage grant");
        let result = evaluate(
            &allowed,
            &format!(
                r#"
            const db = new sqlite.Database({copied_json}, {{readonly:true, create:true}});
            let writeDenied = false;
            try {{ db.exec("INSERT INTO data VALUES ('forbidden')"); }}
            catch (error) {{ writeDenied = error.code === 'SQLITE_READONLY'; }}
            globalThis.sqliteResult = {{
              rows:db.query('SELECT value FROM data').all(), writeDenied,
            }};
            db.close();
        "#
            ),
        )
        .await;
        assert_eq!(
            result,
            json!({"rows":[{"value":"durable"}], "writeDenied":true})
        );
        assert_ne!(
            std::fs::read(namespace).expect("rebound native identity"),
            copied_identity,
            "permitted read-only open must rebind the copied native namespace"
        );
    });
}

#[cfg(unix)]
#[test]
fn sqlite_rejects_database_and_companion_symlinks_without_touching_targets() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    use std::os::unix::fs::symlink;
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let outside = root.join("outside");
        std::fs::write(&outside, b"sentinel").expect("sentinel");
        symlink(&outside, storage.join("linked.db")).expect("main symlink");
        symlink(&outside, storage.join("sidecar.db-wal")).expect("WAL symlink");
        symlink(&outside, storage.join("locks.db-lock-shared")).expect("Windows sidecar symlink");
        std::fs::hard_link(&outside, storage.join("hard.db")).expect("main hard link");
        let runtime = runtime(&root, "owner", None).await;
        runtime
            .add_persistent_storage_root(storage.clone(), "owner")
            .expect("grant");
        let paths = serde_json::to_string(&[
            storage.join("linked.db"),
            storage.join("sidecar.db"),
            storage.join("locks.db"),
            storage.join("hard.db"),
        ])
        .unwrap();
        assert_eq!(
            evaluate(
                &runtime,
                &format!(
                    r"
            globalThis.sqliteResult = {paths}.map(path => {{
              try {{ new sqlite.Database(path); return false; }} catch {{ return true; }}
            }});
        "
                )
            )
            .await,
            json!([true, true, true, true])
        );
        assert_eq!(std::fs::read(outside).unwrap(), b"sentinel");
        assert!(!storage.join("sidecar.db").exists());
        assert!(!storage.join("locks.db").exists());
    });
}

// Native first-open migration writes these even though guest SQL never names
// them. WAL's repair worker also opens a stable lock companion. Keep the
// regression cases explicit: an omitted native path must fail before the
// engine creates its main file or repairs a copied database's namespace.
const NATIVE_RECOVERY_COMPANIONS: &[&str] = &[
    ".fsqlite-migration-state",
    ".fsqlite-migration-state.tmp",
    ".pre-migration-bak",
    ".pre-migration-bak.tmp",
    ".pre-migration-bak-wal",
    ".pre-migration-bak-wal.tmp",
    ".pre-migration-bak-shm",
    ".pre-migration-bak-shm.tmp",
    "-wal-fec.lock",
    "-journal-lock-shared",
    "-journal-lock-reserved",
    "-journal-lock-pending",
    "-wal-cert-lock-shared",
    "-wal-cert-lock-reserved",
    "-wal-cert-lock-pending",
    "-wal-cert-head-lock-shared",
    "-wal-cert-head-lock-reserved",
    "-wal-cert-head-lock-pending",
];

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn assert_linked_native_recovery_companions_are_rejected(
    link: fn(&Path, &Path) -> std::io::Result<()>,
) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = safe_canonicalize(temp.path());
    let workspace = root.join("workspace");
    let storage = root.join("storage");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&storage).expect("storage");
    let outside = root.join("outside-grant");
    std::fs::write(&outside, b"outside sentinel").expect("sentinel");
    let seed = storage.join("seed.db");
    let seed_json = serde_json::to_string(&seed).unwrap();
    let runtime = runtime(&workspace, "owner", None).await;
    runtime
        .add_persistent_storage_root(storage.clone(), "owner")
        .expect("storage grant");
    assert_eq!(
        evaluate(
            &runtime,
            &format!(
                r#"
        const database = new sqlite.Database({seed_json});
        database.exec("CREATE TABLE events(data TEXT); INSERT INTO events VALUES ('durable seed')");
        database.close();
        globalThis.sqliteResult = true;
    "#
            )
        )
        .await,
        json!(true)
    );
    let seed_bytes = std::fs::read(&seed).expect("committed native database");
    for (index, suffix) in NATIVE_RECOVERY_COMPANIONS.iter().enumerate() {
        for existing in [false, true] {
            let name = format!("recovery-{index}-{existing}.db");
            let database = storage.join(&name);
            if existing {
                // A normal copied main file has no native admission artifacts
                // yet. Read-only open still needs those coordination writes.
                std::fs::copy(&seed, &database).expect("copy native database");
            }
            let companion = storage.join(format!("{name}{suffix}"));
            link(&outside, &companion).expect("linked native recovery companion");
            let entries = || {
                std::fs::read_dir(&storage)
                    .expect("storage entries")
                    .map(|entry| entry.expect("storage entry").file_name())
                    .collect::<std::collections::BTreeSet<_>>()
            };
            let before = entries();
            let filename = serde_json::to_string(&database).unwrap();
            let rejected = evaluate(
                &runtime,
                &format!(
                    r"
            try {{
              const database = new sqlite.Database({filename}, {{readonly:{existing}, create:true}});
              database.close();
              globalThis.sqliteResult = false;
            }} catch (error) {{
              globalThis.sqliteResult = String(error).includes('persistent storage');
            }}
        "
                ),
            )
            .await;
            assert_eq!(rejected, json!(true), "{suffix}, readonly={existing}");
            assert_eq!(
                entries(),
                before,
                "denial must precede native companion creation: {suffix}"
            );
            if existing {
                assert_eq!(
                    std::fs::read(&database).expect("copied database after denial"),
                    seed_bytes,
                    "read-only denial must not repair or rewrite the copied main file"
                );
            } else {
                assert!(
                    !database.exists(),
                    "denial must precede creation of the main file: {suffix}"
                );
            }
            assert_eq!(
                std::fs::read(&outside).expect("outside sentinel after denial"),
                b"outside sentinel",
                "native recovery must not follow {suffix} outside its grant"
            );
        }
    }
    assert_eq!(
        evaluate(
            &runtime,
            &format!(
                r"
        const database = new sqlite.Database({seed_json}, {{readonly:true}});
        globalThis.sqliteResult = database.query('SELECT data FROM events').get();
        database.close();
    "
            )
        )
        .await,
        json!({"data":"durable seed"}),
        "failed admission must leave ordinary native SQLite usable"
    );
}

#[cfg(unix)]
#[test]
fn sqlite_rejects_linked_migration_and_repair_companions_before_native_open() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(assert_linked_native_recovery_companions_are_rejected(
        |from, to| std::os::unix::fs::symlink(from, to),
    ));
}

#[cfg(any(unix, windows))]
#[test]
fn sqlite_rejects_hardlinked_migration_and_repair_companions_before_native_open() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(assert_linked_native_recovery_companions_are_rejected(
        |from, to| std::fs::hard_link(from, to),
    ));
}

#[cfg(unix)]
#[test]
fn sqlite_recovery_temporaries_cannot_create_files_through_dangling_links() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let runtime = runtime(&root, "owner", None).await;
        runtime
            .add_persistent_storage_root(storage.clone(), "owner")
            .expect("grant");
        for (index, suffix) in [".fsqlite-migration-state.tmp", "-wal-fec.lock"]
            .iter()
            .enumerate()
        {
            let name = format!("dangling-{index}.db");
            let database = storage.join(&name);
            let outside = root.join(format!("outside-{index}"));
            std::os::unix::fs::symlink(&outside, storage.join(format!("{name}{suffix}")))
                .expect("dangling native companion");
            let filename = serde_json::to_string(&database).unwrap();
            assert_eq!(
                evaluate(&runtime, &format!(r"
                    try {{ new sqlite.Database({filename}); globalThis.sqliteResult = false; }}
                    catch (error) {{ globalThis.sqliteResult = String(error).includes('persistent storage'); }}
                ")).await,
                json!(true),
                "{suffix}"
            );
            assert!(!database.exists(), "native main creation must not run");
            assert!(
                !outside.exists(),
                "native open must not create an outside file"
            );
        }
    });
}

#[cfg(any(unix, windows))]
#[test]
#[allow(clippy::too_many_lines)]
fn sqlite_retires_handles_when_native_recovery_companions_are_replaced_by_links() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let outside = root.join("outside-grant");
        std::fs::write(&outside, b"untouched outside state").expect("sentinel");
        let runtime = runtime(&root, "owner", None).await;
        runtime
            .add_persistent_storage_root(storage.clone(), "owner")
            .expect("grant");
        for (index, suffix) in NATIVE_RECOVERY_COMPANIONS.iter().enumerate() {
            let name = format!("live-{index}.db");
            let database = storage.join(&name);
            let filename = serde_json::to_string(&database).unwrap();
            assert_eq!(
                evaluate(&runtime, &format!(r#"
                    globalThis.liveRecoveryDatabase = new sqlite.Database({filename});
                    liveRecoveryDatabase.exec("CREATE TABLE events(data TEXT); INSERT INTO events VALUES ('committed')");
                    globalThis.sqliteResult = liveRecoveryDatabase.query('SELECT data FROM events').get();
                "#)).await,
                json!({"data":"committed"})
            );
            let main_before = std::fs::read(&database).expect("committed main file");
            let companion = storage.join(format!("{name}{suffix}"));
            if companion.exists() {
                // The marker is already present from the real native open.
                // Preserve it while simulating a later namespace substitution.
                std::fs::rename(&companion, storage.join(format!("{name}{suffix}.saved")))
                    .expect("preserve original native companion");
            }
            std::fs::hard_link(&outside, &companion).expect("late linked companion");
            let result = evaluate(
                &runtime,
                r#"
                let readDenied = false;
                let reuseDenied = false;
                let closeDenied = false;
                try { liveRecoveryDatabase.query('SELECT * FROM events').all(); }
                catch (error) { readDenied = String(error).includes('persistent storage'); }
                try { liveRecoveryDatabase.exec("INSERT INTO events VALUES ('must not commit')"); }
                catch { reuseDenied = true; }
                try { liveRecoveryDatabase.close(); }
                catch { closeDenied = true; }
                globalThis.liveRecoveryDatabase = null;
                globalThis.sqliteResult = {readDenied, reuseDenied, closeDenied};
            "#,
            )
            .await;
            assert_eq!(
                result,
                json!({"readDenied":true,"reuseDenied":true,"closeDenied":true}),
                "late replacement must retire the native handle: {suffix}"
            );
            assert_eq!(
                std::fs::read(database).expect("main file after retirement"),
                main_before,
                "retirement must not commit further writes"
            );
            assert_eq!(
                std::fs::read(&outside).expect("sentinel after retirement"),
                b"untouched outside state"
            );
        }
    });
}

#[test]
fn sqlite_counts_native_recovery_backups_in_the_aggregate_storage_budget() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let runtime = runtime(&root, "owner", None).await;
        runtime
            .add_persistent_storage_root(storage.clone(), "owner")
            .expect("grant");
        let seed = storage.join("seed.db");
        let seed_json = serde_json::to_string(&seed).unwrap();
        evaluate(
            &runtime,
            &format!(
                r#"
            const db = new sqlite.Database({seed_json});
            db.exec("CREATE TABLE events(data TEXT); INSERT INTO events VALUES ('durable')");
            db.close();
            globalThis.sqliteResult = true;
        "#
            ),
        )
        .await;
        let database = storage.join("over-budget.db");
        std::fs::copy(seed, &database).expect("copy existing native database");
        let main_before = std::fs::read(&database).expect("main contents");
        let main_bytes = database.metadata().expect("main size").len();
        let backup = storage.join("over-budget.db.pre-migration-bak.tmp");
        let limit = 128 * 1024 * 1024_u64;
        let backup_bytes = limit - main_bytes + 1;
        assert!(backup_bytes < limit, "each individual file fits the limit");
        std::fs::File::create(&backup)
            .expect("backup temporary")
            .set_len(backup_bytes)
            .expect("sparse backup exceeding aggregate family budget");
        let filename = serde_json::to_string(&database).unwrap();
        let result = evaluate(
            &runtime,
            &format!(
                r"
            try {{
              const db = new sqlite.Database({filename}, {{readonly:true, create:true}});
              db.close();
              globalThis.sqliteResult = false;
            }} catch (error) {{
              globalThis.sqliteResult = String(error).includes('exceed 128 MiB');
            }}
        "
            ),
        )
        .await;
        assert_eq!(result, json!(true), "backup must count before native open");
        assert_eq!(std::fs::read(database).unwrap(), main_before);
        assert_eq!(backup.metadata().unwrap().len(), backup_bytes);
        for suffix in [
            "-fsqlite-ns-gate",
            "-fsqlite-ns-use",
            ".fsqlite-migration-state",
        ] {
            assert!(
                !storage.join(format!("over-budget.db{suffix}")).exists(),
                "quota denial must precede native admission"
            );
        }
    });
}

#[test]
fn sqlite_fts5_search_uses_real_indexed_content() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(temp.path(), "fts", None).await;
        let result = evaluate(&runtime, r#"
            const db = new sqlite.Database(':memory:');
            db.exec("CREATE VIRTUAL TABLE chunks USING fts5(title, content, tokenize='porter unicode61')");
            const insert = db.prepare('INSERT INTO chunks(title, content) VALUES (?, ?)');
            insert.run('SQLite', 'Transactions keep persistent session records consistent');
            insert.run('Unrelated', 'A completely different document');
            globalThis.sqliteResult = db.prepare('SELECT title FROM chunks WHERE chunks MATCH ? ORDER BY bm25(chunks)').all('transaction');
            db.close();
        "#).await;
        assert_eq!(result, json!([{"title": "SQLite"}]));
    });
}

#[test]
fn sqlite_memory_obeys_sql_readonly_without_filesystem_capabilities() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut policy = ExtensionPolicy {
            mode: ExtensionPolicyMode::Strict,
            ..Default::default()
        };
        policy.deny_caps.extend(["read".into(), "write".into()]);
        let runtime = runtime(temp.path(), "readonly", Some(policy)).await;
        let result = evaluate(
            &runtime,
            r"
            const db = new sqlite.Database(':memory:', {readonly: true, create: true});
            let writeDenied = false;
            try { db.prepare('CREATE TABLE forbidden(value TEXT)').run(); }
            catch { writeDenied = true; }
            const row = db.query('SELECT 42 AS value').get();
            db.close();
            const memory = new sqlite.Database(':memory:');
            memory.exec('CREATE TABLE data(value); INSERT INTO data VALUES (7)');
            globalThis.sqliteResult = {
              writeDenied, row, writable:memory.query('SELECT value FROM data').get(),
            };
            memory.close();
        ",
        )
        .await;
        assert_eq!(
            result,
            json!({"writeDenied":true, "row":{"value":42}, "writable":{"value":7}})
        );
    });
}

#[test]
#[allow(clippy::large_futures)]
fn sqlite_rejects_preexisting_schema_triggers() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let path = storage.join("preexisting.db");
        let seed_path = path.to_string_lossy().into_owned();
        // This host-created database deliberately bypasses guest SQL
        // admission to reproduce an unsafe schema present before opening.
        std::thread::Builder::new().name("sqlite-schema-fixture".into())
            .stack_size(16 * 1024 * 1024).spawn(move || {
                futures::executor::block_on(async {
                    let mut env = fsqlite::ConnectionEnv::default();
                    env.mark_blocking_io_inline_safe();
                    let conn = fsqlite::Connection::open_with_env(seed_path, env).await.expect("seed database");
                    conn.execute_batch("CREATE TABLE data(value TEXT); CREATE TABLE audit(value TEXT); CREATE TRIGGER changed AFTER INSERT ON data BEGIN INSERT INTO audit VALUES(new.value); END")
                        .await.expect("seed trigger");
                    conn.close().await.expect("close fixture");
                });
            }).expect("fixture worker").join().expect("fixture worker join");
        let runtime = runtime(&root, "owner", None).await;
        runtime
            .add_persistent_storage_root(storage, "owner")
            .expect("grant");
        let path = serde_json::to_string(&path.to_string_lossy()).unwrap();
        let error = evaluate(
            &runtime,
            &format!(
                r"
            try {{ new sqlite.Database({path}); globalThis.sqliteResult = 'unexpected success'; }}
            catch (error) {{ globalThis.sqliteResult = error.message; }}
        "
            ),
        )
        .await;
        assert!(error.as_str().unwrap().contains("triggers"), "{error}");
    });
}

#[cfg(unix)]
#[test]
fn sqlite_retires_a_handle_if_a_companion_becomes_a_symlink() {
    let _serial = SQLITE_TEST_LOCK.lock().expect("SQLite test lock");
    use std::os::unix::fs::symlink;
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = safe_canonicalize(temp.path());
        let storage = root.join("storage");
        std::fs::create_dir(&storage).expect("storage");
        let path = storage.join("live.db");
        let filename = serde_json::to_string(&path.to_string_lossy()).unwrap();
        let runtime = runtime(&root, "owner", None).await;
        runtime
            .add_persistent_storage_root(storage.clone(), "owner")
            .expect("grant");
        evaluate(
            &runtime,
            &format!(
                r"
            globalThis.liveDatabase = new sqlite.Database({filename});
            liveDatabase.exec('CREATE TABLE data(value TEXT)');
            liveDatabase.prepare('INSERT INTO data VALUES (?)').run('durable before link');
            globalThis.sqliteResult = true;
        "
            ),
        )
        .await;
        let outside = root.join("outside");
        std::fs::write(&outside, b"sentinel").expect("sentinel");
        // This Windows companion is also checked on Unix, where the native
        // engine has not created it. No database file needs to be removed.
        symlink(&outside, storage.join("live.db-lock-pending")).expect("late companion link");
        let result = evaluate(&runtime, r#"
            let readDenied = false;
            let reuseDenied = false;
            try { liveDatabase.query('SELECT * FROM data').all(); } catch { readDenied = true; }
            try { liveDatabase.exec("INSERT INTO data VALUES ('must fail')"); } catch { reuseDenied = true; }
            globalThis.sqliteResult = {readDenied, reuseDenied};
        "#).await;
        assert_eq!(result, json!({"readDenied": true, "reuseDenied": true}));
        drop(runtime);
        assert_eq!(std::fs::read(outside).unwrap(), b"sentinel");
    });
}
