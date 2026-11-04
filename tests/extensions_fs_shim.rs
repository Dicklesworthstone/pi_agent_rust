//! Unit tests for the node:fs shim (bd-1av0.1).
//!
//! Tests the virtual filesystem, sync/async operations, host FS fallback,
//! stat objects, promises namespace, and callback-based async functions.
#![allow(clippy::needless_raw_string_hashes)]

use std::sync::Arc;

use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::DeterministicClock;

fn default_config() -> PiJsRuntimeConfig {
    PiJsRuntimeConfig {
        cwd: "/test".to_string(),
        ..Default::default()
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
                    const dir = fs.mkdtempSync('tmp/test-');
                    globalThis.mkdtempResult = dir;
                    globalThis.mkdtempIsDir = fs.statSync(dir).isDirectory();
                });
                "#,
            )
            .await
            .expect("eval");

        runtime.drain_microtasks().await.expect("drain");

        let dir: serde_json::Value = runtime.read_global_json("mkdtempResult").await.unwrap();
        assert!(dir.as_str().unwrap().starts_with("tmp/test-"));

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
        assert!(
            errs["readdir"].as_str().unwrap().contains("ENOENT"),
            "unexpected fs shim errors: {errs}"
        );
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
// createReadStream / createWriteStream facades
// ---------------------------------------------------------------------------

