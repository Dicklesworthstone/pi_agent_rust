//! Package map selection is independent of filesystem existence and loading.
//!
//! Choose one exact entry or most-specific pattern, then one conditional or
//! array target. Only invalid target syntax permits array fallback. Missing
//! files, unsupported module formats, permissions and compilation failures must
//! never select another candidate after the map has committed to a target.

use super::{Admission, Export, MAX_PATH_BYTES, error, valid_segments};

#[derive(Debug, PartialEq, Eq)]
enum Selection {
    Unmatched,
    Blocked,
    Path(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    InvalidTarget,
    InvalidMap,
    Limit,
}

impl Failure {
    fn message(self) -> String {
        match self {
            Self::InvalidTarget => error("TARGET", "package target must be a bounded ./ path without traversal or URL syntax"),
            Self::InvalidMap => error("EXPORTS", "invalid or mixed package map keys"),
            Self::Limit => error("LIMIT", "expanded package target exceeds the path byte budget"),
        }
    }
}

/// Validate before and after expansion. A wildcard cannot hide a traversal,
/// reserved segment or encoded separator in either the template or capture.
fn expand(target: &str, capture: Option<&str>) -> Result<String, Failure> {
    if target.len() > MAX_PATH_BYTES { return Err(Failure::Limit); }
    let relative = target.strip_prefix("./").ok_or(Failure::InvalidTarget)?;
    if !valid_segments(&relative.replace('*', "x")) {
        return Err(Failure::InvalidTarget);
    }
    let stars = target.bytes().filter(|byte| *byte == b'*').count();
    let Some(capture) = capture else {
        return if stars == 0 { Ok(target.to_owned()) } else { Err(Failure::InvalidTarget) };
    };
    if !valid_segments(capture) { return Err(Failure::InvalidTarget); }
    let size = capture.len().checked_mul(stars)
        .and_then(|bytes| target.len().checked_sub(stars)?.checked_add(bytes))
        .filter(|size| *size <= MAX_PATH_BYTES).ok_or(Failure::Limit)?;
    let expanded = target.replace('*', capture);
    debug_assert_eq!(expanded.len(), size);
    if !valid_segments(&expanded[2..]) { return Err(Failure::InvalidTarget); }
    Ok(expanded)
}

fn select(value: &Export, capture: Option<&str>) -> Result<Selection, Failure> {
    match value {
        Export::Invalid => Err(Failure::InvalidTarget),
        Export::Blocked => Ok(Selection::Blocked),
        Export::Target(target) => expand(target, capture).map(Selection::Path),
        Export::Conditions(entries) => {
            if entries.iter().any(|(key, _)| key.starts_with('.') || key.parse::<u32>().is_ok()) {
                return Err(Failure::InvalidMap);
            }
            // Preserve author order. This realm supplies neither Node nor
            // browser built-ins, so only import/default conditions match.
            for (condition, value) in entries {
                if matches!(condition.as_str(), "import" | "default") {
                    match select(value, capture)? {
                        Selection::Unmatched => {}
                        selected => return Ok(selected),
                    }
                }
            }
            Ok(Selection::Unmatched)
        }
        Export::Alternatives(entries) => {
            if entries.is_empty() { return Ok(Selection::Blocked); }
            let mut last = Ok(Selection::Unmatched);
            for value in entries {
                match select(value, capture) {
                    Ok(Selection::Unmatched) => {}
                    Ok(Selection::Blocked) => last = Ok(Selection::Blocked),
                    Err(Failure::InvalidTarget) => last = Err(Failure::InvalidTarget),
                    // Null/invalid array elements permit another syntactic
                    // alternative. Invalid configuration and budgets do not.
                    selected => return selected,
                }
            }
            last
        }
    }
}

fn valid_key(key: &str) -> bool {
    key == "." || key.strip_prefix("./").is_some_and(|relative| {
        key.bytes().filter(|byte| *byte == b'*').count() <= 1
            && valid_segments(&relative.replace('*', "x"))
    })
}

fn mapped(entries: &[(String, Export)], subpath: &str) -> Result<Selection, Failure> {
    if entries.iter().any(|(key, _)| !valid_key(key)) { return Err(Failure::InvalidMap); }
    if let Some((_, value)) = entries.iter().find(|(key, _)| key == subpath && !key.contains('*')) {
        return select(value, None);
    }
    let mut best: Option<(&Export, &str, usize, usize)> = None;
    for (key, value) in entries {
        let Some(star) = key.find('*') else { continue; };
        let prefix = &key[..star];
        let suffix = &key[star + 1..];
        let Some(capture) = subpath.strip_prefix(prefix).and_then(|rest| rest.strip_suffix(suffix)) else { continue; };
        if capture.is_empty() { continue; }
        // Most-specific prefix, then longest full key; never map iteration
        // order or filesystem availability. A selected null seals the entry.
        if best.as_ref().is_none_or(|(_, _, base, length)| (star, key.len()) > (*base, *length)) {
            best = Some((value, capture, star, key.len()));
        }
    }
    match best {
        Some((value, capture, _, _)) => select(value, Some(capture)),
        None => Ok(Selection::Unmatched),
    }
}

pub(super) fn exported_target(exports: &Export, subpath: &str) -> Admission<String> {
    let selected = if let Export::Conditions(entries) = exports
        && entries.iter().any(|(key, _)| key.starts_with('.'))
    {
        mapped(entries, subpath)
    } else if subpath == "." {
        select(exports, None)
    } else {
        Ok(Selection::Unmatched)
    }.map_err(Failure::message)?;
    match selected {
        Selection::Path(path) => Ok(path),
        _ => Err(error("NOT_EXPORTED", "package does not export this entry for import/default conditions")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(map: &str, name: &str) -> Admission<String> {
        exported_target(&serde_json::from_str(map).unwrap(), name)
    }

    #[test]
    fn exact_and_specific_patterns_win_independently_of_author_order() {
        let map = r#"{"./*":"./broad/*.mjs","./feature/*":"./feature/*.mjs","./feature/*.js":"./typed/*.mts","./feature/main.js":"./main.mjs"}"#;
        assert_eq!(target(map, "./feature/main.js").unwrap(), "./main.mjs");
        assert_eq!(target(map, "./feature/other.js").unwrap(), "./typed/other.mts");
        assert_eq!(target(map, "./feature/nested/file").unwrap(), "./feature/nested/file.mjs");
        assert_eq!(target(map, "./elsewhere").unwrap(), "./broad/elsewhere.mjs");
        let reverse = r#"{"./feature/*.js":"./typed/*.mts","./feature/*":"./feature/*.mjs","./*":"./broad/*.mjs"}"#;
        assert_eq!(target(reverse, "./feature/other.js").unwrap(), "./typed/other.mts");
    }

    #[test]
    fn exact_and_specific_nulls_seal_broader_patterns() {
        let map = r#"{"./*":"./public/*.mjs","./private/*":null,"./private/open":"./open.mjs","./closed":null}"#;
        assert!(target(map, "./private/secret").unwrap_err().contains("NOT_EXPORTED"));
        assert!(target(map, "./closed").unwrap_err().contains("NOT_EXPORTED"));
        assert_eq!(target(map, "./private/open").unwrap(), "./open.mjs");
        assert!(target(r#"{"./*":{"browser":"./*.mjs"},"./private/*":null}"#, "./private/x").is_err());
    }

    #[test]
    fn alternatives_skip_invalid_targets_not_invalid_maps_or_missing_files() {
        assert_eq!(target(r#"[false,42,"../outside.mjs",null,{"browser":"./browser.mjs"},"./ok.mjs"]"#, ".").unwrap(), "./ok.mjs");
        assert_eq!(target(r#"["./missing.mjs","./exists.mjs"]"#, ".").unwrap(), "./missing.mjs");
        assert_eq!(target(r#"["./unsupported.cjs","./exists.mjs"]"#, ".").unwrap(), "./unsupported.cjs");
        assert!(target(r#"[{"0":"./bad.mjs"},"./ok.mjs"]"#, ".").unwrap_err().contains("EXPORTS"));
        assert!(target(r#"["../bad.mjs"]"#, ".").unwrap_err().contains("TARGET"));
        assert!(target(r#"["../bad.mjs",null]"#, ".").unwrap_err().contains("NOT_EXPORTED"));
        assert!(target("[]", ".").unwrap_err().contains("NOT_EXPORTED"));
    }

    #[test]
    fn conditions_keep_author_order_inside_alternatives_and_patterns() {
        assert_eq!(target(r#"{"./*":{"default":["bad","./first-*.mjs"],"import":"./second-*.mjs"}}"#, "./x").unwrap(), "./first-x.mjs");
        assert!(target(r#"{"import":null,"default":"./fallback.mjs"}"#, ".").is_err());
        assert_eq!(target(r#"[{"browser":"./unused.mjs"},{"import":"./selected.mjs"}]"#, ".").unwrap(), "./selected.mjs");
    }

    #[test]
    fn repeated_target_stars_and_unicode_captures_are_literal() {
        assert_eq!(target(r#"{"./feature/*":"./copies/*/mirror-*.mjs"}"#, "./feature/界").unwrap(), "./copies/界/mirror-界.mjs");
        assert_eq!(target(r#"{"./feature/*":"./one.mjs"}"#, "./feature/nested/value").unwrap(), "./one.mjs");
        assert!(target(r#"{"./x*.js":"./*.mjs"}"#, "./x.js").is_err());
    }

    #[test]
    fn traversal_reserved_segments_and_ambiguous_map_shapes_fail_closed() {
        for map in [r#"{"./*":"./../*.mjs"}"#, r#"{"./*":"./node_modules/*.mjs"}"#,
            r#"{"./*":"./%2e%2e/*.mjs"}"#, r#"{"./a**":"./*.mjs"}"#,
            r#"{"./a/*":"./*.mjs","default":"./a.mjs"}"#] {
            assert!(target(map, "./a/x").is_err(), "{map}");
        }
        for capture in ["..", "x/../y", "node_modules/x", "%2foutside", "/absolute", "x\\y"] {
            assert!(expand("./*.mjs", Some(capture)).is_err(), "{capture}");
        }
    }

    #[test]
    fn expansion_is_bounded_before_allocation_and_cannot_fallback() {
        let huge = "x".repeat(MAX_PATH_BYTES / 2);
        assert_eq!(expand("./*/*/*.mjs", Some(&huge)), Err(Failure::Limit));
        let map = Export::Alternatives(vec![Export::Target("./*/*/*.mjs".into()), Export::Target("./small.mjs".into())]);
        assert_eq!(select(&map, Some(&huge)), Err(Failure::Limit));
        let exact = "x".repeat(MAX_PATH_BYTES - 6);
        assert_eq!(expand("./*.mjs", Some(&exact)).unwrap().len(), MAX_PATH_BYTES);
        assert_eq!(expand("./*.mjs", Some(&(exact + "x"))), Err(Failure::Limit));
    }

    fn write_package(root: &std::path::Path, name: &str, map: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let directory = root.join("node_modules").join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("package.json"), map).unwrap();
        for (name, text) in files {
            let path = directory.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        directory
    }

    fn eval(tool: &crate::eval::EvalTool, code: &str) -> crate::tools::ToolOutput {
        use crate::tools::Tool;
        asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8)
            .build().unwrap().block_on(tool.execute(
                "package-map", serde_json::json!({"kernel":"js","code":code}), None,
            )).unwrap()
    }

    fn shown(output: crate::tools::ToolOutput) -> serde_json::Value {
        assert!(!output.is_error, "{:?}", output.content);
        output.details.unwrap()["jsonOutputs"][0].clone()
    }

    #[test]
    fn public_eval_links_pattern_typed_and_data_exports_through_one_resolver() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), "@test/kit",
            r#"{"type":"module","exports":{"./feature/*":{"import":["../invalid","./src/*.mts"]},"./data/*":"./data/*.json","./same":"./src/math.mts"}}"#,
            &[("src/math.mts", "import {base} from 'base-kit'; export const answer: number = base + 2;"),
              ("data/answer.json", "{\"answer\":42}")]);
        write_package(dir.path(), "base-kit", r#"{"exports":[false,null,"./index.mjs"]}"#,
            &[("index.mjs", "export const base = 40;")]);
        let tool = crate::eval::EvalTool::new(dir.path());
        let result = eval(&tool, "globalThis.math = await import('@test/kit/feature/math'); const data = await import('@test/kit/data/answer', {with:{type:'json'}}); const same = await import('@test/kit/same'); display([math.answer, data.default.answer, same === math]);");
        assert_eq!(shown(result), serde_json::json!([42,42,true]));
        assert!(eval(&tool, "await import('@test/kit/data/answer')").is_error);
    }

    #[test]
    fn public_eval_never_falls_back_after_target_selection_or_a_sealed_match() {
        let dir = tempfile::tempdir().unwrap();
        write_package(dir.path(), "sealed",
            r#"{"exports":{"./*":"./open.mjs","./private/*":null,"./exact":null,"./missing":["./absent.mjs","./open.mjs"],"./format":["./legacy.cjs","./open.mjs"]}}"#,
            &[("open.mjs", "globalThis.executed = true; export const value = 42;"),
              ("legacy.cjs", "globalThis.executed = true;")]);
        let tool = crate::eval::EvalTool::new(dir.path());
        for spec in ["sealed/private/hidden", "sealed/exact", "sealed/missing", "sealed/format"] {
            let result = eval(&tool, &format!("await import({})", serde_json::to_string(spec).unwrap()));
            assert!(result.is_error, "{spec}");
        }
        assert_eq!(shown(eval(&tool, "display(typeof executed)")), "undefined");
        assert_eq!(shown(eval(&tool, "display((await import('sealed/public')).value)")), 42);
    }

    #[test]
    fn public_eval_reload_versions_pattern_maps_without_rebinding_old_functions() {
        let dir = tempfile::tempdir().unwrap();
        let directory = write_package(dir.path(), "versioned",
            r#"{"exports":{".":"./index.mjs","./feature/*":"./old/*.mjs"}}"#,
            &[("index.mjs", "export async function read() { return (await import('versioned/feature/value')).answer; }"),
              ("old/value.mjs", "export const answer = 41;"),
              ("new/value.mjs", "export const answer = 42;")]);
        let tool = crate::eval::EvalTool::new(dir.path());
        assert_eq!(shown(eval(&tool, "globalThis.saved = await import('versioned'); globalThis.keep = 7; display(await saved.read())")), 41);
        std::fs::write(directory.join("package.json"), r#"{"exports":{".":"./index.mjs","./feature/*":"./new/*.mjs"}}"#).unwrap();
        let result = eval(&tool, "reloadModules(); globalThis.fresh = await import('versioned'); display([await fresh.read(), await saved.read(), keep])");
        assert_eq!(shown(result), serde_json::json!([42,41,7]));
    }

    #[cfg(unix)]
    #[test]
    fn pattern_expansion_does_not_bypass_canonical_package_containment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("outside.mjs"), "globalThis.escape = true;").unwrap();
        let directory = write_package(dir.path(), "linked", r#"{"exports":{"./*":"./src/*.mjs"}}"#, &[]);
        std::fs::create_dir(directory.join("src")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside.mjs"), directory.join("src/escape.mjs")).unwrap();
        let tool = crate::eval::EvalTool::new(dir.path());
        assert!(eval(&tool, "await import('linked/escape')").is_error);
        assert_eq!(shown(eval(&tool, "display(typeof escape)")), "undefined");
    }
}
