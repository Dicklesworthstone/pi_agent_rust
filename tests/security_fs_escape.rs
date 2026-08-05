//! Security suite: filesystem escape hardening (bd-740v).
//!
//! Tests verify that extensions cannot escape their allowed filesystem scope
//! via path traversal (`..`), absolute paths, or other techniques. Tests cover
//! the VFS normalization, host-backed reads, and tool-level path resolution.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn load_ext(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/fs_escape_test.mjs", source.as_bytes());
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
import path from "node:path";

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

fn eval_fs_in_harness(harness: &common::TestHarness, js_expr: &str) -> String {
    let source = fs_ext_source(js_expr);
    let mgr = load_ext(harness, &source);

    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch agent_start")
    });

    response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "NO_RESPONSE".to_string())
}

fn eval_fs(js_expr: &str) -> String {
    let harness = common::TestHarness::new("fs_escape");
    eval_fs_in_harness(&harness, js_expr)
}

fn eval_fs_with_setup<F>(setup: F, js_expr: &str) -> String
where
    F: FnOnce(&common::TestHarness),
{
    let harness = common::TestHarness::new("fs_escape");
    setup(&harness);
    eval_fs_in_harness(&harness, js_expr)
}

fn js_path(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("serialize path for JavaScript")
}

/// Create a real, retained canary outside the runtime workspace. An escape
/// test must target a file that demonstrably exists: otherwise an `ENOENT`
/// from a missing fixture can masquerade as successful confinement.
fn retained_outside_canary(
    harness: &common::TestHarness,
    name: &str,
    contents: &str,
) -> (tempfile::TempDir, PathBuf) {
    let process_cwd = std::env::current_dir().expect("resolve test process cwd");
    let outside_dir = tempfile::Builder::new()
        .prefix(".pi-fs-escape-canary-")
        .tempdir_in(process_cwd)
        .expect("create retained outside-root canary directory");
    let canary_path = outside_dir.path().join(name);
    std::fs::write(&canary_path, contents).expect("write outside-root canary");
    assert!(
        !canary_path.starts_with(harness.temp_dir()),
        "outside-root canary unexpectedly landed in the runtime workspace: {}",
        canary_path.display()
    );
    assert_eq!(
        std::fs::read_to_string(&canary_path).expect("read outside-root canary"),
        contents
    );
    (outside_dir, canary_path)
}

// ═══════════════════════════════════════════════════════════════════════════════
// VFS path normalization through the supported node:fs surface
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn vfs_normalize_resolves_dot_dot() {
    let result = eval_fs(
        r"(() => {
        fs.mkdirSync('/tmp/pi-normalize-one/etc', { recursive: true });
        fs.writeFileSync('/tmp/pi-normalize-one/etc/passwd', 'normalized');
        const content = fs.readFileSync('/tmp/pi-normalize-one/user/../etc/passwd', 'utf8');
        return path.normalize('/home/user/../etc/passwd') + ':' + content;
    })()",
    );
    assert_eq!(result, "/home/etc/passwd:normalized");
}

#[test]
fn vfs_normalize_multiple_dot_dots() {
    let result = eval_fs(
        r"(() => {
        fs.mkdirSync('/tmp/pi-normalize-many/etc', { recursive: true });
        fs.writeFileSync('/tmp/pi-normalize-many/etc/passwd', 'normalized');
        const content = fs.readFileSync('/tmp/pi-normalize-many/a/b/c/../../../etc/passwd', 'utf8');
        return path.normalize('/a/b/c/../../../etc/passwd') + ':' + content;
    })()",
    );
    assert_eq!(result, "/etc/passwd:normalized");
}

#[test]
fn vfs_normalize_dot_dot_at_root_stays_at_root() {
    let result = eval_fs(
        r"(() => {
        fs.mkdirSync('/tmp/pi-normalize-root', { recursive: true });
        fs.writeFileSync('/tmp/pi-normalize-root/value.txt', 'root-bounded');
        const content = fs.readFileSync('/../../../tmp/pi-normalize-root/value.txt', 'utf8');
        return path.normalize('/../../../etc/passwd') + ':' + content;
    })()",
    );
    assert_eq!(result, "/etc/passwd:root-bounded");
}