#[test]
fn fs_stream_facades() {
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
                    fs.writeFileSync('/test', 'data');
                    const w = fs.watch('/test');
                    globalThis.hasClose = typeof w.close === 'function';
                    globalThis.hasUnref = typeof w.unref === 'function';

                    const wf = fs.watchFile('/test', () => {});
                    globalThis.wfHasClose = typeof wf.close === 'function';

                    try {
                        fs.watch('/missing');
                        globalThis.watchMissing = 'no_error';
                    } catch (e) {
                        globalThis.watchMissing = String(e.message).includes('ENOENT');
                    }

                    try {
                        fs.watchFile('/missing', () => {});
                        globalThis.watchFileMissing = 'no_error';
                    } catch (e) {
                        globalThis.watchFileMissing = String(e.message).includes('ENOENT');
                    }

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
        let wf_close: serde_json::Value = runtime.read_global_json("wfHasClose").await.unwrap();
        assert_eq!(wf_close, true);
        let watch_missing: serde_json::Value =
            runtime.read_global_json("watchMissing").await.unwrap();
        assert_eq!(watch_missing, true);
        let watch_file_missing: serde_json::Value =
            runtime.read_global_json("watchFileMissing").await.unwrap();
        assert_eq!(watch_file_missing, true);
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

                    globalThis.outsideExists = fs.existsSync('/usr');
                    try {
                        fs.statSync('/usr');
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
        let expected_size =
            u64::try_from("host-fallback".len()).expect("test fixture size fits in u64");
        let observed_size = size
            .as_u64()
            .or_else(|| {
                let value = size.as_f64()?;
                if !value.is_finite() || value.fract() != 0.0 || value < 0.0 {
                    return None;
                }
                format!("{value:.0}").parse::<u64>().ok()
            })
            .expect("numeric hostStatSize");
        assert_eq!(observed_size, expected_size);

        let outside_exists: serde_json::Value =
            runtime.read_global_json("outsideExists").await.unwrap();
        assert_eq!(outside_exists, false);
        let outside_stat: serde_json::Value =
            runtime.read_global_json("outsideStat").await.unwrap();
        let outside_message = outside_stat.as_str().unwrap_or_default();
        assert!(
            outside_message.contains("outside extension root")
                || outside_message.contains("ENOENT"),
            "outside stat should be denied or not found, got: {outside_message}"
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

async fn persistent_storage_runtime(
    workspace: &std::path::Path,
    storage: &std::path::Path,
    policy: pi::extensions::ExtensionPolicy,
) -> PiJsRuntime<DeterministicClock> {
    let runtime = PiJsRuntime::with_clock_and_config_with_policy_for_extension(
        DeterministicClock::new(0),
        PiJsRuntimeConfig {
            cwd: workspace.display().to_string(),
            disk_cache_dir: None,
            ..Default::default()
        },
        Some(policy),
        "storage.owner".to_string(),
    )
    .await
    .expect("runtime");
    runtime.add_extension_root_with_id(workspace.to_path_buf(), Some("storage.owner"));
    runtime
        .add_persistent_storage_root(storage.to_path_buf(), "storage.owner")
        .expect("host storage grant");
    runtime
}

async fn configuration_file_runtime(
    workspace: &std::path::Path,
    configuration: &std::path::Path,
    policy: pi::extensions::ExtensionPolicy,
) -> PiJsRuntime<DeterministicClock> {
    let runtime = PiJsRuntime::with_clock_and_config_with_policy_for_extension(
        DeterministicClock::new(0),
        PiJsRuntimeConfig {
            cwd: workspace.display().to_string(),
            disk_cache_dir: None,
            ..Default::default()
        },
        Some(policy),
        "configuration.owner".to_string(),
    )
    .await
    .expect("configuration runtime");
    runtime.add_extension_root_with_id(workspace.to_path_buf(), Some("configuration.owner"));
    runtime
        .add_configuration_read_file(configuration.to_path_buf(), "configuration.owner")
        .expect("host configuration grant");
    runtime
}

#[test]
fn fs_configuration_file_preserves_missing_errno_and_reads_current_host_bytes() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let configuration = base.join("home/.pi/agent/extensions/context-engineering/config.json");
        std::fs::create_dir(&workspace).unwrap();
        let runtime =
            configuration_file_runtime(&workspace, &configuration, Default::default()).await;
        let file = serde_json::to_string(&configuration.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then(async (fs) => {{
                const file = {file};
                let sync, promise;
                try {{ fs.readFileSync(file, 'utf-8'); }} catch (error) {{ sync = error.code; }}
                try {{ await fs.promises.readFile(file, 'utf-8'); }} catch (error) {{ promise = error.code; }}
                const callback = await new Promise(resolve => {{
                    fs.readFile(file, 'utf-8', error => resolve(error && error.code));
                }});
                globalThis.missingConfiguration = {{ sync, promise, callback, exists: fs.existsSync(file) }};
            }});
        "#)).await.expect("missing configuration");
        runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            runtime
                .read_global_json("missingConfiguration")
                .await
                .unwrap(),
            serde_json::json!({
                "sync": "ENOENT", "promise": "ENOENT", "callback": "ENOENT", "exists": false,
            })
        );
        assert!(
            !base.join("home").exists(),
            "a read grant must not create directories"
        );

        std::fs::create_dir_all(configuration.parent().unwrap()).unwrap();
        for enabled in [false, true] {
            let expected = serde_json::json!({ "context-engineering": { "enabled": enabled } });
            std::fs::write(&configuration, expected.to_string()).unwrap();
            runtime.eval(&format!(r#"
                import('node:fs').then(async (fs) => {{
                    const file = {file};
                    const fd = fs.openSync(file, 'r');
                    const bytes = Buffer.alloc(256);
                    const count = fs.readSync(fd, bytes, 0, bytes.length, 0);
                    fs.closeSync(fd);
                    const callback = await new Promise((resolve, reject) => {{
                        fs.readFile(file, 'utf-8', (error, text) => error ? reject(error) : resolve(text));
                    }});
                    globalThis.currentConfiguration = {{
                        sync: JSON.parse(fs.readFileSync(file, 'utf-8')),
                        promise: JSON.parse(await fs.promises.readFile(file, 'utf-8')),
                        callback: JSON.parse(callback),
                        descriptor: JSON.parse(bytes.subarray(0, count).toString('utf-8')),
                        exists: fs.existsSync(file), regular: fs.statSync(file).isFile(),
                    }};
                }});
            "#)).await.expect("authorized configuration reads");
            runtime.drain_microtasks().await.unwrap();
            assert_eq!(
                runtime
                    .read_global_json("currentConfiguration")
                    .await
                    .unwrap(),
                serde_json::json!({
                    "sync": expected, "promise": expected, "callback": expected,
                    "descriptor": expected, "exists": true, "regular": true,
                })
            );
        }
    });
}

#[test]
fn fs_configuration_grant_denies_mutations_and_neighbouring_home_files() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let configuration = base.join("home/.pi/agent/extensions/context-engineering/config.json");
        let namespace = configuration.parent().unwrap();
        std::fs::create_dir_all(namespace).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(
            &configuration,
            r#"{"context-engineering":{"enabled":false}}"#,
        )
        .unwrap();
        std::fs::write(namespace.join("private.json"), "private configuration").unwrap();
        std::fs::write(namespace.join("config.json.extra"), "prefix neighbour").unwrap();
        std::fs::write(base.join("home/.pi/agent/auth.json"), "private auth").unwrap();
        std::fs::write(base.join("home/private.txt"), "unrelated home file").unwrap();
        std::fs::write(workspace.join("source.txt"), "source").unwrap();
        let runtime =
            configuration_file_runtime(&workspace, &configuration, Default::default()).await;
        let file = serde_json::to_string(&configuration.display().to_string()).unwrap();
        let root = serde_json::to_string(&namespace.display().to_string()).unwrap();
        let home = serde_json::to_string(&base.join("home").display().to_string()).unwrap();
        let work = serde_json::to_string(&workspace.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then(async (fs) => {{
                const file = {file}, root = {root}, work = {work}, home = {home};
                const actions = {{
                    write: () => fs.writeFileSync(file, 'forbidden'),
                    append: () => fs.appendFileSync(file, 'forbidden'),
                    writableDescriptor: () => fs.openSync(file, 'w'),
                    readWriteDescriptor: () => fs.openSync(file, 'r+'),
                    copyInto: () => fs.copyFileSync(work + '/source.txt', file),
                    renameInto: () => fs.renameSync(work + '/source.txt', file),
                    renameOut: () => fs.renameSync(file, work + '/moved.json'),
                    unlink: () => fs.unlinkSync(file),
                    removeDirectory: () => fs.rmSync(root, {{ recursive: true }}),
                    mkdir: () => fs.mkdirSync(root + '/new', {{ recursive: true }}),
                    chmod: () => fs.chmodSync(file, 0o777),
                    chown: () => fs.chownSync(file, 0, 0),
                    utimes: () => fs.utimesSync(file, new Date(), new Date()),
                    writableAccess: () => fs.accessSync(file, fs.constants.W_OK),
                    directMutation: () => __pi_host_persistent_storage_mutate('write', file, '{{"data":"Zm9yYmlkZGVu"}}'),
                    sibling: () => fs.readFileSync(root + '/private.json', 'utf8'),
                    prefixSibling: () => fs.readFileSync(file + '.extra', 'utf8'),
                    listNamespace: () => fs.readdirSync(root),
                    authentication: () => fs.readFileSync(home + '/.pi/agent/auth.json', 'utf8'),
                    unrelatedHome: () => fs.readFileSync(home + '/private.txt', 'utf8'),
                }};
                globalThis.configurationDenials = Object.fromEntries(Object.entries(actions).map(([name, action]) => {{
                    try {{ action(); return [name, false]; }} catch (_) {{ return [name, true]; }}
                }}));
                let promiseDenied = false;
                try {{ await fs.promises.writeFile(file, 'forbidden'); }} catch (_) {{ promiseDenied = true; }}
                globalThis.configurationPromiseWriteDenied = promiseDenied;
                const {{ Database }} = await import('bun:sqlite');
                globalThis.configurationSqliteErrors = [{{}}, {{ readonly: true }}].map(options => {{
                    try {{ const database = new Database(file, options); database.close(); return 'allowed'; }}
                    catch (error) {{ return String(error.message || error); }}
                }});
            }});
        "#)).await.expect("configuration mutation checks");
        runtime.drain_microtasks().await.unwrap();
        let denials = runtime
            .read_global_json("configurationDenials")
            .await
            .unwrap();
        assert_eq!(denials.as_object().unwrap().len(), 20);
        for (operation, denied) in denials.as_object().unwrap() {
            assert_eq!(
                denied.as_bool(),
                Some(true),
                "configuration operation must be denied: {operation}"
            );
        }
        assert_eq!(
            runtime
                .read_global_json("configurationPromiseWriteDenied")
                .await
                .unwrap(),
            true
        );
        let sqlite_errors = runtime
            .read_global_json("configurationSqliteErrors")
            .await
            .unwrap();
        assert_eq!(sqlite_errors.as_array().unwrap().len(), 2);
        for error in sqlite_errors.as_array().unwrap() {
            assert!(
                error
                    .as_str()
                    .unwrap()
                    .contains("SQLite write access denied by the persistent storage grant"),
                "{error}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&configuration).unwrap(),
            r#"{"context-engineering":{"enabled":false}}"#
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("source.txt")).unwrap(),
            "source"
        );
        assert!(!workspace.join("moved.json").exists());
        assert!(!namespace.join("new").exists());
        for sidecar in ["config.json-journal", "config.json-wal", "config.json-shm"] {
            assert!(
                !namespace.join(sidecar).exists(),
                "SQLite must not create a configuration sidecar"
            );
        }
    });
}

