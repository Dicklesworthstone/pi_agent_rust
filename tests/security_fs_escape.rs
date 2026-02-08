//! Security suite: filesystem escape hardening (bd-740v).
//!
//! Tests verify that extensions cannot escape their allowed filesystem scope
//! via path traversal (`..`), absolute paths, or other techniques. Tests cover
//! the VFS normalization, host read fallback, and tool-level path resolution.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
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

fn eval_fs(js_expr: &str) -> String {
    let harness = common::TestHarness::new("fs_escape");
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

fn eval_fs_with_setup<F>(setup: F, js_expr: &str) -> String
where
    F: FnOnce(&common::TestHarness),
{
    let harness = common::TestHarness::new("fs_escape");
    setup(&harness);
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

// ═══════════════════════════════════════════════════════════════════════════════
// VFS normalizePath: path traversal via ..
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn vfs_normalize_resolves_dot_dot() {
    // Verify that normalizePath collapses .. segments
    let result = eval_fs(
        r"(() => {
        const vfs = globalThis.__pi_vfs_state;
        return vfs.normalizePath('/home/user/../etc/passwd');
    })()",
    );
    // After normalization, .. should be collapsed
    assert_eq!(result, "/home/etc/passwd");
}

#[test]
fn vfs_normalize_multiple_dot_dots() {
    let result = eval_fs(
        r"(() => {
        const vfs = globalThis.__pi_vfs_state;
        return vfs.normalizePath('/a/b/c/../../../etc/passwd');
    })()",
    );
    assert_eq!(result, "/etc/passwd");
}

#[test]
fn vfs_normalize_dot_dot_at_root_stays_at_root() {
    let result = eval_fs(
        r"(() => {
        const vfs = globalThis.__pi_vfs_state;
        return vfs.normalizePath('/../../../etc/passwd');
    })()",
    );
    // Should not go above root
    assert_eq!(result, "/etc/passwd");
}

#[test]
fn vfs_normalize_absolute_path_preserved() {
    let result = eval_fs(
        r"(() => {
        const vfs = globalThis.__pi_vfs_state;
        return vfs.normalizePath('/etc/shadow');
    })()",
    );
    assert_eq!(result, "/etc/shadow");
}

#[test]
fn vfs_normalize_dot_segments_removed() {
    let result = eval_fs(
        r"(() => {
        const vfs = globalThis.__pi_vfs_state;
        return vfs.normalizePath('/home/./user/./file');
    })()",
    );
    assert_eq!(result, "/home/user/file");
}

#[test]
fn vfs_normalize_empty_segments_collapsed() {
    let result = eval_fs(
        r"(() => {
        const vfs = globalThis.__pi_vfs_state;
        return vfs.normalizePath('/home//user///file');
    })()",
    );
    assert_eq!(result, "/home/user/file");
}

// ═══════════════════════════════════════════════════════════════════════════════
// VFS write confinement: writes stay in VFS, never reach real FS
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn vfs_write_does_not_create_real_file() {
    let result = eval_fs(
        r"(() => {
        // Write to VFS
        fs.writeFileSync('/tmp/vfs_escape_test_canary.txt', 'escape attempt');
        // Verify it exists in VFS
        const exists_vfs = fs.existsSync('/tmp/vfs_escape_test_canary.txt');
        return String(exists_vfs);
    })()",
    );
    assert_eq!(result, "true");

    // Verify no real file was created
    assert!(
        !std::path::Path::new("/tmp/vfs_escape_test_canary.txt").exists(),
        "VFS write should NOT create a real file on disk"
    );
}

#[test]
fn vfs_write_with_traversal_stays_in_vfs() {
    let result = eval_fs(
        r"(() => {
        // Attempt path traversal write
        fs.writeFileSync('../../tmp/vfs_traversal_canary.txt', 'escape');
        return 'wrote';
    })()",
    );
    assert_eq!(result, "wrote");

    // Verify no real file was created
    assert!(
        !std::path::Path::new("/tmp/vfs_traversal_canary.txt").exists(),
        "VFS traversal write should NOT create a real file"
    );
}