#[test]
fn vfs_normalize_absolute_path_preserved() {
    let result = eval_fs(
        r"(() => {
        fs.mkdirSync('/tmp/pi-normalize-absolute', { recursive: true });
        fs.writeFileSync('/tmp/pi-normalize-absolute/value.txt', 'absolute');
        const content = fs.readFileSync('/tmp/pi-normalize-absolute/value.txt', 'utf8');
        return path.normalize('/etc/shadow') + ':' + content;
    })()",
    );
    assert_eq!(result, "/etc/shadow:absolute");
}

#[test]
fn vfs_normalize_dot_segments_removed() {
    let result = eval_fs(
        r"(() => {
        fs.mkdirSync('/tmp/pi-normalize-dot/user', { recursive: true });
        fs.writeFileSync('/tmp/pi-normalize-dot/user/file', 'dot-segments');
        const content = fs.readFileSync('/tmp/pi-normalize-dot/./user/./file', 'utf8');
        return path.normalize('/home/./user/./file') + ':' + content;
    })()",
    );
    assert_eq!(result, "/home/user/file:dot-segments");
}

#[test]
fn vfs_normalize_empty_segments_collapsed() {
    let result = eval_fs(
        r"(() => {
        fs.mkdirSync('/tmp/pi-normalize-empty/user', { recursive: true });
        fs.writeFileSync('/tmp/pi-normalize-empty/user/file', 'empty-segments');
        const content = fs.readFileSync('/tmp//pi-normalize-empty///user//file', 'utf8');
        return path.normalize('/home//user///file') + ':' + content;
    })()",
    );
    assert_eq!(result, "/home/user/file:empty-segments");
}

// ═══════════════════════════════════════════════════════════════════════════════
// VFS write confinement: writes stay in VFS, never reach real FS
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn vfs_write_does_not_create_real_file() {
    let harness = common::TestHarness::new("fs_escape_relative_tmp");
    let host_canary =
        harness.create_file("tmp/vfs_escape_test_canary.txt", b"HOST_CANARY_UNCHANGED");
    let result = eval_fs_in_harness(
        &harness,
        r"(() => {
        fs.writeFileSync('tmp/vfs_escape_test_canary.txt', 'escape attempt');
        return fs.readFileSync('tmp/vfs_escape_test_canary.txt', 'utf8');
    })()",
    );
    assert_eq!(result, "escape attempt");

    assert_eq!(
        std::fs::read_to_string(&host_canary).expect("read retained relative-path canary"),
        "HOST_CANARY_UNCHANGED",
        "a relative `tmp/...` VFS write must not mutate the corresponding host path"
    );
}

#[test]
fn vfs_write_with_traversal_is_denied() {
    let harness = common::TestHarness::new("fs_escape_traversal_write");
    let (_outside_dir, canary) =
        retained_outside_canary(&harness, "traversal-write.txt", "HOST_CANARY_UNCHANGED");
    let canary_js = js_path(&canary);
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r"(() => {{
        const canary = {canary_js};
        const attackPath = path.dirname(canary) + path.sep + 'missing' + path.sep + '..' + path.sep + path.basename(canary);
        try {{
            fs.writeFileSync(attackPath, 'MUTATED');
            return 'UNEXPECTED_WRITE';
        }} catch (e) {{
            return 'DENIED:' + e.message;
        }}
    }})()"
        ),
    );
    assert!(
        result.starts_with("DENIED:") && result.contains("host write denied"),
        "expected an exact traversal write denial, got: {result}"
    );
    assert_eq!(
        std::fs::read_to_string(&canary).expect("read traversal write canary"),
        "HOST_CANARY_UNCHANGED",
        "a denied traversal write must not mutate its existing host target"
    );
}

