//! Unit tests for the node:fs shim (bd-1av0.1).
//!
//! Tests the virtual filesystem, sync/async operations, host FS fallback,
//! stat objects, promises namespace, and callback-based async functions.
#![allow(clippy::needless_raw_string_hashes)]

use std::collections::HashMap;
use std::sync::Arc;

use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig, PiJsRuntimeLimits};
use pi::scheduler::DeterministicClock;

fn default_config() -> PiJsRuntimeConfig {
    PiJsRuntimeConfig {
        cwd: "/test".to_string(),
        args: vec![],
        env: HashMap::new(),
        limits: PiJsRuntimeLimits::default(),
    }
}

// ---------------------------------------------------------------------------
// VFS core: write + read round-trip
// ---------------------------------------------------------------------------

#[test]
fn fs_write_read_roundtrip() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFileSync('/tmp/hello.txt', 'Hello, world!');
                    globalThis.content = fs.readFileSync('/tmp/hello.txt', 'utf8');
                    globalThis.exists = fs.existsSync('/tmp/hello.txt');
                    globalThis.missing = fs.existsSync('/tmp/nope.txt');
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let content: serde_json::Value = runtime.read_global_json("content").await.unwrap();
        assert_eq!(content, "Hello, world!");

        let exists: serde_json::Value = runtime.read_global_json("exists").await.unwrap();
        assert_eq!(exists, true);

        let missing: serde_json::Value = runtime.read_global_json("missing").await.unwrap();
        assert_eq!(missing, false);
    });
}

// ---------------------------------------------------------------------------
// stat objects shape
// ---------------------------------------------------------------------------

#[test]
fn fs_stat_object_shape() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFileSync('/tmp/stat_test.txt', 'abcdef');
                    const s = fs.statSync('/tmp/stat_test.txt');
                    globalThis.statResult = {
                        isFile: s.isFile(),
                        isDir: s.isDirectory(),
                        isSymlink: s.isSymbolicLink(),
                        size: s.size,
                        hasMode: typeof s.mode === 'number',
                        hasBlksize: typeof s.blksize === 'number',
                    };

                    fs.mkdirSync('/tmp/mydir');
                    const ds = fs.statSync('/tmp/mydir');
                    globalThis.dirStat = {
                        isFile: ds.isFile(),
                        isDir: ds.isDirectory(),
                    };
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let s: serde_json::Value = runtime.read_global_json("statResult").await.unwrap();
        assert_eq!(s["isFile"], true);
        assert_eq!(s["isDir"], false);
        assert_eq!(s["isSymlink"], false);
        assert_eq!(s["size"], 6); // "abcdef" = 6 bytes
        assert_eq!(s["hasMode"], true);
        assert_eq!(s["hasBlksize"], true);

        let ds: serde_json::Value = runtime.read_global_json("dirStat").await.unwrap();
        assert_eq!(ds["isFile"], false);
        assert_eq!(ds["isDir"], true);
    });
}

// ---------------------------------------------------------------------------
// readdir + withFileTypes
// ---------------------------------------------------------------------------

