//! TypeScript preparation using the project's existing SWC stack.
//!
//! This is deliberately separate from the extension module loader: compiled
//! cells keep the eval realm, globals and host policy. This compiler does not
//! fetch modules or execute guest code; native linking remains owned by QuickJS.

use super::failure;
use crate::error::{Error, Result};
use std::io::Write;
use std::path::Path;
use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, Span, Spanned};
use swc_common::{comments::SingleThreadedComments, sync::Lrc};
use swc_ecma_ast::{
    Decorator, EsVersion, MetaPropExpr, MetaPropKind, ModuleDecl, ModuleItem, Program, Script,
    TsExportAssignment, TsImportEqualsDecl, TsType,
};
use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_transforms_base::{fixer::fixer, hygiene::hygiene, resolver};
use swc_ecma_transforms_typescript::{Config, typescript};
use swc_ecma_visit::{Visit, VisitWith};

// TypeScript runtime lowering (enums/namespaces/parameter properties) can
// expand a bounded input. Cap code generation itself, not just its final Vec.
const MAX_EMITTED_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Script,
    Module,
}

pub(super) fn prepare(path: &Path, code: String) -> Result<String> {
    prepare_kind(path, code, Kind::Script)
}

pub(in crate::eval) fn prepare_module(path: &Path, code: String) -> Result<String> {
    prepare_kind(path, code, Kind::Module)
}

fn prepare_kind(path: &Path, code: String, kind: Kind) -> Result<String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if ["tsx", "cts"]
        .iter()
        .any(|suffix| extension.eq_ignore_ascii_case(suffix))
    {
        return Err(failure(
            "UNSUPPORTED",
            "TSX and CommonJS TypeScript are not supported",
        ));
    }
    if extension.eq_ignore_ascii_case("mts") && kind == Kind::Script {
        return Err(failure(
            "UNSUPPORTED",
            "use await import('./file.mts') for a TypeScript ES module",
        ));
    }
    if !extension.eq_ignore_ascii_case("ts") && !extension.eq_ignore_ascii_case("mts") {
        return Ok(code);
    }
    if path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.ends_with(".d.ts") || name.ends_with(".d.mts")
        })
    {
        return Err(failure(
            "UNSUPPORTED",
            "a TypeScript declaration file is not executable",
        ));
    }
    compile(path, code, kind)
}

fn at(cm: &SourceMap, path: &Path, span: Span, message: impl std::fmt::Display) -> Error {
    let location = cm.lookup_char_pos(span.lo);
    failure(
        "TYPESCRIPT",
        format!(
            "{}:{}:{}: {message}",
            path.display(),
            location.line,
            location.col_display + 1,
        ),
    )
}

#[allow(clippy::too_many_lines)] // one SWC pipeline: parse, screen, strip types, emit
fn compile(path: &Path, code: String, kind: Kind) -> Result<String> {
    let cm: Lrc<SourceMap> = Lrc::new(SourceMap::default());
    let file = cm.new_source_file(FileName::Real(path.to_path_buf()).into(), code);
    let comments = SingleThreadedComments::default();
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: false,
            decorators: false,
            ..Default::default()
        }),
        EsVersion::default(),
        StringInput::from(&*file),
        Some(&comments),
    );
    let mut parser = Parser::new_from(lexer);
    // Keep imports in module grammar while erasing types. The retained-script
    // evaluator lowers the remaining value imports through its native loader;
    // actual imported modules keep their full ESM semantics.
    let module = parser
        .parse_module()
        .map_err(|error| at(&cm, path, error.span(), format_args!("{:?}", error.kind())))?;
    if let Some(error) = parser.take_errors().into_iter().next() {
        return Err(at(
            &cm,
            path,
            error.span(),
            format_args!("{:?}", error.kind()),
        ));
    }
    let program = match kind {
        Kind::Module => Program::Module(module),
        Kind::Script
            if module
                .body
                .iter()
                .any(|item| matches!(item, ModuleItem::ModuleDecl(ModuleDecl::Import(_)))) =>
        {
            for item in &module.body {
                if let ModuleItem::ModuleDecl(declaration) = item
                    && !matches!(declaration, ModuleDecl::Import(_))
                {
                    return Err(at(
                        &cm,
                        path,
                        declaration.span(),
                        "exports require an imported module file, not a %load script",
                    ));
                }
            }
            Program::Module(module)
        }
        Kind::Script => {
            let mut statements = Vec::with_capacity(module.body.len());
            for item in module.body {
                match item {
                    ModuleItem::Stmt(statement) => statements.push(statement),
                    ModuleItem::ModuleDecl(declaration) => {
                        return Err(at(
                            &cm,
                            path,
                            declaration.span(),
                            "exports require an imported module file, not a %load script",
                        ));
                    }
                }
            }
            Program::Script(Script {
                span: module.span,
                body: statements,
                shebang: module.shebang,
            })
        }
    };
    let mut unsupported = Unsupported {
        first: None,
        module: kind == Kind::Module,
    };
    program.visit_with(&mut unsupported);
    if let Some((span, message)) = unsupported.first {
        return Err(at(&cm, path, span, message));
    }
    GLOBALS.set(&Globals::default(), || {
        let unresolved = Mark::new();
        let top_level = Mark::new();
        // Preserve value imports even when their bindings are unused: loading
        // that module can have side effects. Only explicit type imports vanish.
        // Retained cells can mutate enum objects; never inline their old values.
        let config = Config {
            verbatim_module_syntax: true,
            native_class_properties: true,
            ts_enum_is_mutable: true,
            // Erasing a type-only import must not synthesize export {} in a
            // retained script. Genuine imported modules retain that marker.
            no_empty_export: kind == Kind::Script,
            ..Default::default()
        };
        let program = program
            .apply(resolver(unresolved, top_level, true))
            .apply(typescript(config, unresolved, top_level))
            .apply(hygiene())
            .apply(fixer(Some(&comments)));
        let mut generated = BoundedCode(Vec::new());
        {
            let mut emitter = Emitter {
                cfg: swc_ecma_codegen::Config::default(),
                cm: cm.clone(),
                comments: Some(&comments),
                wr: JsWriter::new(cm.clone(), "\n", &mut generated, None),
            };
            emitter
                .emit_program(&program)
                .map_err(|error| failure("TYPESCRIPT", error))?;
        }
        String::from_utf8(generated.0)
            .map_err(|_| failure("ENCODING", "compiler output was not UTF-8"))
    })
}

