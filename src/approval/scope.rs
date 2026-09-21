//! Lexical scope admission for plan-scoped file auto-approval.
//!
//! One top-level `Files:` declaration is required, either the original
//! comma/space-separated form or a JSON string array (for paths with spaces).
//! Exact files are exact; a trailing `/` explicitly grants a directory tree.
//! `*` stays within one component, and a whole `**` component is recursive.
//! Wildcards do not implicitly match dot-prefixed components. Malformed,
//! duplicate, over-budget or unsupported declarations grant nothing.
//!
//! This does not resolve symlinks or authorize filesystem access. The tool's
//! workspace, capability and hard-policy gates remain authoritative; callers
//! must not treat a lexical match as a filesystem containment proof.

use serde_json::Value;

const MAX_ENTRIES: usize = 128;
const MAX_PATH_BYTES: usize = 1024;
const MAX_COMPONENTS: usize = 64;

/// Make structured tool input part of the exact text that gets reviewed.
/// It must not override another declaration or live only in hidden metadata.
pub(crate) fn append_files_declaration(plan: &str, files: &Value) -> Result<String, &'static str> {
    if plan.len() > crate::plan::MAX_PLAN_BYTES {
        return Err("plan plus files exceeds the 256 KiB UTF-8 limit");
    }
    let files = files
        .as_array()
        .ok_or("files must be an array of relative path strings")?;
    if files.is_empty() || files.len() > MAX_ENTRIES {
        return Err("files must contain between 1 and 128 entries");
    }
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let entry = file.as_str().ok_or("every files entry must be a string")?;
        let entry = normalized_path(entry, true)
            .ok_or("files contains an unsupported, ambiguous or over-budget relative path")?;
        entries.push(entry);
    }
    let declaration = serde_json::to_string(&entries).map_err(|_| "cannot encode files")?;
    if plan.len() + "\n\nFiles: ".len() + declaration.len() > crate::plan::MAX_PLAN_BYTES {
        return Err("plan plus files exceeds the 256 KiB UTF-8 limit");
    }
    let text = format!("{plan}\n\nFiles: {declaration}");
    if parse_plan_files(&text).len() != entries.len() {
        return Err("use files or one top-level Files: declaration, not both; close any code fence");
    }
    Ok(text)
}

pub(super) fn plan_covers_target(plan_text: Option<&str>, tool_args: &Value) -> bool {
    let Some(path) = tool_args.get("path").and_then(Value::as_str) else {
        return false;
    };
    let Some(path) = normalized_path(path, false) else {
        return false;
    };
    let Some(text) = plan_text else { return false };
    parse_plan_files(text)
        .iter()
        .any(|entry| entry_matches(entry, path))
}