#[test]
fn fs_readdir_with_filetypes() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.mkdirSync('/proj/src', { recursive: true });
                    fs.writeFileSync('/proj/src/index.js', 'export default 1;');
                    fs.writeFileSync('/proj/src/util.js', 'export default 2;');
                    fs.mkdirSync('/proj/src/lib');

                    globalThis.entries = fs.readdirSync('/proj/src');
                    const dirents = fs.readdirSync('/proj/src', { withFileTypes: true });
                    globalThis.direntNames = dirents.map(d => d.name);
                    globalThis.direntTypes = dirents.map(d => ({
                        name: d.name,
                        isFile: d.isFile(),
                        isDir: d.isDirectory(),
                    }));
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let entries: serde_json::Value = runtime.read_global_json("entries").await.unwrap();
        let arr = entries.as_array().unwrap();
        assert!(arr.contains(&serde_json::json!("index.js")));
        assert!(arr.contains(&serde_json::json!("util.js")));
        assert!(arr.contains(&serde_json::json!("lib")));
        assert_eq!(arr.len(), 3);

        let types: serde_json::Value = runtime.read_global_json("direntTypes").await.unwrap();
        let types_arr = types.as_array().unwrap();
        for entry in types_arr {
            if entry["name"] == "lib" {
                assert_eq!(entry["isDir"], true);
                assert_eq!(entry["isFile"], false);
            } else {
                assert_eq!(entry["isDir"], false);
                assert_eq!(entry["isFile"], true);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// mkdir + unlink + rmdir
// ---------------------------------------------------------------------------

#[test]
fn fs_mkdir_unlink_rmdir() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.mkdirSync('/d1/d2', { recursive: true });
                    fs.writeFileSync('/d1/d2/file.txt', 'data');
                    globalThis.beforeUnlink = fs.existsSync('/d1/d2/file.txt');

                    fs.unlinkSync('/d1/d2/file.txt');
                    globalThis.afterUnlink = fs.existsSync('/d1/d2/file.txt');

                    fs.rmdirSync('/d1/d2');
                    globalThis.afterRmdir = false;
                    try { fs.statSync('/d1/d2'); }
                    catch (_e) { globalThis.afterRmdir = true; }
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let before: serde_json::Value = runtime.read_global_json("beforeUnlink").await.unwrap();
        assert_eq!(before, true);

        let after: serde_json::Value = runtime.read_global_json("afterUnlink").await.unwrap();
        assert_eq!(after, false);

        let rmdir: serde_json::Value = runtime.read_global_json("afterRmdir").await.unwrap();
        assert_eq!(rmdir, true);
    });
}

// ---------------------------------------------------------------------------
// rename + copyFile
// ---------------------------------------------------------------------------

#[test]
fn fs_rename_and_copy() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFileSync('/a.txt', 'original');

                    fs.renameSync('/a.txt', '/b.txt');
                    globalThis.aExists = fs.existsSync('/a.txt');
                    globalThis.bContent = fs.readFileSync('/b.txt', 'utf8');

                    fs.copyFileSync('/b.txt', '/c.txt');
                    globalThis.cContent = fs.readFileSync('/c.txt', 'utf8');
                    globalThis.bStillExists = fs.existsSync('/b.txt');
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let a: serde_json::Value = runtime.read_global_json("aExists").await.unwrap();
        assert_eq!(a, false);

        let b: serde_json::Value = runtime.read_global_json("bContent").await.unwrap();
        assert_eq!(b, "original");

        let c: serde_json::Value = runtime.read_global_json("cContent").await.unwrap();
        assert_eq!(c, "original");

        let b_exists: serde_json::Value = runtime.read_global_json("bStillExists").await.unwrap();
        assert_eq!(b_exists, true);
    });
}

// ---------------------------------------------------------------------------
// appendFileSync
// ---------------------------------------------------------------------------

#[test]
fn fs_append_file() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFileSync('/log.txt', 'line1\n');
                    fs.appendFileSync('/log.txt', 'line2\n');
                    fs.appendFileSync('/log.txt', 'line3\n');
                    globalThis.logContent = fs.readFileSync('/log.txt', 'utf8');
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let content: serde_json::Value = runtime.read_global_json("logContent").await.unwrap();
        assert_eq!(content, "line1\nline2\nline3\n");
    });
}

// ---------------------------------------------------------------------------
// rmSync with recursive
// ---------------------------------------------------------------------------

#[test]
fn fs_rm_recursive() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.mkdirSync('/tree/a/b', { recursive: true });
                    fs.writeFileSync('/tree/a/b/f1.txt', 'd');
                    fs.writeFileSync('/tree/a/f2.txt', 'd');

                    fs.rmSync('/tree', { recursive: true });
                    globalThis.treeGone = !fs.existsSync('/tree');
                    globalThis.fileGone = !fs.existsSync('/tree/a/b/f1.txt');
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let tree: serde_json::Value = runtime.read_global_json("treeGone").await.unwrap();
        assert_eq!(tree, true);

        let file: serde_json::Value = runtime.read_global_json("fileGone").await.unwrap();
        assert_eq!(file, true);
    });
}

// ---------------------------------------------------------------------------
// accessSync
// ---------------------------------------------------------------------------

#[test]
fn fs_access_sync() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFileSync('/acc.txt', 'x');
                    globalThis.accessOk = false;
                    try { fs.accessSync('/acc.txt'); globalThis.accessOk = true; }
                    catch (_e) {}

                    globalThis.accessFail = false;
                    try { fs.accessSync('/no_exist.txt'); }
                    catch (_e) { globalThis.accessFail = true; }
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let ok: serde_json::Value = runtime.read_global_json("accessOk").await.unwrap();
        assert_eq!(ok, true);

        let fail: serde_json::Value = runtime.read_global_json("accessFail").await.unwrap();
        assert_eq!(fail, true);
    });
}