struct Unsupported {
    first: Option<(Span, &'static str)>,
    module: bool,
}

impl Unsupported {
    const fn record(&mut self, span: Span, message: &'static str) {
        if self.first.is_none() {
            self.first = Some((span, message));
        }
    }
}

impl Visit for Unsupported {
    fn visit_meta_prop_expr(&mut self, expression: &MetaPropExpr) {
        if expression.kind == MetaPropKind::ImportMeta && !self.module {
            self.record(
                expression.span,
                "import.meta is not available in eval scripts",
            );
        }
    }

    fn visit_decorator(&mut self, decorator: &Decorator) {
        self.record(
            decorator.span,
            "decorator transforms are not enabled for eval scripts",
        );
    }

    fn visit_ts_import_equals_decl(&mut self, declaration: &TsImportEqualsDecl) {
        self.record(
            declaration.span,
            "CommonJS TypeScript import assignments are not supported",
        );
    }

    fn visit_ts_export_assignment(&mut self, declaration: &TsExportAssignment) {
        self.record(
            declaration.span,
            "CommonJS TypeScript export assignments are not supported",
        );
    }

    fn visit_ts_type(&mut self, _annotation: &TsType) {
        // Erased type queries (including import types) do not load a module.
        // There is no type checking or dependency resolution in this pass.
    }
}

struct BoundedCode(Vec<u8>);

impl Write for BoundedCode {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_EMITTED_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "EVAL_LOAD_LIMIT: emitted JavaScript exceeds 4 MiB",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transpile(code: &str) -> String {
        prepare(Path::new("input.ts"), code.into()).unwrap()
    }

    #[test]
    fn typed_functions_enums_and_parameter_properties_execute_as_javascript() {
        let code = transpile(
            r"
            interface Named { value: number }
            type Identity<T> = T;
            enum Offset { First = 40, Second }
            class Boxed { constructor(public value: number) {} }
            function identity<T>(value: T): T { return value; }
            const answer: Identity<Named> = new Boxed(identity(Offset.Second + 1));
            answer.value
        ",
        );
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| {
            assert_eq!(ctx.eval::<i32, _>(code.as_bytes()).unwrap(), 42);
            assert_eq!(ctx.eval::<i32, _>("answer.value").unwrap(), 42);
        });
    }

    #[test]
    fn ordinary_js_bytes_and_keyword_strings_are_not_rewritten() {
        let js = "// interface\nconst text = 'value: number; import'; text";
        assert_eq!(prepare(Path::new("input.js"), js.into()).unwrap(), js);
        let ts = transpile("const text: string = 'interface X { type: number }'; text");
        assert!(ts.contains("interface X { type: number }"));
        assert!(prepare(Path::new("input.TS"), "const x: number = 1".into()).is_ok());
    }