#[test]
fn fs_configuration_grant_does_not_bypass_read_policy_or_become_executable() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let configuration = base.join("home/.pi/agent/extensions/context-engineering/config.json");
        let namespace = configuration.parent().unwrap();
        std::fs::create_dir_all(namespace).unwrap();
        std::fs::write(&configuration, "{}").unwrap();
        std::fs::write(
            namespace.join("payload.mjs"),
            "globalThis.configurationCodeExecuted = true;",
        )
        .unwrap();
        // Deliberately place HOME beneath the workspace: ordinary workspace
        // authority must not override the protected configuration namespace.
        let mut policy = pi::extensions::ExtensionPolicy::default();
        policy.deny_caps.push("read".to_string());
        let denied_runtime = configuration_file_runtime(&base, &configuration, policy).await;
        let file = serde_json::to_string(&configuration.display().to_string()).unwrap();
        denied_runtime.eval(&format!(r#"
            import('node:fs').then((fs) => {{
                try {{ fs.readFileSync({file}, 'utf8'); globalThis.configurationReadCode = 'allowed'; }}
                catch (error) {{ globalThis.configurationReadCode = error.code; }}
            }});
        "#)).await.expect("denied read capability");
        denied_runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            denied_runtime
                .read_global_json("configurationReadCode")
                .await
                .unwrap(),
            "EACCES"
        );

        let runtime = configuration_file_runtime(&base, &configuration, Default::default()).await;
        let payload =
            serde_json::to_string(&namespace.join("payload.mjs").display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then(async (fs) => {{
                let readDenied = false, importDenied = false;
                try {{ fs.readFileSync({payload}, 'utf8'); }} catch (_) {{ readDenied = true; }}
                try {{ await import({payload}); }} catch (_) {{ importDenied = true; }}
                globalThis.configurationDataOnly = {{ readDenied, importDenied, executed: !!globalThis.configurationCodeExecuted }};
            }});
        "#)).await.expect("configuration is data-only");
        runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            runtime
                .read_global_json("configurationDataOnly")
                .await
                .unwrap(),
            serde_json::json!({
                "readDenied": true, "importDenied": true, "executed": false,
            })
        );
        assert!(
            runtime
                .add_configuration_read_file(configuration.clone(), "other.owner")
                .is_err()
        );
        assert!(
            runtime
                .add_persistent_storage_root(namespace.to_path_buf(), "configuration.owner")
                .is_err()
        );
        assert!(
            runtime
                .add_persistent_storage_root(base.join("home"), "configuration.owner")
                .is_err()
        );

        let write_runtime = persistent_storage_runtime(&base, namespace, Default::default()).await;
        assert!(
            write_runtime
                .add_configuration_read_file(configuration, "storage.owner")
                .is_err()
        );
    });
}