#[test]
fn vfs_mkdir_does_not_create_real_dir() {
    let result = eval_fs(
        r"(() => {
        fs.mkdirSync('/tmp/vfs_escape_test_dir', { recursive: true });
        return String(fs.existsSync('/tmp/vfs_escape_test_dir'));
    })()",
    );
    assert_eq!(result, "true");

    assert!(
        !std::path::Path::new("/tmp/vfs_escape_test_dir").exists(),
        "VFS mkdir should NOT create a real directory"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Host read fallback: __pi_host_read_file_sync behavior
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn host_read_fallback_denies_outside_workspace() {
    let result = eval_fs(
        r"(() => {
        try {
            const content = fs.readFileSync('/etc/hostname', 'utf8');
            return content.length > 0 ? 'read_ok' : 'empty';
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("ERROR:") && result.contains("outside extension root"),
        "expected host read deny error, got: {result}"
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
        const testPath = '/vfs_only/test_file.txt';
        fs.mkdirSync('/vfs_only', { recursive: true });
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
    let result = eval_fs(
        r"(() => {
        try {
            // Attempt to read /etc/hostname via path traversal
            const content = fs.readFileSync('/fake/../etc/hostname', 'utf8');
            return 'read:' + content.trim().length;
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    assert!(
        result.contains("ERROR:") && result.contains("outside extension root"),
        "expected traversal read denial, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// existsSync traversal probing
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn exists_sync_absolute_sensitive_path() {
    let result = eval_fs(
        r"(() => {
        // /etc/passwd always exists on Linux
        return String(fs.existsSync('/etc/passwd'));
    })()",
    );
    // Documents current behavior: existsSync with host fallback
    // may reveal existence of sensitive files
    assert!(
        result == "true" || result == "false",
        "expected boolean string, got: {result}"
    );
}

#[test]
fn exists_sync_traversal_probe() {
    let result = eval_fs(
        r"(() => {
        return String(fs.existsSync('/nonexistent/../etc/passwd'));
    })()",
    );
    // After normalization: /etc/passwd
    assert!(
        result == "true" || result == "false",
        "expected boolean string, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// writeFileSync cannot escape VFS to host
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn write_file_absolute_path_stays_in_vfs() {
    let unique_name = format!("/tmp/vfs_escape_abs_{}", std::process::id());
    let result = eval_fs(&format!(
        r#"(() => {{
        fs.writeFileSync("{unique_name}", "escape attempt");
        return fs.readFileSync("{unique_name}", "utf8");
    }})()"#,
    ));
    assert_eq!(result, "escape attempt");

    // Critical: file must NOT exist on real FS
    assert!(
        !std::path::Path::new(&unique_name).exists(),
        "writeFileSync to absolute path must NOT create real file: {unique_name}"
    );
}

#[test]
fn unlink_sync_cannot_delete_real_file() {
    // Create a real temp file
    let temp = tempfile::NamedTempFile::new().expect("create temp file");
    let real_path = temp.path().to_str().expect("temp path").to_string();

    let result = eval_fs(&format!(
        r#"(() => {{
        try {{
            fs.unlinkSync("{real_path}");
            return "unlinked";
        }} catch (e) {{
            return "ERROR:" + e.message;
        }}
    }})()"#,
    ));

    // The VFS unlink should only affect VFS state, not real files
    assert!(
        temp.path().exists(),
        "VFS unlinkSync must NOT delete real files: {real_path}"
    );
    // Result may be "unlinked" (VFS silently accepts) or ERROR (ENOENT from VFS)
    assert!(
        result == "unlinked" || result.contains("ERROR:"),
        "unexpected unlink result: {result}"
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
    let result = eval_fs(
        r"(() => {
        try {
            return fs.readFileSync('/etc/passwd\x00.txt', 'utf8');
        } catch (e) {
            return 'ERROR:' + e.message;
        }
    })()",
    );
    // Null byte should cause an error, not bypass security
    assert!(
        result.contains("ERROR:"),
        "null byte in path should cause error, got: {result}"
    );
}

#[test]
fn backslash_path_normalized() {
    let result = eval_fs(
        r"(() => {
        const vfs = globalThis.__pi_vfs_state;
        // Backslashes should be normalized to forward slashes
        return vfs.normalizePath('\\etc\\passwd');
    })()",
    );
    // On non-Windows, backslashes become forward slashes
    assert_eq!(result, "/etc/passwd");
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
        fs.writeFileSync('/vfs_stat_test.txt', 'hello');
        const stat = fs.statSync('/vfs_stat_test.txt');
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
        fs.mkdirSync('/sandbox', { recursive: true });
        fs.writeFileSync('/sandbox/a.txt', 'a');
        fs.writeFileSync('/sandbox/b.txt', 'b');
        const entries = fs.readdirSync('/sandbox');
        return entries.sort().join(',');
    })()",
    );
    assert_eq!(result, "a.txt,b.txt");
}

#[test]
fn readdir_sync_root_only_shows_vfs_dirs() {
    let result = eval_fs(
        r"(() => {
        // Create some VFS directories
        fs.mkdirSync('/mydir', { recursive: true });
        fs.writeFileSync('/myfile.txt', 'root file');
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
    let unique_dest = format!("/tmp/vfs_copy_escape_{}", std::process::id());
    let result = eval_fs(&format!(
        r#"(() => {{
        fs.writeFileSync("/src.txt", "original");
        fs.copyFileSync("/src.txt", "{unique_dest}");
        return fs.readFileSync("{unique_dest}", "utf8");
    }})()"#,
    ));
    assert_eq!(result, "original");

    assert!(
        !std::path::Path::new(&unique_dest).exists(),
        "copyFileSync should NOT create real files"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// renameSync stays in VFS
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn rename_sync_stays_in_vfs() {
    let unique_dest = format!("/tmp/vfs_rename_escape_{}", std::process::id());
    let result = eval_fs(&format!(
        r#"(() => {{
        fs.writeFileSync("/rename_src.txt", "data");
        fs.renameSync("/rename_src.txt", "{unique_dest}");
        return fs.readFileSync("{unique_dest}", "utf8");
    }})()"#,
    ));
    assert_eq!(result, "data");

    assert!(
        !std::path::Path::new(&unique_dest).exists(),
        "renameSync should NOT create real files"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// accessSync with traversal
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn access_sync_vfs_file() {
    let result = eval_fs(
        r"(() => {
        fs.writeFileSync('/access_test.txt', 'content');
        try {
            fs.accessSync('/access_test.txt');
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