// ---------------------------------------------------------------------------
// promises.readFile / promises.writeFile
// ---------------------------------------------------------------------------

#[test]
fn fs_promises_read_write() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then(async (fs) => {
                    await fs.promises.writeFile('/prom.txt', 'promise data');
                    globalThis.promContent = await fs.promises.readFile('/prom.txt', 'utf8');
                    const s = await fs.promises.stat('/prom.txt');
                    globalThis.promIsFile = s.isFile();
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let content: serde_json::Value = runtime.read_global_json("promContent").await.unwrap();
        assert_eq!(content, "promise data");

        let is_file: serde_json::Value = runtime.read_global_json("promIsFile").await.unwrap();
        assert_eq!(is_file, true);
    });
}

// ---------------------------------------------------------------------------
// node:fs/promises module
// ---------------------------------------------------------------------------

#[test]
fn fs_promises_module_direct() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs/promises').then(async (fsp) => {
                    await fsp.writeFile('/fsp.txt', 'direct promises');
                    globalThis.fspContent = await fsp.readFile('/fsp.txt', 'utf8');
                    await fsp.mkdir('/fsp_dir');
                    const entries = await fsp.readdir('/fsp_dir');
                    globalThis.fspEmpty = entries.length === 0;
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let content: serde_json::Value = runtime.read_global_json("fspContent").await.unwrap();
        assert_eq!(content, "direct promises");

        let empty: serde_json::Value = runtime.read_global_json("fspEmpty").await.unwrap();
        assert_eq!(empty, true);
    });
}

// ---------------------------------------------------------------------------
// promises.copyFile + promises.rename
// ---------------------------------------------------------------------------

#[test]
fn fs_promises_copy_rename() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs/promises').then(async (fsp) => {
                    await fsp.writeFile('/src.txt', 'payload');
                    await fsp.copyFile('/src.txt', '/dst.txt');
                    globalThis.copyOk = await fsp.readFile('/dst.txt', 'utf8');

                    await fsp.rename('/dst.txt', '/moved.txt');
                    globalThis.renameOk = await fsp.readFile('/moved.txt', 'utf8');
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let copy: serde_json::Value = runtime.read_global_json("copyOk").await.unwrap();
        assert_eq!(copy, "payload");

        let rename: serde_json::Value = runtime.read_global_json("renameOk").await.unwrap();
        assert_eq!(rename, "payload");
    });
}

// ---------------------------------------------------------------------------
// symlink/readlink + dirent/lstat semantics
// ---------------------------------------------------------------------------