#[test]
fn fs_configuration_paths_reject_traversal_before_vfs_normalization() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let configuration = base.join("home/.pi/agent/extensions/context-engineering/config.json");
        let namespace = configuration.parent().unwrap();
        std::fs::create_dir_all(namespace.join("nested")).unwrap();
        std::fs::create_dir_all(namespace.parent().unwrap().join("peer")).unwrap();
        std::fs::write(&configuration, "authorized").unwrap();
        std::fs::write(
            namespace.parent().unwrap().join("peer/config.json"),
            "peer secret",
        )
        .unwrap();
        let runtime = configuration_file_runtime(&base, &configuration, Default::default()).await;
        let root = serde_json::to_string(&namespace.display().to_string()).unwrap();
        let work = serde_json::to_string(&base.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then(async (fs) => {{
                const root = {root};
                const paths = [root + '/nested/../config.json', root + '/../peer/config.json', root + '/../context-engineering/config.json'];
                globalThis.configurationTraversalDenied = paths.map(path => {{
                    try {{ fs.readFileSync(path, 'utf8'); return false; }} catch (_) {{ return true; }}
                }});
                fs.symlinkSync(paths[0], {work} + '/configuration-alias');
                let aliasDenied = false;
                try {{ await fs.promises.readFile({work} + '/configuration-alias', 'utf8'); }} catch (_) {{ aliasDenied = true; }}
                globalThis.configurationVirtualLinkDenied = aliasDenied;
                globalThis.configurationStillReadable = fs.readFileSync(root + '/config.json', 'utf8');
            }});
        "#)).await.expect("configuration traversal checks");
        runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            runtime
                .read_global_json("configurationTraversalDenied")
                .await
                .unwrap(),
            serde_json::json!([true, true, true])
        );
        assert_eq!(
            runtime
                .read_global_json("configurationVirtualLinkDenied")
                .await
                .unwrap(),
            true
        );
        assert_eq!(
            runtime
                .read_global_json("configurationStillReadable")
                .await
                .unwrap(),
            "authorized"
        );
    });
}

#[cfg(unix)]
#[test]
fn fs_configuration_grant_rejects_existing_and_late_native_links() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let configuration = base.join("home/.pi/agent/extensions/context-engineering/config.json");
        let namespace = configuration.parent().unwrap();
        let secret = base.join("secret.json");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(namespace).unwrap();
        std::fs::write(&configuration, "authorized").unwrap();
        std::fs::write(&secret, "must not be read").unwrap();
        let runtime =
            configuration_file_runtime(&workspace, &configuration, Default::default()).await;
        std::fs::rename(&configuration, namespace.join("original.json")).unwrap();
        std::os::unix::fs::symlink(&secret, &configuration).unwrap();
        let file = serde_json::to_string(&configuration.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then((fs) => {{
                try {{ fs.readFileSync({file}, 'utf8'); globalThis.configurationLinkDenied = false; }}
                catch (error) {{ globalThis.configurationLinkDenied = error.code; }}
            }});
        "#)).await.unwrap();
        runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            runtime
                .read_global_json("configurationLinkDenied")
                .await
                .unwrap(),
            "EACCES"
        );

        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("config.json"), "outside configuration").unwrap();
        std::fs::rename(namespace, base.join("original-namespace")).unwrap();
        std::os::unix::fs::symlink(&outside, namespace).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then((fs) => {{
                try {{ fs.readFileSync({file}, 'utf8'); globalThis.configurationParentLinkDenied = false; }}
                catch (error) {{ globalThis.configurationParentLinkDenied = error.code; }}
            }});
        "#)).await.unwrap();
        runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            runtime
                .read_global_json("configurationParentLinkDenied")
                .await
                .unwrap(),
            "EACCES"
        );

        let linked_namespace = base.join("linked-namespace");
        std::os::unix::fs::symlink(namespace, &linked_namespace).unwrap();
        assert!(
            runtime
                .add_configuration_read_file(
                    linked_namespace.join("original.json"),
                    "configuration.owner"
                )
                .is_err()
        );
        let hardlink = base.join("hardlinked/config.json");
        std::fs::create_dir(hardlink.parent().unwrap()).unwrap();
        std::fs::hard_link(&secret, &hardlink).unwrap();
        assert!(
            runtime
                .add_configuration_read_file(hardlink, "configuration.owner")
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(secret).unwrap(), "must not be read");
    });
}

#[cfg(any(unix, windows))]
#[test]
fn fs_configuration_grant_rejects_hardlinks_created_after_admission() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let configuration = base.join("home/.pi/agent/extensions/context-engineering/config.json");
        let secret = base.join("private.json");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(&secret, "private configuration").unwrap();
        let runtime =
            configuration_file_runtime(&workspace, &configuration, Default::default()).await;
        std::fs::create_dir_all(configuration.parent().unwrap()).unwrap();
        std::fs::hard_link(&secret, &configuration).unwrap();
        let file = serde_json::to_string(&configuration.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then((fs) => {{
                try {{ fs.readFileSync({file}, 'utf8'); globalThis.configurationHardlinkDenied = false; }}
                catch (error) {{ globalThis.configurationHardlinkDenied = error.code; }}
            }});
        "#)).await.expect("late hardlink rejection");
        runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            runtime
                .read_global_json("configurationHardlinkDenied")
                .await
                .unwrap(),
            "EACCES"
        );
        assert_eq!(
            std::fs::read_to_string(secret).unwrap(),
            "private configuration"
        );
    });
}