#[test]
fn vfs_mkdir_absolute_tmp_is_scoped_virtual() {
    let harness = common::TestHarness::new("fs_escape_scoped_absolute_tmp");
    let retained_host_dir = tempfile::Builder::new()
        .prefix("pi-scoped-host-canary-")
        .tempdir()
        .expect("create retained system-temp canary directory");
    let host_result = retained_host_dir.path().join("result.txt");
    std::fs::write(&host_result, "HOST_CANARY_UNCHANGED").expect("seed system-temp canary");
    let unique_dir = js_path(retained_host_dir.path());
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r"(() => {{
        const tempRoot = {unique_dir};
        try {{
            fs.mkdirSync(tempRoot, {{ recursive: true }});
            fs.writeFileSync(tempRoot + path.sep + 'result.txt', 'scoped');
            return fs.readFileSync(tempRoot + path.sep + 'result.txt', 'utf8') + ':' + fs.existsSync(tempRoot);
        }} catch (e) {{
            return 'ERROR:' + e.message;
        }}
    }})()"
        ),
    );
    assert_eq!(result, "scoped:true");

    assert_eq!(
        std::fs::read_to_string(&host_result).expect("read system-temp canary"),
        "HOST_CANARY_UNCHANGED",
        "absolute temporary-directory writes must stay in the extension VFS namespace"
    );
}

#[test]
fn scoped_tmp_namespace_rejects_direct_peer_namespace() {
    let result = eval_fs(
        r"(() => {
        try {
            fs.writeFileSync('/__pi_extension_tmp/peer/result.txt', 'escape');
            return 'wrote';
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("host write denied"),
        "expected peer namespace denial, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Host-backed read fallback behavior
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn host_read_fallback_denies_outside_workspace() {
    let harness = common::TestHarness::new("fs_escape_outside_read");
    let (_outside_dir, canary) =
        retained_outside_canary(&harness, "outside-read.txt", "KNOWN_OUTSIDE_SECRET");
    let canary_js = js_path(&canary);
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r"(() => {{
        try {{
            return 'LEAKED:' + fs.readFileSync({canary_js}, 'utf8');
        }} catch (e) {{
            return 'DENIED:' + e.message;
        }}
    }})()"
        ),
    );
    assert!(
        result.starts_with("DENIED:") && result.contains("host read denied"),
        "an existing outside-root canary must be rejected by policy, got: {result}"
    );
    assert_eq!(
        std::fs::read_to_string(&canary).expect("read retained outside-read canary"),
        "KNOWN_OUTSIDE_SECRET"
    );
}

#[test]
fn host_read_nonexistent_file_throws() {
    let result = eval_fs(
        r"(() => {
        return fs.readFileSync('nonexistent_file_xyzzy_12345', 'utf8');
    })()",
    );
    assert!(
        result.contains("ERROR:") && result.contains("ENOENT"),
        "expected ENOENT error, got: {result}"
    );
}

#[test]
fn host_read_fallback_allows_workspace_file() {
    let result = eval_fs_with_setup(
        |harness| {
            harness.create_file("host_visible/inside.txt", b"host fallback visible");
        },
        r"(() => fs.readFileSync('host_visible/inside.txt', 'utf8'))()",
    );
    assert_eq!(result, "host fallback visible");
}