#[test]
fn fs_symlink_readlink_and_dirent_semantics() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                (async () => {
                    const fs = await import('node:fs');
                    const fsp = await import('node:fs/promises');

                    fs.mkdirSync('/links', { recursive: true });
                    fs.writeFileSync('/links/target.txt', 'payload');
                    fs.symlinkSync('/links/target.txt', '/links/alias.txt');
                    fs.symlinkSync('/links/missing.txt', '/links/broken.txt');

                    globalThis.symlinkReadlinkSync = fs.readlinkSync('/links/alias.txt');
                    globalThis.symlinkStatIsFile = fs.statSync('/links/alias.txt').isFile();
                    globalThis.symlinkLstatIsSymlink = fs.lstatSync('/links/alias.txt').isSymbolicLink();
                    globalThis.brokenExists = fs.existsSync('/links/broken.txt');
                    globalThis.brokenLstatIsSymlink = fs.lstatSync('/links/broken.txt').isSymbolicLink();

                    const dirents = fs.readdirSync('/links', { withFileTypes: true });
                    const aliasEntry = dirents.find((d) => d.name === 'alias.txt');
                    globalThis.direntIsSymlink = aliasEntry ? aliasEntry.isSymbolicLink() : null;

                    await fsp.symlink('/links/target.txt', '/links/alias2.txt');
                    globalThis.symlinkReadlinkPromise = await fsp.readlink('/links/alias2.txt');
                    const lstat = await fsp.lstat('/links/alias2.txt');
                    globalThis.promisesLstatIsSymlink = lstat.isSymbolicLink();
                    await fsp.appendFile('/links/alias2.txt', '-more');
                    globalThis.promisesAppendContent = await fsp.readFile('/links/target.txt', 'utf8');
                })();
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let readlink_sync: serde_json::Value = runtime
            .read_global_json("symlinkReadlinkSync")
            .await
            .unwrap();
        assert_eq!(readlink_sync, "/links/target.txt");

        let stat_is_file: serde_json::Value =
            runtime.read_global_json("symlinkStatIsFile").await.unwrap();
        assert_eq!(stat_is_file, true);

        let lstat_is_symlink: serde_json::Value = runtime
            .read_global_json("symlinkLstatIsSymlink")
            .await
            .unwrap();
        assert_eq!(lstat_is_symlink, true);

        let broken_exists: serde_json::Value =
            runtime.read_global_json("brokenExists").await.unwrap();
        assert_eq!(broken_exists, false);

        let broken_lstat: serde_json::Value = runtime
            .read_global_json("brokenLstatIsSymlink")
            .await
            .unwrap();
        assert_eq!(broken_lstat, true);

        let dirent_symlink: serde_json::Value =
            runtime.read_global_json("direntIsSymlink").await.unwrap();
        assert_eq!(dirent_symlink, true);

        let readlink_promise: serde_json::Value = runtime
            .read_global_json("symlinkReadlinkPromise")
            .await
            .unwrap();
        assert_eq!(readlink_promise, "/links/target.txt");

        let promises_lstat: serde_json::Value = runtime
            .read_global_json("promisesLstatIsSymlink")
            .await
            .unwrap();
        assert_eq!(promises_lstat, true);

        let appended: serde_json::Value = runtime
            .read_global_json("promisesAppendContent")
            .await
            .unwrap();
        assert_eq!(appended, "payload-more");
    });
}

// ---------------------------------------------------------------------------
// callback-based readFile / writeFile
// ---------------------------------------------------------------------------

#[test]
fn fs_callback_read_write() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFile('/cb.txt', 'callback data', (writeErr) => {
                        globalThis.writeErr = writeErr;
                        fs.readFile('/cb.txt', 'utf8', (readErr, data) => {
                            globalThis.readErr = readErr;
                            globalThis.cbData = data;
                        });
                    });
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let write_err: serde_json::Value = runtime.read_global_json("writeErr").await.unwrap();
        assert!(write_err.is_null());

        let read_err: serde_json::Value = runtime.read_global_json("readErr").await.unwrap();
        assert!(read_err.is_null());

        let data: serde_json::Value = runtime.read_global_json("cbData").await.unwrap();
        assert_eq!(data, "callback data");
    });
}

// ---------------------------------------------------------------------------
// callback-based stat / readdir / mkdir / unlink
// ---------------------------------------------------------------------------

#[test]
fn fs_callback_stat_readdir_mkdir_unlink() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFileSync('/cb_stat.txt', 'data');

                    fs.stat('/cb_stat.txt', (err, s) => {
                        globalThis.cbStatIsFile = s.isFile();
                    });

                    fs.mkdir('/cb_dir', (err) => {
                        globalThis.mkdirErr = err;
                        fs.readdir('/cb_dir', (err2, entries) => {
                            globalThis.readdirEmpty = entries.length === 0;
                        });
                    });

                    fs.unlink('/cb_stat.txt', (err) => {
                        globalThis.unlinkErr = err;
                        globalThis.unlinkGone = !fs.existsSync('/cb_stat.txt');
                    });
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let is_file: serde_json::Value = runtime.read_global_json("cbStatIsFile").await.unwrap();
        assert_eq!(is_file, true);

        let mkdir_err: serde_json::Value = runtime.read_global_json("mkdirErr").await.unwrap();
        assert!(mkdir_err.is_null());

        let readdir_empty: serde_json::Value =
            runtime.read_global_json("readdirEmpty").await.unwrap();
        assert_eq!(readdir_empty, true);

        let unlink_err: serde_json::Value = runtime.read_global_json("unlinkErr").await.unwrap();
        assert!(unlink_err.is_null());

        let gone: serde_json::Value = runtime.read_global_json("unlinkGone").await.unwrap();
        assert_eq!(gone, true);
    });
}

// ---------------------------------------------------------------------------
// callback-based lstat / rmdir / rm / rename / copyFile / appendFile
// ---------------------------------------------------------------------------