#[cfg(windows)]
#[test]
fn fs_configuration_grant_rejects_native_windows_path_aliases() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let configuration = base.join("home/.pi/agent/extensions/context-engineering/config.json");
        std::fs::create_dir_all(configuration.parent().unwrap()).unwrap();
        std::fs::write(
            configuration.parent().unwrap().join("Config.JSON"),
            "authorized",
        )
        .unwrap();
        let runtime = configuration_file_runtime(&base, &configuration, Default::default()).await;
        let file = serde_json::to_string(&configuration.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then((fs) => {{
                const file = {file};
                const native = file.replace(/\//g, '\\');
                const root = native.slice(0, native.lastIndexOf('\\'));
                const paths = [
                    native + ':hidden', native + '.', native + ' ',
                    '\\\\?\\' + native, '\\\\.\\' + native,
                    root + '\\CONIN$', root + '\\CONOUT$', root + '\\NUL',
                    root + '\\COM¹', root + '\\LPT²',
                    root + '\\sub\\..\\config.json',
                ];
                globalThis.configurationWindowsDenied = paths.map(path => {{
                    try {{ fs.readFileSync(path, 'utf8'); return 'allowed'; }} catch (error) {{ return error.code; }}
                }});
                globalThis.configurationWindowsCaseRead = fs.readFileSync(native.toUpperCase(), 'utf8');
            }});
        "#)).await.expect("native Windows configuration path checks");
        runtime.drain_microtasks().await.unwrap();
        let results = runtime
            .read_global_json("configurationWindowsDenied")
            .await
            .unwrap();
        assert_eq!(results.as_array().unwrap().len(), 11);
        assert!(
            results
                .as_array()
                .unwrap()
                .iter()
                .all(|code| code.as_str() == Some("EACCES"))
        );
        assert_eq!(
            runtime
                .read_global_json("configurationWindowsCaseRead")
                .await
                .unwrap(),
            "authorized"
        );
        assert_eq!(
            std::fs::read_to_string(configuration).unwrap(),
            "authorized"
        );

        let missing = base.join("latehome/.pi/agent/extensions/context-engineering/config.json");
        let late_runtime = configuration_file_runtime(&base, &missing, Default::default()).await;
        let actual = base.join("LATEHOME/.PI/AGENT/EXTENSIONS/CONTEXT-ENGINEERING/Config.JSON");
        std::fs::create_dir_all(actual.parent().unwrap()).unwrap();
        std::fs::write(&actual, "late configuration").unwrap();
        let requested = serde_json::to_string(&missing.display().to_string()).unwrap();
        late_runtime
            .eval(&format!(
                r#"
            import('node:fs').then((fs) => {{
                globalThis.lateWindowsConfiguration = fs.readFileSync({requested}, 'utf8');
            }});
        "#
            ))
            .await
            .expect("late Windows case spelling");
        late_runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            late_runtime
                .read_global_json("lateWindowsConfiguration")
                .await
                .unwrap(),
            "late configuration"
        );
    });
}

#[cfg(windows)]
#[test]
fn fs_configuration_grant_rejects_native_windows_junction_replacement() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let configuration = base.join("home/.pi/agent/extensions/context-engineering/config.json");
        let namespace = configuration.parent().unwrap();
        let outside = base.join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(namespace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(&configuration, "authorized").unwrap();
        std::fs::write(outside.join("config.json"), "must not be read").unwrap();
        let runtime =
            configuration_file_runtime(&workspace, &configuration, Default::default()).await;
        std::fs::rename(namespace, base.join("original-namespace")).unwrap();
        // Native junctions do not require the symlink privilege; fail the test
        // if the Windows filesystem cannot create the malicious fixture.
        let created = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(namespace)
            .arg(&outside)
            .output()
            .expect("create native Windows junction");
        assert!(
            created.status.success(),
            "junction creation failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        let file = serde_json::to_string(&configuration.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then((fs) => {{
                try {{ fs.readFileSync({file}, 'utf8'); globalThis.configurationJunctionDenied = false; }}
                catch (error) {{ globalThis.configurationJunctionDenied = error.code; }}
            }});
        "#)).await.expect("Windows junction rejection");
        runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            runtime
                .read_global_json("configurationJunctionDenied")
                .await
                .unwrap(),
            "EACCES"
        );
        assert!(
            runtime
                .add_configuration_read_file(configuration.clone(), "configuration.owner")
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("config.json")).unwrap(),
            "must not be read"
        );
    });
}