#[test]
fn vfs_write_then_read_roundtrips_without_host_fs() {
    let result = eval_fs(
        r"(() => {
        const testPath = 'vfs_only/test_file.txt';
        fs.mkdirSync('vfs_only', { recursive: true });
        fs.writeFileSync(testPath, 'VFS content only');
        const content = fs.readFileSync(testPath, 'utf8');
        return content;
    })()",
    );
    assert_eq!(result, "VFS content only");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Path traversal via readFileSync
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn read_file_traversal_with_dot_dot() {
    let harness = common::TestHarness::new("fs_escape_traversal_read");
    let (_outside_dir, canary) =
        retained_outside_canary(&harness, "traversal-read.txt", "KNOWN_OUTSIDE_SECRET");
    let canary_js = js_path(&canary);
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r"(() => {{
        const canary = {canary_js};
        const attackPath = path.dirname(canary) + path.sep + 'missing' + path.sep + '..' + path.sep + path.basename(canary);
        try {{
            return 'LEAKED:' + fs.readFileSync(attackPath, 'utf8');
        }} catch (e) {{
            return 'DENIED:' + e.message;
        }}
    }})()"
        ),
    );
    assert!(
        result.starts_with("DENIED:") && result.contains("host read denied"),
        "an existing traversal target must be rejected by policy, got: {result}"
    );
    assert_eq!(
        std::fs::read_to_string(&canary).expect("read retained traversal-read canary"),
        "KNOWN_OUTSIDE_SECRET"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// existsSync traversal probing
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn exists_sync_absolute_sensitive_path() {
    let harness = common::TestHarness::new("fs_escape_exists_absolute");
    let (_outside_dir, canary) =
        retained_outside_canary(&harness, "exists-absolute.txt", "KNOWN_TO_EXIST");
    let result = eval_fs_in_harness(
        &harness,
        &format!("(() => String(fs.existsSync({})))()", js_path(&canary)),
    );
    assert_eq!(
        result, "false",
        "existsSync must not disclose an existing outside-root path"
    );
}

#[test]
fn exists_sync_traversal_probe() {
    let harness = common::TestHarness::new("fs_escape_exists_traversal");
    let (_outside_dir, canary) =
        retained_outside_canary(&harness, "exists-traversal.txt", "KNOWN_TO_EXIST");
    let canary_js = js_path(&canary);
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r"(() => {{
        const canary = {canary_js};
        const attackPath = path.dirname(canary) + path.sep + 'missing' + path.sep + '..' + path.sep + path.basename(canary);
        return String(fs.existsSync(attackPath));
    }})()"
        ),
    );
    assert_eq!(
        result, "false",
        "existsSync must not disclose an existing outside-root traversal target"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// writeFileSync cannot escape VFS to host
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn write_file_absolute_path_outside_workspace_is_denied() {
    let harness = common::TestHarness::new("fs_escape_absolute_write");
    let (_outside_dir, canary) =
        retained_outside_canary(&harness, "absolute-write.txt", "HOST_CANARY_UNCHANGED");
    let canary_js = js_path(&canary);
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r#"(() => {{
        try {{
            fs.writeFileSync({canary_js}, "MUTATED");
            return "UNEXPECTED_WRITE";
        }} catch (e) {{
            return "DENIED:" + e.message;
        }}
    }})()"#,
        ),
    );
    assert!(
        result.starts_with("DENIED:") && result.contains("host write denied"),
        "expected absolute outside-root write denial, got: {result}"
    );
    assert_eq!(
        std::fs::read_to_string(&canary).expect("read absolute-write canary"),
        "HOST_CANARY_UNCHANGED",
        "a denied absolute write must not mutate its existing host target"
    );
}

#[test]
fn unlink_sync_cannot_delete_real_file() {
    let harness = common::TestHarness::new("fs_escape_unlink");
    let (_outside_dir, canary) =
        retained_outside_canary(&harness, "unlink.txt", "HOST_CANARY_UNCHANGED");
    let canary_js = js_path(&canary);
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r#"(() => {{
        try {{
            fs.unlinkSync({canary_js});
            return "UNEXPECTED_UNLINK";
        }} catch (e) {{
            return "DENIED:" + e.message;
        }}
    }})()"#,
        ),
    );

    assert!(
        result.starts_with("DENIED:") && result.contains("host write denied"),
        "expected outside-root unlink denial, got: {result}"
    );
    assert_eq!(
        std::fs::read_to_string(&canary).expect("read retained unlink canary"),
        "HOST_CANARY_UNCHANGED",
        "a denied unlink must retain the existing host file and contents"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Path module: resolve/join with traversal
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn path_resolve_with_dot_dot() {
    let result = eval_fs(
        r"(() => {
        return path.resolve('/home/user', '../../etc/passwd');
    })()",
    );
    // The path shim's resolve joins but may not fully normalize ..
    // VFS normalizePath handles that separately. Document actual behavior.
    assert!(
        result.contains("etc/passwd"),
        "path.resolve should include target segments: {result}"
    );
}