#[test]
fn fs_callback_lstat_rmdir_rm() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFileSync('/lf.txt', 'data');
                    fs.lstat('/lf.txt', (err, s) => {
                        globalThis.lstatIsFile = s.isFile();
                    });

                    fs.mkdirSync('/rmdir_test');
                    fs.rmdir('/rmdir_test', (err) => {
                        globalThis.rmdirErr = err;
                    });

                    fs.mkdirSync('/rm_test/sub', { recursive: true });
                    fs.writeFileSync('/rm_test/sub/f.txt', 'x');
                    fs.rm('/rm_test', { recursive: true }, (err) => {
                        globalThis.rmErr = err;
                        globalThis.rmGone = !fs.existsSync('/rm_test');
                    });
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let lstat: serde_json::Value = runtime.read_global_json("lstatIsFile").await.unwrap();
        assert_eq!(lstat, true);

        let rmdir_err: serde_json::Value = runtime.read_global_json("rmdirErr").await.unwrap();
        assert!(rmdir_err.is_null());

        let rm_err: serde_json::Value = runtime.read_global_json("rmErr").await.unwrap();
        assert!(rm_err.is_null());

        let rm_gone: serde_json::Value = runtime.read_global_json("rmGone").await.unwrap();
        assert_eq!(rm_gone, true);
    });
}

// ---------------------------------------------------------------------------
// callback rename / copyFile / appendFile / access / chmod / chown / realpath
// ---------------------------------------------------------------------------

#[test]
fn fs_callback_rename_copy_append() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFileSync('/ren_src.txt', 'hello');

                    fs.rename('/ren_src.txt', '/ren_dst.txt', (err) => {
                        globalThis.renameErr = err;
                        globalThis.renDst = fs.readFileSync('/ren_dst.txt', 'utf8');
                    });

                    fs.writeFileSync('/cp_src.txt', 'copy me');
                    fs.copyFile('/cp_src.txt', '/cp_dst.txt', (err) => {
                        globalThis.copyErr = err;
                        globalThis.cpDst = fs.readFileSync('/cp_dst.txt', 'utf8');
                    });

                    fs.writeFileSync('/app.txt', 'a');
                    fs.appendFile('/app.txt', 'b', (err) => {
                        globalThis.appendErr = err;
                        globalThis.appContent = fs.readFileSync('/app.txt', 'utf8');
                    });

                    fs.writeFileSync('/acc2.txt', 'x');
                    fs.access('/acc2.txt', (err) => {
                        globalThis.accessErr = err;
                    });

                    fs.chmod('/acc2.txt', 0o644, (err) => {
                        globalThis.chmodErr = err;
                    });

                    fs.chown('/acc2.txt', 0, 0, (err) => {
                        globalThis.chownErr = err;
                    });

                    fs.realpath('/acc2.txt', (err, resolved) => {
                        globalThis.realpathResult = resolved;
                    });
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let rename_err: serde_json::Value = runtime.read_global_json("renameErr").await.unwrap();
        assert!(rename_err.is_null());
        let ren_dst: serde_json::Value = runtime.read_global_json("renDst").await.unwrap();
        assert_eq!(ren_dst, "hello");

        let copy_err: serde_json::Value = runtime.read_global_json("copyErr").await.unwrap();
        assert!(copy_err.is_null());
        let cp_dst: serde_json::Value = runtime.read_global_json("cpDst").await.unwrap();
        assert_eq!(cp_dst, "copy me");

        let append_err: serde_json::Value = runtime.read_global_json("appendErr").await.unwrap();
        assert!(append_err.is_null());
        let app: serde_json::Value = runtime.read_global_json("appContent").await.unwrap();
        assert_eq!(app, "ab");

        let access_err: serde_json::Value = runtime.read_global_json("accessErr").await.unwrap();
        assert!(access_err.is_null());

        let chmod_err: serde_json::Value = runtime.read_global_json("chmodErr").await.unwrap();
        assert!(chmod_err.is_null());

        let chown_err: serde_json::Value = runtime.read_global_json("chownErr").await.unwrap();
        assert!(chown_err.is_null());

        let realpath: serde_json::Value = runtime.read_global_json("realpathResult").await.unwrap();
        assert_eq!(realpath, "/acc2.txt");
    });
}