#[test]
fn fs_persistent_namespace_survives_cold_runtime_restart() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let storage = base.join("home/.pi/context-mode");
        let dir = serde_json::to_string(&storage.join("sessions").display().to_string()).unwrap();
        let runtime = persistent_storage_runtime(&workspace, &storage, Default::default()).await;
        runtime
            .eval(&format!(
                r#"
            import('node:fs').then((fs) => {{
                const dir = {dir};
                fs.mkdirSync(dir, {{ recursive: true }});
                fs.writeFileSync(dir + '/session.json', '{{"turn":1}}');
                fs.appendFileSync(dir + '/session.json', '\n');
                fs.renameSync(dir + '/session.json', dir + '/saved.json');
                fs.copyFileSync(dir + '/saved.json', dir + '/copy.json');
                fs.unlinkSync(dir + '/copy.json');
                globalThis.persisted = {{
                    text: fs.readFileSync(dir + '/saved.json', 'utf8'),
                    names: fs.readdirSync(dir),
                    size: fs.statSync(dir + '/saved.json').size,
                }};
            }});
        "#
            ))
            .await
            .expect("persistent fs initialization");
        runtime.drain_microtasks().await.expect("drain");
        let result = runtime.read_global_json("persisted").await.unwrap();
        assert_eq!(result["text"], "{\"turn\":1}\n");
        assert_eq!(result["names"], serde_json::json!(["saved.json"]));
        assert_eq!(
            std::fs::read_to_string(storage.join("sessions/saved.json")).unwrap(),
            "{\"turn\":1}\n"
        );
        drop(runtime);

        let runtime = persistent_storage_runtime(&workspace, &storage, Default::default()).await;
        runtime
            .eval(&format!(
                r#"
            import('node:fs').then(async (fs) => {{
                globalThis.reloaded = await fs.promises.readFile({dir} + '/saved.json', 'utf8');
            }});
        "#
            ))
            .await
            .expect("cold reload");
        runtime.drain_microtasks().await.expect("drain");
        assert_eq!(
            runtime.read_global_json("reloaded").await.unwrap(),
            "{\"turn\":1}\n"
        );
    });
}

#[test]
fn fs_persistent_inputs_are_checked_before_normalization() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let storage = base.join("home/.pi/context-mode");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(storage.join("sessions")).unwrap();
        std::fs::write(storage.join("value.txt"), "unchanged").unwrap();
        let runtime = persistent_storage_runtime(&workspace, &storage, Default::default()).await;
        let root = serde_json::to_string(&storage.display().to_string()).unwrap();
        let workspace = serde_json::to_string(&workspace.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            Promise.all([import('node:fs'), import('node:path')]).then(async ([fs, path]) => {{
                const root = {root};
                const workspace = {workspace};
                const raw = root + '/sessions/../value.txt';
                const actions = [
                    () => fs.writeFileSync(raw, 'changed'),
                    () => fs.readFileSync(raw, 'utf8'),
                    () => fs.statSync(raw),
                    () => fs.openSync(raw, 'r'),
                    () => fs.mkdirSync(root + '/sessions/../new', {{ recursive: true }}),
                    () => fs.renameSync(root + '/value.txt', root + '/sessions/../moved.txt'),
                    () => fs.unlinkSync(raw),
                    () => fs.realpathSync.native(raw),
                ];
                if (process.platform === 'win32') {{
                    actions.push(() => fs.writeFileSync(root + '/file:stream/../value.txt', 'changed'));
                    actions.push(() => fs.writeFileSync(root + '/directory. /../value.txt', 'changed'));
                }}
                const denied = actions.map((action) => {{ try {{ action(); return false; }} catch (_) {{ return true; }} }});
                let promiseDenied = false;
                try {{ await fs.promises.readFile(raw, 'utf8'); }} catch (_) {{ promiseDenied = true; }}
                let callbackDenied = false;
                fs.readFile(raw, 'utf8', (error) => {{ callbackDenied = !!error; }});

                fs.mkdirSync(workspace, {{ recursive: true }});
                fs.symlinkSync(raw, workspace + '/virtual-alias');
                let virtualAliasDenied = false;
                try {{ fs.readFileSync(workspace + '/virtual-alias', 'utf8'); }} catch (_) {{ virtualAliasDenied = true; }}
                // Ordinary overlay paths preserve their established lexical
                // normalization. A caller may also pass an already normalized
                // safe storage pathname produced by node:path.
                fs.writeFileSync(workspace + '/ordinary/../virtual.txt', 'overlay');
                globalThis.inputChecks = {{
                    allDenied: denied.every(Boolean), count: denied.length,
                    promiseDenied, callbackDenied, virtualAliasDenied,
                    unchanged: fs.readFileSync(path.join(root, 'sessions', '..', 'value.txt'), 'utf8'),
                    overlay: fs.readFileSync(workspace + '/virtual.txt', 'utf8'),
                }};
            }});
        "#)).await.expect("input path checks");
        runtime.drain_microtasks().await.expect("drain");
        assert_eq!(
            runtime.read_global_json("inputChecks").await.unwrap(),
            serde_json::json!({
                "allDenied": true, "count": if cfg!(windows) { 10 } else { 8 },
                "promiseDenied": true, "callbackDenied": true, "virtualAliasDenied": true,
                "unchanged": "unchanged", "overlay": "overlay",
            })
        );
        assert_eq!(
            std::fs::read_to_string(storage.join("value.txt")).unwrap(),
            "unchanged"
        );
        assert!(!storage.join("new").exists());
        assert!(!storage.join("moved.txt").exists());
    });
}

