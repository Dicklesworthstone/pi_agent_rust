//! Module admission for eval's shared TypeScript compiler.
//!
//! This is deliberately a syntax transform, not a package/runtime shim. SWC
//! Script preflight and module compilation use the same SWC pipeline. This
//! boundary retains module diagnostics and does not load an extension realm.

use std::path::Path;

use crate::eval::lifecycle::source;

const MAX_DIAGNOSTIC_BYTES: usize = 1024;

fn diagnostic(label: &str, value: &impl std::fmt::Debug) -> String {
    let mut detail = format!("{value:?}");
    if detail.len() > MAX_DIAGNOSTIC_BYTES {
        let mut end = MAX_DIAGNOSTIC_BYTES;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        detail.push_str("...");
    }
    format!("EVAL_TYPESCRIPT_{label}: {detail}")
}

/// Strip TypeScript syntax from one already-admitted workspace module.
///
/// TSX is intentionally not accepted by the resolver, so JSX lowering is
/// not part of this transform. Recovered parser errors are failures rather than
/// best-effort output: eval must not execute a prefix from malformed source.
pub(super) fn transpile(path: &Path, source: &str) -> Result<String, String> {
    source::prepare_module(path, source.to_owned()).map_err(|error| {
        let label = if error.to_string().contains("EVAL_LOAD_LIMIT") {
            "LIMIT"
        } else {
            "PARSE"
        };
        diagnostic(label, &error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_types_and_type_only_imports_without_erasing_runtime_esm() {
        let output = transpile(
            Path::new("/workspace/value.ts"),
            "import type {Shape} from './shape.ts';\nimport {base} from './base.ts';\nconst value: Shape = {answer: base + 2};\nexport const answer: number = value.answer;",
        )
        .unwrap();
        assert!(!output.contains("import type"));
        assert!(!output.contains(": Shape"));
        assert!(!output.contains(": number"));
        assert!(output.contains("./base.ts"));
        assert!(output.contains("export const answer"));
    }

    #[test]
    fn lowers_typescript_enums_into_executable_javascript() {
        let output = transpile(
            Path::new("/workspace/level.mts"),
            "enum Level { Low = 40, High = 42 }\nexport const answer: number = Level.High;",
        )
        .unwrap();
        assert!(!output.contains(": number"));
        assert!(output.contains("Level"));
        assert!(output.contains("42"));
    }

    #[test]
    fn malformed_typescript_is_rejected_before_quickjs_execution() {
        let error = transpile(
            Path::new("/workspace/bad.ts"),
            "globalThis.executed = true; export const broken: = 1;",
        )
        .unwrap_err();
        assert!(error.starts_with("EVAL_TYPESCRIPT_PARSE:"));
    }

    #[test]
    fn tsx_is_not_implicitly_enabled_by_the_transpiler() {
        let error = transpile(
            Path::new("/workspace/view.ts"),
            "export const view = <div>no implicit JSX</div>;",
        )
        .unwrap_err();
        assert!(error.starts_with("EVAL_TYPESCRIPT_PARSE:"));
    }
}