// ---------------------------------------------------------------------------
// constants
// ---------------------------------------------------------------------------

#[test]
fn fs_constants() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    globalThis.consts = {
                        R_OK: fs.constants.R_OK,
                        W_OK: fs.constants.W_OK,
                        X_OK: fs.constants.X_OK,
                        F_OK: fs.constants.F_OK,
                    };
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let c: serde_json::Value = runtime.read_global_json("consts").await.unwrap();
        assert_eq!(c["R_OK"], 4);
        assert_eq!(c["W_OK"], 2);
        assert_eq!(c["X_OK"], 1);
        assert_eq!(c["F_OK"], 0);
    });
}

// ---------------------------------------------------------------------------
// mkdtempSync
// ---------------------------------------------------------------------------

#[test]
fn fs_mkdtemp() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    const dir = fs.mkdtempSync('/tmp/test-');
                    globalThis.mkdtempResult = dir;
                    globalThis.mkdtempIsDir = fs.statSync(dir).isDirectory();
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let dir: serde_json::Value = runtime.read_global_json("mkdtempResult").await.unwrap();
        assert!(dir.as_str().unwrap().starts_with("/tmp/test-"));

        let is_dir: serde_json::Value = runtime.read_global_json("mkdtempIsDir").await.unwrap();
        assert_eq!(is_dir, true);
    });
}

// ---------------------------------------------------------------------------
// ENOENT errors
// ---------------------------------------------------------------------------

#[test]
fn fs_enoent_errors() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    globalThis.errors = {};
                    try { fs.readFileSync('/nope'); }
                    catch (e) { globalThis.errors.read = e.message; }

                    try { fs.statSync('/nope'); }
                    catch (e) { globalThis.errors.stat = e.message; }

                    try { fs.unlinkSync('/nope'); }
                    catch (e) { globalThis.errors.unlink = e.message; }

                    try { fs.rmdirSync('/nope'); }
                    catch (e) { globalThis.errors.rmdir = e.message; }

                    try { fs.readdirSync('/nope'); }
                    catch (e) { globalThis.errors.readdir = e.message; }

                    try { fs.renameSync('/nope', '/x'); }
                    catch (e) { globalThis.errors.rename = e.message; }
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let errs: serde_json::Value = runtime.read_global_json("errors").await.unwrap();
        assert!(errs["read"].as_str().unwrap().contains("ENOENT"));
        assert!(errs["stat"].as_str().unwrap().contains("ENOENT"));
        assert!(errs["unlink"].as_str().unwrap().contains("ENOENT"));
        assert!(errs["rmdir"].as_str().unwrap().contains("ENOENT"));
        assert!(errs["readdir"].as_str().unwrap().contains("ENOENT"));
        assert!(errs["rename"].as_str().unwrap().contains("ENOENT"));
    });
}

// ---------------------------------------------------------------------------
// path normalization (relative, .., etc.)
// ---------------------------------------------------------------------------

#[test]
fn fs_path_normalization() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.mkdirSync('/a/b', { recursive: true });
                    fs.writeFileSync('/a/b/f.txt', 'data');

                    // Read with .. path
                    globalThis.dotdot = fs.readFileSync('/a/b/../b/f.txt', 'utf8');

                    // realpath normalizes
                    globalThis.realp = fs.realpathSync('/a/b/../b/./f.txt');
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let dotdot: serde_json::Value = runtime.read_global_json("dotdot").await.unwrap();
        assert_eq!(dotdot, "data");

        let realp: serde_json::Value = runtime.read_global_json("realp").await.unwrap();
        assert_eq!(realp, "/a/b/f.txt");
    });
}

// ---------------------------------------------------------------------------
// createReadStream / createWriteStream stubs
// ---------------------------------------------------------------------------