#[test]
fn fs_persistent_mkdtemp_creates_unique_host_directories() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let storage = base.join("home/.pi/context-mode");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(&storage).unwrap();
        let runtime = persistent_storage_runtime(&workspace, &storage, Default::default()).await;
        let root = serde_json::to_string(&storage.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then(async (fs) => {{
                const root = {root};
                const directories = [
                    fs.mkdtempSync(root + '/cache-'),
                    await fs.promises.mkdtemp(root + '/cache-'),
                    fs.mkdtempSync(root + '/', {{ encoding: 'buffer' }}).toString('utf8'),
                    fs.mkdtempSync(root + '/.'),
                ];
                for (const directory of directories) fs.writeFileSync(directory + '/state.txt', 'persistent');
                let siblingDenied = false;
                try {{ fs.mkdtempSync(root); }} catch (_) {{ siblingDenied = true; }}
                let nativeSiblingDenied = false;
                try {{ __pi_host_persistent_storage_mkdtemp(root); }} catch (_) {{ nativeSiblingDenied = true; }}
                globalThis.persistentTemp = {{ directories, siblingDenied, nativeSiblingDenied }};
            }});
        "#)).await.expect("persistent temporary directories");
        runtime.drain_microtasks().await.expect("drain");
        let result = runtime.read_global_json("persistentTemp").await.unwrap();
        assert_eq!(result["siblingDenied"], true);
        assert_eq!(result["nativeSiblingDenied"], true);
        let directories = result["directories"].as_array().unwrap();
        let unique: std::collections::BTreeSet<_> = directories
            .iter()
            .map(|path| path.as_str().unwrap())
            .collect();
        assert_eq!(unique.len(), 4);
        for path in unique {
            let path = std::path::Path::new(path);
            assert_eq!(path.parent(), Some(storage.as_path()));
            assert_eq!(
                std::fs::read_to_string(path.join("state.txt")).unwrap(),
                "persistent"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
    });
}

#[cfg(unix)]
#[test]
fn fs_realpath_resolves_authorized_project_aliases_and_requires_existing_paths() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let storage = base.join("home/.pi/context-mode");
        let project = workspace.join("real-project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&storage).unwrap();
        std::os::unix::fs::symlink(&project, workspace.join("project-alias")).unwrap();
        std::os::unix::fs::symlink(&project, storage.join("forbidden-alias")).unwrap();
        let outside = base.join("private");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("outside-alias")).unwrap();
        let runtime = persistent_storage_runtime(&workspace, &storage, Default::default()).await;
        let root = serde_json::to_string(&storage.display().to_string()).unwrap();
        let work = serde_json::to_string(&workspace.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then(async (fs) => {{
                const alias = {work} + '/project-alias';
                let callbackPath;
                fs.realpath.native(alias, (error, value) => {{ if (error) throw error; callbackPath = value; }});
                const denied = [{work} + '/missing', {work} + '/outside-alias', {root} + '/forbidden-alias']
                    .map((value) => {{ try {{ fs.realpathSync.native(value); return false; }} catch (_) {{ return true; }} }});
                fs.writeFileSync({work} + '/virtual.txt', 'overlay');
                globalThis.realpathChecks = {{
                    native: fs.realpathSync.native(alias),
                    promise: await fs.promises.realpath(alias), callbackPath,
                    buffer: fs.realpathSync(alias, {{ encoding: 'buffer' }}).toString('utf8'),
                    denied, virtual: fs.realpathSync({work} + '/virtual.txt'),
                }};
            }});
        "#)).await.expect("realpath checks");
        runtime.drain_microtasks().await.expect("drain");
        let expected = project.display().to_string();
        assert_eq!(
            runtime.read_global_json("realpathChecks").await.unwrap(),
            serde_json::json!({
                "native": expected, "promise": expected, "callbackPath": expected, "buffer": expected,
                "denied": [true, true, true], "virtual": workspace.join("virtual.txt").display().to_string(),
            })
        );
    });
}

#[cfg(unix)]
#[test]
fn fs_persistent_descriptor_can_be_released_after_its_path_is_revoked() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let storage = base.join("home/.pi/context-mode");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("value.txt"), "initial").unwrap();
        let secret = base.join("private.txt");
        std::fs::write(&secret, "must not be read").unwrap();
        let runtime = persistent_storage_runtime(&workspace, &storage, Default::default()).await;
        let file = serde_json::to_string(&storage.join("value.txt").display().to_string()).unwrap();
        runtime
            .eval(&format!(
                r#"
            import('node:fs').then((fs) => {{ globalThis.storageFd = fs.openSync({file}, 'r'); }});
        "#
            ))
            .await
            .unwrap();
        runtime.drain_microtasks().await.unwrap();
        std::fs::rename(storage.join("value.txt"), storage.join("original.txt")).unwrap();
        std::os::unix::fs::symlink(&secret, storage.join("value.txt")).unwrap();
        runtime.eval(r#"
            import('node:fs').then((fs) => {
                let denied = false;
                try { fs.readSync(storageFd, Buffer.alloc(32), 0, 32, 0); } catch (_) { denied = true; }
                let closed = false;
                try { fs.closeSync(storageFd); closed = true; } catch (_) {}
                let stale = false;
                try { fs.readSync(storageFd, Buffer.alloc(32), 0, 32, 0); }
                catch (error) { stale = String(error).includes('EBADF'); }
                globalThis.revokedDescriptor = { denied, closed, stale };
            });
        "#).await.unwrap();
        runtime.drain_microtasks().await.unwrap();
        assert_eq!(
            runtime.read_global_json("revokedDescriptor").await.unwrap(),
            serde_json::json!({
                "denied": true, "closed": true, "stale": true,
            })
        );
        assert_eq!(std::fs::read_to_string(secret).unwrap(), "must not be read");
    });
}

