//! Unit tests for the node:fs shim (bd-1av0.1).
//!
//! Tests verify that the virtual filesystem (VFS) and `node:fs` module follow
//! Node.js semantics: `readFileSync`/`writeFileSync`, `existsSync`, `statSync`,
//! `readdirSync`, `mkdirSync`, `unlinkSync`, `rmdirSync`, `rmSync`,
//! `copyFileSync`, `renameSync`, `appendFileSync`, `accessSync`, callback-based
//! async functions, and the `promises` namespace.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use std::sync::Arc;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn load_ext(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/fs_test.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec");

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    manager
}

fn fs_ext_source(js_expr: &str) -> String {
    format!(
        r#"
import fs from "node:fs";

export default function activate(pi) {{
  pi.on("agent_start", (event, ctx) => {{
    let result;
    try {{
      result = String({js_expr});
    }} catch (e) {{
      result = "ERROR:" + e.message;
    }}
    return {{ result }};
  }});
}}
"#
    )
}

fn eval_fs(js_expr: &str) -> String {
    let harness = common::TestHarness::new("fs_shim");
    let source = fs_ext_source(js_expr);
    let mgr = load_ext(&harness, &source);

    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch agent_start")
    });

    response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "NO_RESPONSE".to_string())
}

// ─── writeFileSync + readFileSync roundtrip ─────────────────────────────────

#[test]
fn write_read_roundtrip_utf8() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/test.txt", "hello world");
        return fs.readFileSync("/tmp/test.txt", "utf8");
    })()"#,
    );
    assert_eq!(result, "hello world");
}

#[test]
fn write_read_roundtrip_buffer() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/buf.bin", "binary data");
        const buf = fs.readFileSync("/tmp/buf.bin");
        return typeof buf === "object" && buf.length > 0;
    })()"#,
    );
    assert_eq!(result, "true");
}

// ─── existsSync ─────────────────────────────────────────────────────────────

#[test]
fn exists_sync_false_for_missing() {
    let result = eval_fs(r#"fs.existsSync("/nonexistent/file.txt")"#);
    assert_eq!(result, "false");
}

#[test]
fn exists_sync_true_after_write() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/exists.txt", "data");
        return fs.existsSync("/tmp/exists.txt");
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn exists_sync_true_for_directory() {
    let result = eval_fs(
        r#"(() => {
        fs.mkdirSync("/tmp/mydir");
        return fs.existsSync("/tmp/mydir");
    })()"#,
    );
    assert_eq!(result, "true");
}

// ─── statSync ───────────────────────────────────────────────────────────────

#[test]
fn stat_sync_file() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/stat.txt", "hello");
        const s = fs.statSync("/tmp/stat.txt");
        return s.isFile() + ":" + s.isDirectory() + ":" + s.size;
    })()"#,
    );
    assert_eq!(result, "true:false:5");
}

#[test]
fn stat_sync_directory() {
    let result = eval_fs(
        r#"(() => {
        fs.mkdirSync("/tmp/statdir");
        const s = fs.statSync("/tmp/statdir");
        return s.isFile() + ":" + s.isDirectory();
    })()"#,
    );
    assert_eq!(result, "false:true");
}

#[test]
fn stat_sync_throws_on_missing() {
    let result = eval_fs(r#"fs.statSync("/no/such/path")"#);
    assert!(
        result.starts_with("ERROR:"),
        "expected error, got: {result}"
    );
    assert!(result.contains("ENOENT"), "expected ENOENT, got: {result}");
}

// ─── mkdirSync ──────────────────────────────────────────────────────────────

#[test]
fn mkdir_sync_creates_directory() {
    let result = eval_fs(
        r#"(() => {
        fs.mkdirSync("/tmp/newdir");
        return fs.statSync("/tmp/newdir").isDirectory();
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn mkdir_sync_recursive() {
    let result = eval_fs(
        r#"(() => {
        fs.mkdirSync("/tmp/a/b/c", { recursive: true });
        return fs.existsSync("/tmp/a/b/c");
    })()"#,
    );
    assert_eq!(result, "true");
}

// ─── readdirSync ────────────────────────────────────────────────────────────

#[test]
fn readdir_sync_lists_files() {
    let result = eval_fs(
        r#"(() => {
        fs.mkdirSync("/tmp/listdir");
        fs.writeFileSync("/tmp/listdir/a.txt", "a");
        fs.writeFileSync("/tmp/listdir/b.txt", "b");
        return JSON.stringify(fs.readdirSync("/tmp/listdir").sort());
    })()"#,
    );
    assert_eq!(result, r#"["a.txt","b.txt"]"#);
}