#[test]
fn fs_stream_stubs() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    const rs = fs.createReadStream('/test');
                    globalThis.hasReadOn = typeof rs.on === 'function';
                    globalThis.hasReadPipe = typeof rs.pipe === 'function';

                    const ws = fs.createWriteStream('/test');
                    globalThis.hasWriteOn = typeof ws.on === 'function';
                    globalThis.hasWriteWrite = typeof ws.write === 'function';
                    globalThis.hasWriteEnd = typeof ws.end === 'function';
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let read_on: serde_json::Value = runtime.read_global_json("hasReadOn").await.unwrap();
        assert_eq!(read_on, true);
        let read_pipe: serde_json::Value = runtime.read_global_json("hasReadPipe").await.unwrap();
        assert_eq!(read_pipe, true);
        let write_on: serde_json::Value = runtime.read_global_json("hasWriteOn").await.unwrap();
        assert_eq!(write_on, true);
        let write_write: serde_json::Value =
            runtime.read_global_json("hasWriteWrite").await.unwrap();
        assert_eq!(write_write, true);
        let write_end: serde_json::Value = runtime.read_global_json("hasWriteEnd").await.unwrap();
        assert_eq!(write_end, true);
    });
}

// ---------------------------------------------------------------------------
// watch / watchFile stubs
// ---------------------------------------------------------------------------

#[test]
fn fs_watch_stubs() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    const w = fs.watch('/test');
                    globalThis.hasClose = typeof w.close === 'function';
                    globalThis.hasUnref = typeof w.unref === 'function';

                    const wf = fs.watchFile('/test', () => {});
                    globalThis.wfHasClose = typeof wf.close === 'function';

                    // unwatchFile should not throw
                    fs.unwatchFile('/test');
                    globalThis.unwatchOk = true;
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let close: serde_json::Value = runtime.read_global_json("hasClose").await.unwrap();
        assert_eq!(close, true);
        let unref: serde_json::Value = runtime.read_global_json("hasUnref").await.unwrap();
        assert_eq!(unref, true);
        let unwatch: serde_json::Value = runtime.read_global_json("unwatchOk").await.unwrap();
        assert_eq!(unwatch, true);
    });
}

// ---------------------------------------------------------------------------
// fd-based operations (openSync, readSync, writeSync, fstatSync)
// ---------------------------------------------------------------------------

#[test]
fn fs_fd_stubs() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    fs.writeFileSync('/test', 'abcdef');
                    const readFd = fs.openSync('/test', 'r');
                    globalThis.fdVal = typeof readFd === 'number';

                    const buf = new Uint8Array(3);
                    globalThis.readBytes = fs.readSync(readFd, buf, 0, 3, 0);
                    globalThis.readText = Buffer.from(buf).toString('utf8');

                    const fstat = fs.fstatSync(readFd);
                    globalThis.fstatHasIsFile = typeof fstat.isFile === 'function';
                    globalThis.fstatSize = fstat.size;

                    fs.closeSync(readFd);
                    globalThis.closeOk = true;

                    const appendFd = fs.openSync('/test', 'a');
                    globalThis.writeBytes = fs.writeSync(appendFd, '!');
                    fs.closeSync(appendFd);
                    globalThis.afterWrite = fs.readFileSync('/test', 'utf8');

                    const lockFd = fs.openSync('/lock', fs.constants.O_CREAT | fs.constants.O_EXCL | fs.constants.O_RDWR);
                    fs.closeSync(lockFd);
                    try {
                        fs.openSync('/lock', fs.constants.O_CREAT | fs.constants.O_EXCL | fs.constants.O_RDWR);
                        globalThis.exclusiveCreateFailed = false;
                    } catch (e) {
                        globalThis.exclusiveCreateFailed = String(e && e.message || e).includes('EEXIST');
                    }
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let fd_val: serde_json::Value = runtime.read_global_json("fdVal").await.unwrap();
        assert_eq!(fd_val, true);
        let close_ok: serde_json::Value = runtime.read_global_json("closeOk").await.unwrap();
        assert_eq!(close_ok, true);
        let read_bytes: serde_json::Value = runtime.read_global_json("readBytes").await.unwrap();
        assert_eq!(read_bytes, 3);
        let read_text: serde_json::Value = runtime.read_global_json("readText").await.unwrap();
        assert_eq!(read_text, "abc");
        let fstat: serde_json::Value = runtime.read_global_json("fstatHasIsFile").await.unwrap();
        assert_eq!(fstat, true);
        let fstat_size: serde_json::Value = runtime.read_global_json("fstatSize").await.unwrap();
        assert_eq!(fstat_size, 6);
        let write_bytes: serde_json::Value = runtime.read_global_json("writeBytes").await.unwrap();
        assert_eq!(write_bytes, 1);
        let after_write: serde_json::Value = runtime.read_global_json("afterWrite").await.unwrap();
        assert_eq!(after_write, "abcdef!");
        let exclusive: serde_json::Value = runtime
            .read_global_json("exclusiveCreateFailed")
            .await
            .unwrap();
        assert_eq!(exclusive, true);
    });
}