/// Validate the complete declaration before matching any target. Do not keep
/// a valid prefix of a malformed list: that was not an unambiguous grant.
pub(super) fn parse_plan_files(text: &str) -> Vec<String> {
    if text.len() > crate::plan::MAX_PLAN_BYTES {
        return Vec::new();
    }
    let mut declaration = None;
    let mut fence: Option<(u8, usize)> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some((marker, count)) = fence {
            let run = line.bytes().take_while(|byte| *byte == marker).count();
            if run >= count && line[run..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if let Some(marker @ (b'`' | b'~')) = line.bytes().next() {
            let count = line.bytes().take_while(|byte| *byte == marker).count();
            if count >= 3 {
                fence = Some((marker, count));
                continue;
            }
        }
        // Indented Markdown code examples are not scope declarations.
        if raw.starts_with("    ") || raw.trim_start_matches(' ').starts_with('\t') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("files") {
            continue;
        }
        if declaration.replace(value.trim()).is_some() {
            return Vec::new();
        }
    }
    if fence.is_some() {
        return Vec::new();
    }
    let Some(raw) = declaration else {
        return Vec::new();
    };
    let entries: Vec<String> = if raw.starts_with('[') {
        let Ok(entries) = serde_json::from_str(raw) else {
            return Vec::new();
        };
        entries
    } else {
        let tokens: Vec<&str> = raw
            .split([',', ' ', '\t'])
            .filter(|entry| !entry.is_empty())
            .take(MAX_ENTRIES + 1)
            .collect();
        if tokens.len() > MAX_ENTRIES {
            return Vec::new();
        }
        tokens.into_iter().map(str::to_string).collect()
    };
    if entries.is_empty() || entries.len() > MAX_ENTRIES {
        return Vec::new();
    }
    let mut normalized = Vec::with_capacity(entries.len());
    for entry in &entries {
        let Some(entry) = normalized_path(entry, true) else {
            return Vec::new();
        };
        normalized.push(entry.to_string());
    }
    normalized
}

/// Admit a conservative, platform-independent relative path language. Do
/// not canonicalize `..` away: resolving it requires the owning workspace
/// and may traverse a symlink. Unsupported forms simply require approval.
fn normalized_path(path: &str, pattern: bool) -> Option<&str> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.trim() != path
        || path.chars().any(char::is_control)
        || path.contains(['\\', ':', '~', '$', '?', '[', ']', '{', '}', '"', '\'', '`', '#'])
    {
        return None;
    }
    let path = path.trim_start_matches("./");
    let directory = pattern && path.ends_with('/');
    let components = if directory {
        path.strip_suffix('/')?
    } else {
        path
    };
    if components.is_empty() || components.split('/').count() > MAX_COMPONENTS {
        return None;
    }
    for component in components.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.ends_with(['.', ' '])
            || reserved_device(component)
        {
            return None;
        }
        if component.contains('*')
            && (!pattern || directory || (component.contains("**") && component != "**"))
        {
            return None;
        }
    }
    Some(path)
}

// Avoid granting Windows device/alternate-name semantics even when this
// evaluator runs on Unix. A scope can be reused by a different frontend/OS.
fn reserved_device(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
            })
}

fn entry_matches(entry: &str, path: &str) -> bool {
    if entry.ends_with('/') {
        // Only explicit directory scopes authorize descendants. An exact
        // file entry must not also authorize `that-file/anything`.
        return path.starts_with(entry);
    }
    if !entry.contains('*') {
        return entry == path;
    }
    let paths: Vec<&str> = path.split('/').collect();
    let mut states = vec![false; paths.len() + 1];
    states[0] = true;
    let mut components = entry.split('/').peekable();
    while let Some(component) = components.next() {
        let mut next = vec![false; paths.len() + 1];
        if component == "**" {
            // A terminal globstar must consume a target; `src/**` must not
            // authorize replacing `src` itself. Interior globstars may be empty.
            let terminal = components.peek().is_none();
            next[0] = !terminal && states[0];
            for (index, part) in paths.iter().enumerate() {
                next[index + 1] = (!terminal && states[index + 1])
                    || ((next[index] || (terminal && states[index])) && !part.starts_with('.'));
            }
        } else {
            for (index, part) in paths.iter().enumerate() {
                next[index + 1] = states[index] && component_matches(component, part);
            }
        }
        states = next;
    }
    states[paths.len()]
}

