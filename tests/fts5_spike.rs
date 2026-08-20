//! FTS5 capability spike (bd-cv653.4.1): the bead mandates proving the
//! fsqlite build exposes FTS5 BEFORE building the memory store. This test
//! is the spike; it stays as the store's migration smoke test.

#[test]
fn fts5_create_insert_match_roundtrip() {
    // fsqlite's engine futures overflow the platform-default thread stack
    // (see src/session_sqlite.rs); drive them on a big-stack thread.
    let handle = std::thread::Builder::new()
        .name("fts5-spike".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let dir = std::env::temp_dir().join(format!("fts5-spike-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let db = dir.join("spike.sqlite");
            let conn = futures::executor::block_on(fsqlite::Connection::open_strict_multi_process(
                db.to_string_lossy().into_owned(),
            ))
            .expect("open");
            futures::executor::block_on(
                conn.execute_batch("CREATE VIRTUAL TABLE memories_fts USING fts5(content)"),
            )
            .expect("CREATE VIRTUAL TABLE ... fts5 must succeed with the fts5 feature");
            futures::executor::block_on(
                conn.execute_batch(
                    "INSERT INTO memories_fts(content) VALUES ('hello memory world')",
                ),
            )
            .expect("insert");
            let rows = futures::executor::block_on(conn.query_with_params(
                "SELECT rowid, content FROM memories_fts WHERE memories_fts MATCH ?",
                &[fsqlite::SqliteValue::Text("hello".into())],
            ))
            .expect("MATCH query");
            assert_eq!(rows.len(), 1, "FTS5 MATCH must return the inserted row");
        })
        .expect("spawn spike thread");
    handle.join().expect("spike thread panicked");
}