#[test]
fn readdir_sync_with_file_types() {
    let result = eval_fs(
        r#"(() => {
        fs.mkdirSync("/tmp/typedir");
        fs.writeFileSync("/tmp/typedir/file.txt", "data");
        fs.mkdirSync("/tmp/typedir/subdir");
        const entries = fs.readdirSync("/tmp/typedir", { withFileTypes: true });
        const file = entries.find(e => e.name === "file.txt");
        const dir = entries.find(e => e.name === "subdir");
        return file.isFile() + ":" + dir.isDirectory();
    })()"#,
    );
    assert_eq!(result, "true:true");
}

#[test]
fn readdir_sync_throws_on_missing() {
    let result = eval_fs(r#"fs.readdirSync("/no/such/dir")"#);
    assert!(result.contains("ENOENT"), "expected ENOENT, got: {result}");
}

// ─── unlinkSync ─────────────────────────────────────────────────────────────

#[test]
fn unlink_sync_removes_file() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/del.txt", "data");
        fs.unlinkSync("/tmp/del.txt");
        return fs.existsSync("/tmp/del.txt");
    })()"#,
    );
    assert_eq!(result, "false");
}

#[test]
fn unlink_sync_throws_on_missing() {
    let result = eval_fs(r#"fs.unlinkSync("/no/such/file")"#);
    assert!(result.contains("ENOENT"), "expected ENOENT, got: {result}");
}

// ─── rmSync ─────────────────────────────────────────────────────────────────

#[test]
fn rm_sync_recursive() {
    let result = eval_fs(
        r#"(() => {
        fs.mkdirSync("/tmp/rmdir");
        fs.writeFileSync("/tmp/rmdir/a.txt", "a");
        fs.mkdirSync("/tmp/rmdir/sub");
        fs.writeFileSync("/tmp/rmdir/sub/b.txt", "b");
        fs.rmSync("/tmp/rmdir", { recursive: true });
        return fs.existsSync("/tmp/rmdir");
    })()"#,
    );
    assert_eq!(result, "false");
}

// ─── copyFileSync ───────────────────────────────────────────────────────────

#[test]
fn copy_file_sync() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/orig.txt", "content");
        fs.copyFileSync("/tmp/orig.txt", "/tmp/copy.txt");
        return fs.readFileSync("/tmp/copy.txt", "utf8");
    })()"#,
    );
    assert_eq!(result, "content");
}

// ─── renameSync ─────────────────────────────────────────────────────────────

#[test]
fn rename_sync() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/old.txt", "moved");
        fs.renameSync("/tmp/old.txt", "/tmp/new.txt");
        return fs.readFileSync("/tmp/new.txt", "utf8") + ":" + fs.existsSync("/tmp/old.txt");
    })()"#,
    );
    assert_eq!(result, "moved:false");
}

// ─── appendFileSync ─────────────────────────────────────────────────────────

#[test]
fn append_file_sync() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/append.txt", "hello");
        fs.appendFileSync("/tmp/append.txt", " world");
        return fs.readFileSync("/tmp/append.txt", "utf8");
    })()"#,
    );
    assert_eq!(result, "hello world");
}

// ─── accessSync ─────────────────────────────────────────────────────────────

#[test]
fn access_sync_existing() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/access.txt", "ok");
        try { fs.accessSync("/tmp/access.txt"); return "ok"; }
        catch (_) { return "fail"; }
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn access_sync_missing() {
    let result = eval_fs(
        r#"(() => {
        try { fs.accessSync("/no/such/file"); return "found"; }
        catch (_) { return "not_found"; }
    })()"#,
    );
    assert_eq!(result, "not_found");
}

// ─── readFileSync throws ENOENT ─────────────────────────────────────────────

#[test]
fn read_file_sync_throws_enoent() {
    let result = eval_fs(r#"fs.readFileSync("/no/such/file", "utf8")"#);
    assert!(result.contains("ENOENT"), "expected ENOENT, got: {result}");
}

// ─── mkdtempSync ────────────────────────────────────────────────────────────

#[test]
fn mkdtemp_sync_creates_temp_dir() {
    let result = eval_fs(
        r#"(() => {
        const dir = fs.mkdtempSync("/tmp/prefix-");
        return dir.startsWith("/tmp/prefix-") && fs.statSync(dir).isDirectory();
    })()"#,
    );
    assert_eq!(result, "true");
}

// ─── Callback-based async readFile ──────────────────────────────────────────

#[test]
fn read_file_callback() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/cb.txt", "callback data");
        let got = null;
        fs.readFile("/tmp/cb.txt", "utf8", (err, data) => { got = data; });
        return got;
    })()"#,
    );
    assert_eq!(result, "callback data");
}

// ─── Callback-based async writeFile ─────────────────────────────────────────

#[test]
fn write_file_callback() {
    let result = eval_fs(
        r#"(() => {
        let err = "pending";
        fs.writeFile("/tmp/wcb.txt", "written", (e) => { err = e; });
        return (err === null) + ":" + fs.readFileSync("/tmp/wcb.txt", "utf8");
    })()"#,
    );
    assert_eq!(result, "true:written");
}