#[test]
fn fs_persistent_namespace_preserves_scope_capabilities_and_module_isolation() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let storage = base.join("home/.pi/context-mode");
        let root = serde_json::to_string(&storage.display().to_string()).unwrap();
        let runtime = persistent_storage_runtime(&workspace, &storage, Default::default()).await;
        runtime.eval(&format!(r#"
            import('node:fs').then(async (fs) => {{
                const root = {root};
                fs.mkdirSync(root, {{ recursive: true }});
                fs.writeFileSync(root + '/payload.mjs', 'globalThis.executedStorage = true;');
                const denied = [];
                for (const path of [root + '/../agent/auth.json', root + '/../peer/data.txt', root + '-peer/file.txt']) {{
                    try {{ fs.writeFileSync(path, 'forbidden'); denied.push(false); }}
                    catch (_) {{ denied.push(true); }}
                }}
                let importDenied = false;
                try {{ await import(root + '/payload.mjs'); }} catch (_) {{ importDenied = true; }}
                globalThis.scopeResult = {{ denied, importDenied, executed: !!globalThis.executedStorage }};
            }});
        "#)).await.expect("scope checks");
        runtime.drain_microtasks().await.expect("drain");
        let result = runtime.read_global_json("scopeResult").await.unwrap();
        assert_eq!(
            result,
            serde_json::json!({"denied": [true, true, true], "importDenied": true, "executed": false})
        );
        assert!(!base.join("home/.pi/agent/auth.json").exists());

        runtime.add_extension_root_with_id(base.clone(), Some("storage.owner"));
        runtime
            .eval(&format!(
                r#"
            import({root} + '/payload.mjs').then(
                () => {{ globalThis.overlappingImportDenied = false; }},
                () => {{ globalThis.overlappingImportDenied = true; }},
            );
        "#
            ))
            .await
            .expect("overlapping executable root check");
        runtime.drain_microtasks().await.expect("drain");
        assert_eq!(
            runtime
                .read_global_json("overlappingImportDenied")
                .await
                .unwrap(),
            true
        );

        let mut policy = pi::extensions::ExtensionPolicy::default();
        policy.deny_caps.push("write".to_string());
        let denied_runtime = persistent_storage_runtime(&workspace, &storage, policy).await;
        denied_runtime.eval(&format!(r#"
            import('node:fs').then((fs) => {{
                try {{ fs.writeFileSync({root} + '/denied.txt', 'forbidden'); globalThis.writeDenied = false; }}
                catch (_) {{ globalThis.writeDenied = true; }}
            }});
        "#)).await.expect("capability check");
        denied_runtime.drain_microtasks().await.expect("drain");
        assert_eq!(
            denied_runtime
                .read_global_json("writeDenied")
                .await
                .unwrap(),
            true
        );
        assert!(!storage.join("denied.txt").exists());
    });
}

#[cfg(unix)]
#[test]
fn fs_persistent_namespace_rejects_existing_and_late_symlink_escapes() {
    futures::executor::block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = pi::extensions::safe_canonicalize(temp.path());
        let workspace = base.join("workspace");
        let storage = base.join("home/.pi/context-mode");
        let outside = base.join("outside");
        for dir in [&workspace, &storage, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(outside.join("secret"), b"unchanged").unwrap();
        let runtime = persistent_storage_runtime(&workspace, &storage, Default::default()).await;
        std::os::unix::fs::symlink(&outside, storage.join("escape")).unwrap();
        std::fs::hard_link(outside.join("secret"), storage.join("hardlink")).unwrap();
        let root = serde_json::to_string(&storage.display().to_string()).unwrap();
        runtime.eval(&format!(r#"
            import('node:fs').then((fs) => {{
                const root = {root};
                globalThis.linkResults = [
                    () => fs.readFileSync(root + '/escape/secret', 'utf8'),
                    () => fs.writeFileSync(root + '/escape/secret', 'changed'),
                    () => fs.mkdirSync(root + '/escape/new/dir', {{ recursive: true }}),
                    () => fs.writeFileSync(root + '/hardlink', 'changed'),
                ].map((action) => {{ try {{ action(); return false; }} catch (_) {{ return true; }} }});
            }});
        "#)).await.expect("link checks");
        runtime.drain_microtasks().await.expect("drain");
        assert_eq!(
            runtime.read_global_json("linkResults").await.unwrap(),
            serde_json::json!([true, true, true, true])
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).unwrap(),
            "unchanged"
        );
        assert!(!outside.join("new").exists());

        let symlink_root = base.join("linked-root");
        std::os::unix::fs::symlink(&outside, &symlink_root).unwrap();
        assert!(
            runtime
                .add_persistent_storage_root(symlink_root, "storage.owner")
                .is_err()
        );
    });
}