fn component_matches(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    if !name.starts_with(first) {
        return false;
    }
    let mut rest = &name[first.len()..];
    let mut parts = parts.peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return rest.ends_with(part);
        }
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }
    rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ApprovalMode, ApprovalState};
    use crate::plan::{PlanMode, PlanState};
    use crate::tools::ToolEffects;
    use serde_json::json;

    fn covers(declaration: &str, path: &str) -> bool {
        plan_covers_target(Some(declaration), &json!({"path": path}))
    }

    #[test]
    fn traversal_and_ambiguous_paths_never_inherit_directory_approval() {
        for path in [
            "src/../secret", "src/a/../../secret", "../src/a", "/src/a",
            "src/./a", "src//a", "src/a/", "src/a.", "src/a /b",
            "src/a\0b", "src/a\nb", "src\\a", "C:/src/a", "//host/src/a",
            "src/a:stream", "~/src/a", "$HOME/src/a", "src/NUL.txt",
            "src/COM1", "src/LPT9.log", "src/COM¹.txt", "src/LPT²", "src/NUL .txt",
            "src/CONIN$", "./", ".", "",
        ] {
            assert!(!covers("Files: src/", path), "unexpected grant for {path:?}");
        }
        assert!(covers("Files: src/", "./src/a"));
        assert!(covers("Files: src/", "src/a/b"));
        assert!(covers("Files: src/", "src/console.rs"));
    }

    #[test]
    fn exact_files_do_not_grant_descendants_or_name_prefixes() {
        assert!(covers("Files: src/main.rs", "src/main.rs"));
        assert!(!covers("Files: src/main.rs", "src/main.rs/child"));
        assert!(!covers("Files: src/main.rs", "src/main.rs.bak"));
        assert!(!covers("Files: src/tools", "src/tools/read.rs"));
        assert!(covers("Files: src/tools/", "src/tools/read.rs"));
        assert!(!covers("Files: src/tools/", "src/toolsmith/read.rs"));
    }

    #[test]
    fn star_is_component_local_and_globstar_is_explicitly_recursive() {
        assert!(covers("Files: tests/*.rs", "tests/unit.rs"));
        assert!(!covers("Files: tests/*.rs", "tests/nested/unit.rs"));
        for path in ["tests/unit.rs", "tests/nested/unit.rs", "tests/a/b/unit.rs"] {
            assert!(covers("Files: tests/**/*.rs", path), "{path}");
        }
        assert!(!covers("Files: tests/**/*.rs", "tests/unit.rs.bak"));
        assert!(!covers("Files: tests/**/*.rs", "other/unit.rs"));
        assert!(!covers("Files: src/**", "src"));
        assert!(!covers("Files: sr*/**", "src"));
        assert!(covers("Files: src/**", "src/file"));
        assert!(covers("Files: src/**/test*/case.rs", "src/tests/a/test_one/case.rs"));
    }

    #[test]
    fn wildcard_matching_never_implicitly_grants_hidden_components() {
        assert!(!covers("Files: **/*.rs", ".private/secret.rs"));
        assert!(!covers("Files: src/**", "src/.git/hooks/pre-commit"));
        assert!(!covers("Files: src/*.rs", "src/.secret.rs"));
        assert!(covers("Files: src/.*.rs", "src/.config.rs"));
        assert!(covers("Files: src/.private/**/*.rs", "src/.private/test.rs"));
    }

    #[test]
    fn full_declaration_is_validated_not_just_the_matching_prefix() {
        for declaration in [
            "Files: src/ ../secrets/",
            "Files: src/, /absolute",
            "Files: src/\nFiles: other/",
            "Files: src/\nfIlEs: other/",
            "Files: src/, tests/a**b.rs",
            "Files: src/, tests/?.rs",
            "Files: src/, tests/*/",
            "Files: src/, C:\\secret",
            "Files: src/, \"two words\"",
        ] {
            assert!(!covers(declaration, "src/main.rs"), "{declaration}");
        }
        assert!(covers("fIlEs: src/", "src/main.rs"));
    }

    #[test]
    fn json_scope_preserves_spaces_commas_and_unicode_as_literal_paths() {
        let declaration = r#"Files: ["src/hello world.rs", "src/a,b.rs", "文档/计划.md"]"#;
        for path in ["src/hello world.rs", "src/a,b.rs", "文档/计划.md"] {
            assert!(covers(declaration, path));
        }
        for path in ["src/hello", "world.rs", "src/a", "b.rs", "文档/other.md"] {
            assert!(!covers(declaration, path));
        }
    }

    #[test]
    fn malformed_json_scopes_grant_nothing() {
        for declaration in [
            r#"Files: ["src/", 7]"#,
            r#"Files: ["src/", "../"]"#,
            r#"Files: ["src/", ""]"#,
            r#"Files: ["src/",]"#,
            r#"Files: ["src/"] trailing"#,
            r#"Files: []"#,
            r#"Files: ["src/", null]"#,
        ] {
            assert!(!covers(declaration, "src/main.rs"), "{declaration}");
        }
    }

    #[test]
    fn code_examples_are_not_permission_declarations() {
        for plan in [
            "```text\nFiles: src/\n```",
            "~~~\nFiles: src/\n~~~",
            "    Files: src/",
            "\tFiles: src/",
            " \tFiles: src/",
            "Files: src/\n```\nunclosed fence",
        ] {
            assert!(!covers(plan, "src/main.rs"), "{plan}");
        }
        let plan = "````\n```\nFiles: other/\n````\nFiles: src/";
        assert!(covers(plan, "src/main.rs"));
        assert!(!covers(plan, "other/file"));
    }

    #[test]
    fn scope_count_path_size_depth_and_plan_bytes_are_bounded() {
        let entries = vec!["src/"; MAX_ENTRIES];
        let exact = format!("Files: {}", serde_json::to_string(&entries).unwrap());
        assert!(covers(&exact, "src/main.rs"));
        let excess = format!("Files: {}", vec!["src/"; MAX_ENTRIES + 1].join(","));
        assert!(!covers(&excess, "src/main.rs"));
        let excess_json = format!(
            "Files: {}",
            serde_json::to_string(&vec!["src/"; MAX_ENTRIES + 1]).unwrap()
        );
        assert!(!covers(&excess_json, "src/main.rs"));
        assert!(!covers("Files: **", &"x".repeat(MAX_PATH_BYTES + 1)));
        assert!(!covers("Files: **", &vec!["x"; MAX_COMPONENTS + 1].join("/")));
        let plan = format!("Files: src/\n{}", "x".repeat(crate::plan::MAX_PLAN_BYTES));
        assert!(!covers(&plan, "src/main.rs"));
        let invalid_tail = format!("Files: src/, {}", "x".repeat(MAX_PATH_BYTES + 1));
        assert!(!covers(&invalid_tail, "src/main.rs"));
    }

    #[test]
    fn absent_or_nonliteral_target_arguments_cannot_inherit_scope() {
        for arguments in [json!({}), json!({"path": null}), json!({"path": ["src/a"]}), json!({"path": "src/*.rs"})] {
            assert!(!plan_covers_target(Some("Files: src/"), &arguments));
        }
        assert!(!plan_covers_target(None, &json!({"path": "src/a"})));
    }

    #[test]
    fn component_glob_is_anchored_nonoverlapping_and_unicode_safe() {
        for (pattern, name, expected) in [
            ("a*a", "a", false), ("ab*bc", "abc", false),
            ("ab*bc", "abbc", true), ("a*b*c", "a1b2c", true),
            ("a*b*c", "xa1b2c", false), ("a*b*c", "a1b2cx", false),
            ("*é*计划", "café和计划", true), ("é*é", "é", false),
            ("*", "name", true), ("*", ".hidden", false),
        ] {
            assert_eq!(component_matches(pattern, name), expected, "{pattern} / {name}");
        }
    }

    fn approved(text: &str) -> PlanState {
        let plan = PlanState::new();
        plan.enter_planning();
        assert!(plan.submit_plan(text.to_string()));
        assert!(plan.approve().is_some());
        plan
    }

    #[test]
    fn production_approval_rejects_traversal_despite_a_matching_raw_prefix() {
        let approval = ApprovalState::new(ApprovalMode::AlwaysAsk, true, Vec::new());
        let plan = approved("Files: src/");
        for path in ["src/../outside", "src/main.rs/../../outside", "src\\..\\outside"] {
            let result = approval.evaluate("write", &json!({"path": path}), ToolEffects::write(), Some(&plan), None);
            assert!(result.requires_approval(), "{path}");
        }
        assert!(approval.evaluate("write", &json!({"path": "src/main.rs"}), ToolEffects::write(), Some(&plan), None).is_auto_approved());
    }

    #[test]
    fn only_known_single_file_writers_inherit_plan_scope() {
        let approval = ApprovalState::new(ApprovalMode::AlwaysAsk, true, Vec::new());
        let plan = approved("Files: src/");
        for name in ["write", "edit", "hashline_edit"] {
            assert!(approval.evaluate(name, &json!({"path": "src/a"}), ToolEffects::write(), Some(&plan), None).is_auto_approved());
        }
        for name in ["custom_writer", "ast_edit", "lsp", "mcp", "append"] {
            assert!(approval.evaluate(name, &json!({"path": "src/a"}), ToolEffects::write(), Some(&plan), None).requires_approval());
        }
        for effects in [ToolEffects::write().union(ToolEffects::process()), ToolEffects::write().union(ToolEffects::network())] {
            assert!(approval.evaluate("write", &json!({"path": "src/a"}), effects, Some(&plan), None).requires_approval());
        }
    }

    #[test]
    fn pending_or_rejected_revisions_do_not_borrow_prior_approval() {
        let approval = ApprovalState::new(ApprovalMode::AlwaysAsk, true, Vec::new());
        let plan = approved("Files: src/");
        plan.enter_planning();
        assert!(plan.submit_plan("Files: other/".to_string()));
        for mode in [PlanMode::PendingApproval, PlanMode::Planning] {
            assert_eq!(plan.mode(), mode);
            for path in ["src/a", "other/a"] {
                assert!(approval.evaluate("write", &json!({"path": path}), ToolEffects::write(), Some(&plan), None).requires_approval());
            }
            let _ = plan.reject();
        }
    }

    #[test]
    fn explicit_global_policy_still_decides_when_plan_scope_does_not_apply() {
        let plan = approved("Files: src/");
        for mode in [ApprovalMode::Write, ApprovalMode::Yolo] {
            let approval = ApprovalState::new(mode, true, Vec::new());
            assert!(approval.evaluate("write", &json!({"path": "outside"}), ToolEffects::write(), Some(&plan), None).is_auto_approved());
        }
        let disabled = ApprovalState::new(ApprovalMode::AlwaysAsk, false, Vec::new());
        assert!(disabled.evaluate("write", &json!({"path": "src/a"}), ToolEffects::write(), Some(&plan), None).requires_approval());
    }

    fn submit_input(plan: &PlanState, auto: bool, input: Value) -> crate::tools::ToolOutput {
        use crate::tools::Tool;
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let tool = crate::plan::SubmitPlanTool::new(plan.clone(), auto);
        runtime.block_on(tool.execute("scope-test", input, None)).unwrap()
    }

    #[test]
    fn structured_files_are_visible_reviewed_and_used_by_the_production_policy() {
        let text = "Goal: update the selected files. Verification: run their tests.";
        for auto in [false, true] {
            let plan = PlanState::new();
            plan.enter_planning();
            let result = submit_input(&plan, auto, json!({
                "plan": text,
                "files": ["src/hello world.rs", "src/a,b.rs", "tests/**/*.rs"]
            }));
            assert!(!result.is_error);
            let reviewed = plan.plan().unwrap();
            assert_eq!(reviewed, format!(
                "{text}\n\nFiles: [\"src/hello world.rs\",\"src/a,b.rs\",\"tests/**/*.rs\"]"
            ));
            if !auto {
                assert_eq!(plan.mode(), PlanMode::PendingApproval);
                assert_eq!(plan.approve_reviewed(&reviewed), Some(reviewed));
            }
            let policy = ApprovalState::new(ApprovalMode::AlwaysAsk, true, Vec::new());
            for (path, expected) in [
                ("src/hello world.rs", true), ("src/a,b.rs", true),
                ("tests/nested/unit.rs", true), ("src/hello", false),
                ("world.rs", false), ("src/a", false), ("b.rs", false),
                ("tests/../secret.rs", false),
            ] {
                let decision = policy.evaluate(
                    "write", &json!({"path": path}), ToolEffects::write(), Some(&plan), None,
                );
                assert_eq!(decision.is_auto_approved(), expected, "{path}");
            }
        }
    }

    #[test]
    fn invalid_structured_files_do_not_replace_an_existing_proposal() {
        let plan = PlanState::new();
        plan.enter_planning();
        assert!(plan.submit_plan("retained previous proposal".to_string()));
        assert!(plan.reject());
        for files in [
            json!(null), json!([]), json!(["src/", 7]), json!(["src/", "../secret"]),
            json!(["src/", ""]), json!(["src/", "C:/secret"]),
            json!(vec!["src/"; MAX_ENTRIES + 1]),
        ] {
            let result = submit_input(&plan, true, json!({
                "plan": "Goal: replace files. Verification: test the change.", "files": files
            }));
            assert!(result.is_error);
            assert_eq!(result.details.unwrap()["planReview"], "invalid_scope");
            assert_eq!(plan.mode(), PlanMode::Planning);
            assert_eq!(plan.plan().as_deref(), Some("retained previous proposal"));
        }
    }

    #[test]
    fn structured_files_cannot_override_or_hide_behind_textual_declarations() {
        for text in [
            "Goal: fix code. Files below are authoritative.\nFiles: old/",
            "Goal: fix code.\n```text\nAn unclosed example",
        ] {
            let plan = PlanState::new();
            plan.enter_planning();
            let result = submit_input(&plan, true, json!({"plan": text, "files": ["new/"]}));
            assert!(result.is_error);
            assert_eq!(plan.mode(), PlanMode::Planning);
            assert!(plan.plan().is_none());
        }
        let text = "Goal: edit new files. Example only:\n```\nFiles: old/\n```";
        let combined = append_files_declaration(text, &json!(["new/"])).unwrap();
        assert!(covers(&combined, "new/file"));
        assert!(!covers(&combined, "old/file"));
    }

    #[test]
    fn structured_files_share_the_plan_byte_budget() {
        let plan = PlanState::new();
        plan.enter_planning();
        let result = submit_input(&plan, true, json!({
            "plan": "x".repeat(crate::plan::MAX_PLAN_BYTES), "files": ["src/"]
        }));
        assert!(result.is_error);
        assert_eq!(plan.mode(), PlanMode::Planning);
        assert!(plan.plan().is_none());
    }

    #[test]
    fn component_matcher_agrees_with_an_independent_exhaustive_oracle() {
        fn oracle(pattern: &[u8], name: &[u8]) -> bool {
            match pattern.first() {
                None => name.is_empty(),
                Some(b'*') => oracle(&pattern[1..], name)
                    || (!name.is_empty() && oracle(pattern, &name[1..])),
                Some(byte) => name.first() == Some(byte) && oracle(&pattern[1..], &name[1..]),
            }
        }
        fn words(alphabet: &[char]) -> Vec<String> {
            let mut words = vec![String::new()];
            let mut start = 0;
            let mut end = 1;
            for _ in 0..4 {
                for index in start..end {
                    let prefix = words[index].clone();
                    for character in alphabet {
                        words.push(format!("{prefix}{character}"));
                    }
                }
                start = end;
                end = words.len();
            }
            words
        }
        for pattern in words(&['a', 'b', '*']) {
            for name in words(&['a', 'b']) {
                assert_eq!(
                    component_matches(&pattern, &name),
                    oracle(pattern.as_bytes(), name.as_bytes()),
                    "pattern={pattern:?}, name={name:?}"
                );
            }
        }
    }
}