// ─── promises.readFile ──────────────────────────────────────────────────────

#[test]
fn promises_read_file() {
    let harness = common::TestHarness::new("fs_promises_read");
    let source = r#"
import fs from "node:fs";

export default function activate(pi) {
  pi.on("agent_start", async (event, ctx) => {
    fs.writeFileSync("/tmp/promise.txt", "async content");
    const data = await fs.promises.readFile("/tmp/promise.txt", "utf8");
    return { result: data };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "async content");
}

// ─── promises.writeFile ─────────────────────────────────────────────────────

#[test]
fn promises_write_file() {
    let harness = common::TestHarness::new("fs_promises_write");
    let source = r#"
import fs from "node:fs";

export default function activate(pi) {
  pi.on("agent_start", async (event, ctx) => {
    await fs.promises.writeFile("/tmp/pw.txt", "promise written");
    return { result: fs.readFileSync("/tmp/pw.txt", "utf8") };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "promise written");
}

// ─── Import styles ──────────────────────────────────────────────────────────

#[test]
fn named_import_works() {
    let harness = common::TestHarness::new("fs_named_import");
    let source = r#"
import { readFileSync, writeFileSync } from "node:fs";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    writeFileSync("/tmp/named.txt", "named import");
    return { result: readFileSync("/tmp/named.txt", "utf8") };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "named import");
}

#[test]
fn bare_fs_import_works() {
    let harness = common::TestHarness::new("fs_bare_import");
    let source = r#"
import fs from "fs";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    fs.writeFileSync("/tmp/bare.txt", "bare import");
    return { result: fs.readFileSync("/tmp/bare.txt", "utf8") };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "bare import");
}

// ─── fs/promises import ────────────────────────────────────────────────────

#[test]
fn fs_promises_import() {
    let harness = common::TestHarness::new("fs_promises_import");
    let source = r#"
import fsp from "node:fs/promises";
import fs from "node:fs";

export default function activate(pi) {
  pi.on("agent_start", async (event, ctx) => {
    fs.writeFileSync("/tmp/fsp.txt", "promises module");
    const data = await fsp.readFile("/tmp/fsp.txt", "utf8");
    return { result: data };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "promises module");
}

// ─── constants ──────────────────────────────────────────────────────────────

#[test]
fn constants_exported() {
    let result = eval_fs(
        r"(() => {
        return fs.constants.R_OK + ':' + fs.constants.W_OK + ':' + fs.constants.F_OK;
    })()",
    );
    assert_eq!(result, "4:2:0");
}

// ─── lstatSync alias ───────────────────────────────────────────────────────

#[test]
fn lstat_sync_works() {
    let result = eval_fs(
        r#"(() => {
        fs.writeFileSync("/tmp/lstat.txt", "data");
        const s = fs.lstatSync("/tmp/lstat.txt");
        return s.isFile();
    })()"#,
    );
    assert_eq!(result, "true");
}

// ─── symlink/readlink coverage ──────────────────────────────────────────────

#[test]
fn symlink_readlink_and_promises_append() {
    let harness = common::TestHarness::new("fs_symlink_readlink");
    let source = r#"
import fs from "node:fs";
import fsp from "node:fs/promises";

export default function activate(pi) {
  pi.on("agent_start", async () => {
    fs.mkdirSync("/tmp/links", { recursive: true });
    fs.writeFileSync("/tmp/links/target.txt", "payload");
    fs.symlinkSync("/tmp/links/target.txt", "/tmp/links/alias.txt");

    const syncReadlink = fs.readlinkSync("/tmp/links/alias.txt");
    const statIsFile = fs.statSync("/tmp/links/alias.txt").isFile();
    const lstatIsSymlink = fs.lstatSync("/tmp/links/alias.txt").isSymbolicLink();

    await fsp.symlink("/tmp/links/target.txt", "/tmp/links/alias2.txt");
    const promiseReadlink = await fsp.readlink("/tmp/links/alias2.txt");
    await fsp.appendFile("/tmp/links/alias2.txt", "-more");
    const appended = await fsp.readFile("/tmp/links/target.txt", "utf8");

    fs.symlinkSync("/tmp/links/missing.txt", "/tmp/links/broken.txt");
    const brokenExists = fs.existsSync("/tmp/links/broken.txt");
    const brokenLstat = fs.lstatSync("/tmp/links/broken.txt").isSymbolicLink();

    const result = [
      syncReadlink,
      String(statIsFile),
      String(lstatIsSymlink),
      promiseReadlink,
      appended,
      String(brokenExists),
      String(brokenLstat),
    ].join("|");
    return { result };
  });
}
"#;

    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10_000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(
        result,
        "/tmp/links/target.txt|true|true|/tmp/links/target.txt|payload-more|false|true"
    );
}