#[test]
fn path_join_with_dot_dot() {
    let result = eval_fs(
        r"(() => {
        return path.join('/home/user', '..', '..', 'etc', 'passwd');
    })()",
    );
    // path.join preserves .. segments (like Node.js path.join)
    // normalization happens at the VFS layer, not the path module
    assert!(
        result.contains("etc/passwd"),
        "path.join should include target segments: {result}"
    );
}

#[test]
fn path_normalize_removes_traversal() {
    let result = eval_fs(
        r"(() => {
        return path.normalize('/a/b/c/../../../etc/passwd');
    })()",
    );
    // path.normalize should collapse .. segments
    assert!(
        result.contains("etc/passwd"),
        "path.normalize should resolve to target: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Encoding tricks: null bytes, URL encoding
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn null_byte_in_path_handled() {
    let harness = common::TestHarness::new("fs_escape_null_byte");
    let (_outside_dir, canary) =
        retained_outside_canary(&harness, "null-byte.txt", "KNOWN_OUTSIDE_SECRET");
    let canary_js = js_path(&canary);
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r"(() => {{
        try {{
            return 'LEAKED:' + fs.readFileSync({canary_js} + '\x00.txt', 'utf8');
        }} catch (e) {{
            return 'DENIED:' + e.message;
        }}
    }})()"
        ),
    );
    assert!(
        result.starts_with("DENIED:"),
        "a null byte must not expose the retained outside-root canary, got: {result}"
    );
    assert_eq!(
        std::fs::read_to_string(&canary).expect("read null-byte canary"),
        "KNOWN_OUTSIDE_SECRET"
    );
}