// ---------------------------------------------------------------------------
// host FS fallback in statSync (the fix we applied)
// ---------------------------------------------------------------------------

#[test]
fn fs_stat_host_fallback() {
    futures::executor::block_on(async {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let host_file = temp_dir.path().join("host-visible.txt");
        std::fs::write(&host_file, "host-fallback").expect("write host file");

        let mut config = default_config();
        config.cwd = temp_dir.path().display().to_string();
        let runtime =
            PiJsRuntime::with_clock_and_config(Arc::new(DeterministicClock::new(0)), config)
                .await
                .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    const insidePath = process.cwd() + '/host-visible.txt';
                    globalThis.hostExists = fs.existsSync(insidePath);
                    if (globalThis.hostExists) {
                        const s = fs.statSync(insidePath);
                        globalThis.hostStatIsFile = s.isFile();
                        globalThis.hostStatSize = s.size;
                    }

                    globalThis.outsideExists = fs.existsSync('/etc/hostname');
                    try {
                        fs.statSync('/etc/hostname');
                        globalThis.outsideStat = 'ok';
                    } catch (e) {
                        globalThis.outsideStat = String(e.message || e);
                    }
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let exists: serde_json::Value = runtime.read_global_json("hostExists").await.unwrap();
        assert_eq!(exists, true);
        let is_file: serde_json::Value = runtime.read_global_json("hostStatIsFile").await.unwrap();
        assert_eq!(is_file, true);
        let size: serde_json::Value = runtime.read_global_json("hostStatSize").await.unwrap();
        assert!(size.as_u64().unwrap() > 0);

        let outside_exists: serde_json::Value =
            runtime.read_global_json("outsideExists").await.unwrap();
        assert_eq!(outside_exists, false);
        let outside_stat: serde_json::Value =
            runtime.read_global_json("outsideStat").await.unwrap();
        let outside_message = outside_stat.as_str().unwrap_or_default();
        assert!(
            outside_message.contains("outside extension root"),
            "outside stat should be denied, got: {outside_message}"
        );
    });
}

// ---------------------------------------------------------------------------
// promises.appendFile
// ---------------------------------------------------------------------------

#[test]
fn fs_promises_append_file() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then(async (fs) => {
                    await fs.promises.writeFile('/papp.txt', 'start');
                    await fs.promises.appendFile('/papp.txt', '-end');
                    globalThis.pappContent = await fs.promises.readFile('/papp.txt', 'utf8');
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let content: serde_json::Value = runtime.read_global_json("pappContent").await.unwrap();
        assert_eq!(content, "start-end");
    });
}

// ---------------------------------------------------------------------------
// default export includes all functions
// ---------------------------------------------------------------------------

#[test]
fn fs_default_export_complete() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock_and_config(
            Arc::new(DeterministicClock::new(0)),
            default_config(),
        )
        .await
        .expect("create runtime");

        runtime
            .eval(
                r#"
                import('node:fs').then((fs) => {
                    const expected = [
                        'constants', 'existsSync', 'readFileSync', 'writeFileSync',
                        'appendFileSync', 'readdirSync', 'statSync', 'lstatSync',
                        'mkdtempSync', 'realpathSync', 'unlinkSync', 'rmdirSync',
                        'rmSync', 'copyFileSync', 'renameSync', 'mkdirSync',
                        'accessSync', 'chmodSync', 'chownSync', 'readlinkSync', 'symlinkSync',
                        'openSync', 'closeSync',
                        'createReadStream', 'createWriteStream',
                        'readFile', 'writeFile', 'stat', 'lstat', 'readdir',
                        'mkdir', 'unlink', 'readlink', 'symlink', 'rmdir', 'rm', 'rename', 'copyFile',
                        'appendFile', 'access', 'realpath', 'promises',
                    ];
                    globalThis.missingExports = expected.filter(
                        k => typeof fs.default[k] === 'undefined'
                    );
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let missing: serde_json::Value = runtime.read_global_json("missingExports").await.unwrap();
        let arr = missing.as_array().unwrap();
        assert!(
            arr.is_empty(),
            "Missing from default export: {:?}",
            arr.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>()
        );
    });
}