    #[test]
    fn unsupported_script_module_syntax_and_transforms_are_rejected_before_execution() {
        for source in [
            "export const answer: number = 42",
            "export {}; 42",
            "import answer = require('./never.js'); 42",
            "const location = import.meta.url; 42",
            "@decorate class Thing {}",
        ] {
            assert!(
                prepare(Path::new("input.ts"), source.into()).is_err(),
                "{source}"
            );
        }
        for path in [
            "input.d.ts",
            "input.D.TS",
            "input.tsx",
            "input.mts",
            "input.cts",
        ] {
            assert!(prepare(Path::new(path), "42".into()).is_err(), "{path}");
        }
    }

    #[test]
    fn loaded_scripts_preserve_unused_value_imports_for_native_evaluation() {
        let code = transpile(
            r"
            import { unused } from './side-effect.ts';
            import type { Missing } from './missing.ts';
            const answer: number = 42;
            answer
        ",
        );
        assert!(code.contains("./side-effect.ts"));
        assert!(!code.contains("./missing.ts"));
        assert!(!code.contains("export"));
        assert!(code.contains("const answer = 42"));
    }

    #[test]
    fn erased_type_only_imports_do_not_load_files_or_leave_module_markers() {
        let code = transpile(
            "import type {Missing} from './never-created.ts'; const answer: number=42; answer",
        );
        assert!(!code.contains("import"));
        assert!(!code.contains("export"));
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| assert_eq!(ctx.eval::<i32, _>(code.as_bytes()).unwrap(), 42));
    }

    #[test]
    fn loaded_imports_keep_attributes_but_do_not_authorize_exports_or_import_meta() {
        let code = transpile(
            "import rows from './rows.json' with {type:'json'}; import template from './template.md' with {type:'text'}; const value: number=rows.length; value",
        );
        assert!(code.contains("./rows.json"));
        assert!(code.contains("./template.md"));
        assert_eq!(code.matches("with {").count(), 2);
        for source in [
            "import './one.ts'; export const answer=42;",
            "import './one.ts'; export {};",
            "import './one.ts'; const origin=import.meta.url;",
            "import './one.ts'; import answer=require('./two.ts');",
            "import './one.ts'; const broken: = 1;",
        ] {
            assert!(
                prepare(Path::new("input.ts"), source.into()).is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn modules_preserve_unused_value_imports_and_erase_only_explicit_type_imports() {
        let code = prepare_module(
            Path::new("input.ts"),
            r"
            import { unused } from './side-effect.ts';
            import type { Missing } from './missing.ts';
            export const answer: number = 42;
            export const origin = import.meta.url;
        "
            .into(),
        )
        .unwrap();
        assert!(code.contains("./side-effect.ts"));
        assert!(!code.contains("./missing.ts"));
        assert!(code.contains("export"));
        assert!(code.contains("import.meta.url"));
        assert!(
            prepare_module(
                Path::new("input.mts"),
                "export const value: number = 1".into()
            )
            .is_ok()
        );
        assert!(prepare_module(Path::new("input.ts"), "export = 42".into()).is_err());
        assert!(
            prepare_module(
                Path::new("input.d.mts"),
                "export declare const value: number".into()
            )
            .is_err()
        );
    }

    #[test]
    fn enum_mutations_are_not_replaced_by_stale_compile_time_constants() {
        let code = transpile(
            "enum Code { Value = 1 }; (Code as any).Value = 42; class Boxed { value = Code.Value; }; new Boxed().value",
        );
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| assert_eq!(ctx.eval::<i32, _>(code.as_bytes()).unwrap(), 42));
    }

    #[test]
    fn parse_errors_include_source_location_and_recovered_errors_are_fatal() {
        for source in [
            "let value: number = ;",
            "const value: number;",
            "const value = <div />;",
        ] {
            let error = prepare(Path::new("analysis/bad.ts"), source.into()).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("EVAL_LOAD_TYPESCRIPT"), "{message}");
            assert!(message.contains("bad.ts"), "{message}");
        }
    }

    #[test]
    fn code_generation_limit_refuses_overflow_without_partial_append() {
        let mut output = BoundedCode(vec![b'x'; MAX_EMITTED_BYTES - 1]);
        output.write_all(b"y").unwrap();
        let error = output.write_all(b"z").unwrap_err();
        assert!(error.to_string().contains("EVAL_LOAD_LIMIT"));
        assert_eq!(output.0.len(), MAX_EMITTED_BYTES);
        assert_eq!(output.0.last(), Some(&b'y'));
    }
}