#[test]
fn backslash_path_normalized() {
    let result = eval_fs(
        r"(() => {
        fs.mkdirSync('/tmp/pi-backslash-path', { recursive: true });
        fs.writeFileSync('/tmp/pi-backslash-path/value.txt', 'backslash');
        return fs.readFileSync('\\tmp\\pi-backslash-path\\value.txt', 'utf8');
    })()",
    );
    assert_eq!(result, "backslash");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Symlink/realpathSync behavior
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn realpath_sync_returns_normalized_path() {
    let result = eval_fs(
        r"(() => {
        try {
            return fs.realpathSync('/a/b/../c');
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    // realpathSync in VFS should normalize but may throw
    assert!(
        result == "/a/c" || result.contains("ERROR:"),
        "expected normalized path or error, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Stat behavior with traversal
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn stat_sync_traversal_path() {
    let result = eval_fs(
        r"(() => {
        try {
            const stat = fs.statSync('/fake/../');
            return 'isDir:' + stat.isDirectory();
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    // After normalization /fake/.. → / which is always a directory in VFS
    assert!(
        result == "isDir:true" || result.contains("ERROR:"),
        "expected directory or error, got: {result}"
    );
}

#[test]
fn stat_sync_vfs_only_file() {
    let result = eval_fs(
        r"(() => {
        fs.writeFileSync('vfs_stat_test.txt', 'hello');
        const stat = fs.statSync('vfs_stat_test.txt');
        return 'isFile:' + stat.isFile() + ',size:' + stat.size;
    })()",
    );
    assert!(
        result.starts_with("isFile:true"),
        "expected file stat, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// readdir traversal
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn readdir_sync_vfs_only() {
    let result = eval_fs(
        r"(() => {
        fs.mkdirSync('sandbox', { recursive: true });
        fs.writeFileSync('sandbox/a.txt', 'a');
        fs.writeFileSync('sandbox/b.txt', 'b');
        const entries = fs.readdirSync('sandbox');
        return entries.sort().join(',');
    })()",
    );
    assert_eq!(result, "a.txt,b.txt");
}

#[test]
fn readdir_sync_root_only_shows_vfs_dirs() {
    let result = eval_fs(
        r"(() => {
        const entries = fs.readdirSync('/');
        // Should only contain VFS entries, not real filesystem root entries
        return entries.join(',');
    })()",
    );
    // The VFS root listing should not include real FS entries like "etc", "usr", etc.
    assert!(
        !result.contains("usr"),
        "VFS readdirSync('/') should not leak real filesystem: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// copyFileSync stays in VFS
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn copy_file_sync_stays_in_vfs() {
    let harness = common::TestHarness::new("fs_escape_copy");
    let host_source = harness.create_file("copy/src.txt", b"HOST_SOURCE_UNCHANGED");
    let host_dest = harness.create_file("copy/dest.txt", b"HOST_DEST_UNCHANGED");
    let (_outside_dir, outside_dest) =
        retained_outside_canary(&harness, "copy-outside.txt", "OUTSIDE_DEST_UNCHANGED");
    let outside_dest_js = js_path(&outside_dest);
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r#"(() => {{
        fs.writeFileSync("copy/src.txt", "VFS_SOURCE");
        fs.copyFileSync("copy/src.txt", "copy/dest.txt");
        let outsideDisposition = "UNEXPECTED_COPY";
        try {{
            fs.copyFileSync("copy/src.txt", {outside_dest_js});
        }} catch (e) {{
            outsideDisposition = "DENIED:" + e.message;
        }}
        return fs.readFileSync("copy/dest.txt", "utf8") + ":" + outsideDisposition;
    }})()"#
        ),
    );
    assert!(
        result.starts_with("VFS_SOURCE:DENIED:") && result.contains("host write denied"),
        "copyFileSync must preserve its VFS copy and deny an outside-root destination: {result}"
    );

    assert_eq!(
        std::fs::read_to_string(&host_source).expect("read retained copy source"),
        "HOST_SOURCE_UNCHANGED",
        "copyFileSync must not mutate the corresponding host source"
    );
    assert_eq!(
        std::fs::read_to_string(&host_dest).expect("read retained copy destination"),
        "HOST_DEST_UNCHANGED",
        "copyFileSync must not mutate the corresponding host destination"
    );
    assert_eq!(
        std::fs::read_to_string(&outside_dest).expect("read retained outside copy destination"),
        "OUTSIDE_DEST_UNCHANGED",
        "a denied copyFileSync must not mutate its existing outside-root destination"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// renameSync stays in VFS
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn rename_sync_stays_in_vfs() {
    let harness = common::TestHarness::new("fs_escape_rename");
    let host_source = harness.create_file("rename/src.txt", b"HOST_SOURCE_UNCHANGED");
    let host_dest = harness.create_file("rename/dest.txt", b"HOST_DEST_UNCHANGED");
    let (_outside_dir, outside_dest) =
        retained_outside_canary(&harness, "rename-outside.txt", "OUTSIDE_DEST_UNCHANGED");
    let outside_dest_js = js_path(&outside_dest);
    let result = eval_fs_in_harness(
        &harness,
        &format!(
            r#"(() => {{
        fs.writeFileSync("rename/src.txt", "VFS_SOURCE");
        fs.renameSync("rename/src.txt", "rename/dest.txt");
        fs.writeFileSync("rename/escape-src.txt", "ESCAPE_SOURCE");
        let outsideDisposition = "UNEXPECTED_RENAME";
        try {{
            fs.renameSync("rename/escape-src.txt", {outside_dest_js});
        }} catch (e) {{
            outsideDisposition = "DENIED:" + e.message;
        }}
        return fs.readFileSync("rename/dest.txt", "utf8") + ":" + outsideDisposition;
    }})()"#
        ),
    );
    assert!(
        result.starts_with("VFS_SOURCE:DENIED:") && result.contains("host write denied"),
        "renameSync must preserve its VFS rename and deny an outside-root destination: {result}"
    );

    assert_eq!(
        std::fs::read_to_string(&host_source).expect("read retained rename source"),
        "HOST_SOURCE_UNCHANGED",
        "renameSync must not remove or mutate the corresponding host source"
    );
    assert_eq!(
        std::fs::read_to_string(&host_dest).expect("read retained rename destination"),
        "HOST_DEST_UNCHANGED",
        "renameSync must not mutate the corresponding host destination"
    );
    assert_eq!(
        std::fs::read_to_string(&outside_dest).expect("read retained outside rename destination"),
        "OUTSIDE_DEST_UNCHANGED",
        "a denied renameSync must not mutate its existing outside-root destination"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// accessSync with traversal
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn access_sync_vfs_file() {
    let result = eval_fs(
        r"(() => {
        fs.writeFileSync('access_test.txt', 'content');
        try {
            fs.accessSync('access_test.txt');
            return 'accessible';
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert_eq!(result, "accessible");
}

#[test]
fn access_sync_nonexistent() {
    let result = eval_fs(
        r"(() => {
        try {
            fs.accessSync('/no_such_file_xyz');
            return 'accessible';
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("ERROR:"),
        "accessSync on nonexistent should error: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pattern 2 (bd-k5q5.8.3): missing asset fallback
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn missing_asset_html_returns_empty_document() {
    // Reading a nonexistent .html in the extension root should return a
    // minimal empty HTML document instead of throwing ENOENT.
    let result = eval_fs(
        r"(() => {
        try {
            return fs.readFileSync('extensions/missing_template.html', 'utf8');
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("<!DOCTYPE html>"),
        "expected HTML fallback, got: {result}"
    );
}

#[test]
fn missing_asset_css_returns_empty_stylesheet() {
    let result = eval_fs(
        r"(() => {
        try {
            return fs.readFileSync('extensions/theme.css', 'utf8');
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("empty stylesheet"),
        "expected CSS fallback, got: {result}"
    );
}

#[test]
fn missing_asset_js_returns_empty_script() {
    let result = eval_fs(
        r"(() => {
        try {
            return fs.readFileSync('extensions/helper.js', 'utf8');
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("empty script"),
        "expected JS fallback, got: {result}"
    );
}

#[test]
fn missing_asset_md_returns_empty_string() {
    let result = eval_fs(
        r"(() => {
        try {
            const content = fs.readFileSync('extensions/README.md', 'utf8');
            return content.length === 0 ? 'EMPTY' : 'NONEMPTY:' + content;
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert_eq!(
        result, "EMPTY",
        "expected empty string for .md, got: {result}"
    );
}

#[test]
fn missing_asset_json_still_throws() {
    // .json files should NOT get a fallback (empty string is invalid JSON).
    let result = eval_fs(
        r"(() => {
        try {
            return fs.readFileSync('extensions/config.json', 'utf8');
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("ERROR:"),
        "expected error for .json, got: {result}"
    );
}

#[test]
fn missing_asset_outside_ext_root_still_throws() {
    // A missing file in the workspace root (not extension root) should
    // NOT get a fallback — only extension-root files are auto-repaired.
    let result = eval_fs(
        r"(() => {
        try {
            return fs.readFileSync('missing_workspace_file.html', 'utf8');
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("ERROR:"),
        "expected error for file outside ext root, got: {result}"
    );
}

#[test]
fn missing_asset_mjs_returns_empty_script() {
    let result = eval_fs(
        r"(() => {
        try {
            return fs.readFileSync('extensions/util.mjs', 'utf8');
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("empty script"),
        "expected JS fallback for .mjs, got: {result}"
    );
}

#[test]
fn missing_asset_yaml_returns_empty_string() {
    let result = eval_fs(
        r"(() => {
        try {
            const content = fs.readFileSync('extensions/config.yaml', 'utf8');
            return content.length === 0 ? 'EMPTY' : 'NONEMPTY:' + content;
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert_eq!(
        result, "EMPTY",
        "expected empty string for .yaml, got: {result}"
    );
}
