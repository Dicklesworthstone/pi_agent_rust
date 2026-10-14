//! TypeScript-to-JavaScript lowering for eval's local ES-module loader.
//!
//! This is deliberately a syntax transform, not a package/runtime shim. SWC
//! removes TypeScript-only syntax while preserving ESM imports/exports for
//! QuickJS to link through the same workspace-scoped module loader.

use std::path::Path;

use swc_common::{FileName, Globals, Mark, SourceMap, GLOBALS, sync::Lrc};
use swc_ecma_ast::EsVersion;
use swc_ecma_codegen::to_code_default;
use swc_ecma_parser::{Syntax, TsSyntax, parse_file_as_program};
use swc_ecma_transforms_base::{fixer::fixer, resolver};
use swc_ecma_transforms_typescript::strip;

const MAX_GENERATED_BYTES: usize = 4 * 1024 * 1024;
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
    let cm: Lrc<SourceMap> = Default::default();
    let file = cm.new_source_file(
        FileName::Real(path.to_path_buf()).into(),
        source.to_owned(),
    );
    let mut recovered = Vec::new();
    let program = parse_file_as_program(
        &file,
        Syntax::Typescript(TsSyntax::default()),
        EsVersion::latest(),
        None,
        &mut recovered,
    )
    .map_err(|error| diagnostic("PARSE", &error))?;
    if let Some(error) = recovered.first() {
        return Err(diagnostic("PARSE", error));
    }

    let output = GLOBALS.set(&Globals::default(), || {
        let unresolved = Mark::new();
        let top_level = Mark::new();
        let program = program
            .apply(resolver(unresolved, top_level, false))
            .apply(strip(unresolved, top_level))
            .apply(fixer(None));
        to_code_default(cm, None, &program)
    });
    if output.len() > MAX_GENERATED_BYTES {
        return Err(
            "EVAL_TYPESCRIPT_LIMIT: generated JavaScript exceeds 4 MiB".to_string(),
        );
    }
    Ok(output)
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
