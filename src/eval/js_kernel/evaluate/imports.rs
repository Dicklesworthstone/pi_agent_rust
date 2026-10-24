//! Static-import convenience syntax for retained scripts, not a second linker.
//!
//! SWC identifies declarations without executing them. QuickJS's ordinary
//! dynamic import still owns workspace admission, linking, caching and awaits.
//! Named/default bindings are immutable snapshots; namespace objects are live.
//! No wrapper surrounds the user's body and no helper binding escapes a cell.

use std::collections::BTreeSet;
use std::ops::Range;

use swc_common::{FileName, SourceMap, Span, Spanned, sync::Lrc};
use swc_ecma_ast::{
    Expr, ImportDecl, ImportPhase, ImportSpecifier, Lit, MetaPropExpr, MetaPropKind, ModuleDecl,
    ModuleExportName, ModuleItem, Prop, PropName, PropOrSpread, Stmt,
};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};
use swc_ecma_visit::{Visit, VisitWith};

const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_IMPORTS: usize = 128;
const MAX_BINDINGS: usize = 512;
const LIMIT: &str =
    "EVAL_IMPORT_LIMIT: static-import preparation exceeded its source, binding or output budget";
const INVALID: &str = "EVAL_IMPORT_SYNTAX: use ordinary static imports in a retained script; exports, import.meta and source/defer imports require a module file";

type Result<T> = std::result::Result<T, &'static str>;

fn quoted(value: &str) -> Result<String> {
    serde_json::to_string(value)
        .map(|value| {
            value
                .replace('\u{2028}', "\\u2028")
                .replace('\u{2029}', "\\u2029")
        })
        .map_err(|_| INVALID)
}

fn string_literal(value: &swc_ecma_ast::Str) -> Result<String> {
    // Never change a specifier or export name through lossy UTF-16 conversion.
    let value = value.value.clone().try_into_atom().map_err(|_| INVALID)?;
    quoted(value.as_ref())
}

fn range(span: Span, start: u32, code: &str) -> Result<Range<usize>> {
    let lo = usize::try_from(span.lo.0.checked_sub(start).ok_or(INVALID)?).map_err(|_| INVALID)?;
    let hi = usize::try_from(span.hi.0.checked_sub(start).ok_or(INVALID)?).map_err(|_| INVALID)?;
    if lo > hi || hi > code.len() || !code.is_char_boundary(lo) || !code.is_char_boundary(hi) {
        return Err(INVALID);
    }
    Ok(lo..hi)
}

fn append(output: &mut String, text: &str) -> Result<()> {
    if text.len() > MAX_OUTPUT_BYTES.saturating_sub(output.len()) {
        return Err(LIMIT);
    }
    output.push_str(text);
    Ok(())
}

fn attributes(declaration: &ImportDecl) -> Result<String> {
    let Some(attributes) = &declaration.with else {
        return Ok(String::new());
    };
    if attributes.props.is_empty() {
        return Ok(String::new());
    }
    if attributes.props.len() != 1 {
        return Err("EVAL_IMPORT_ATTRIBUTES: only one literal type attribute is supported");
    }
    let PropOrSpread::Prop(property) = &attributes.props[0] else {
        return Err(INVALID);
    };
    let Prop::KeyValue(property) = property.as_ref() else {
        return Err(INVALID);
    };
    let key = match &property.key {
        PropName::Ident(key) => quoted(key.sym.as_ref())?,
        PropName::Str(key) => string_literal(key)?,
        _ => return Err(INVALID),
    };
    let Expr::Lit(Lit::Str(value)) = property.value.as_ref() else {
        return Err(INVALID);
    };
    let value = string_literal(value)?;
    if key != "\"type\"" || !matches!(value.as_str(), "\"json\"" | "\"text\"") {
        return Err(
            "EVAL_IMPORT_ATTRIBUTES: type must be json or text; executable modules take no attributes",
        );
    }
    Ok(format!(",{{with:{{type:{value}}}}}"))
}

#[derive(Default)]
struct Plan {
    calls: String,
    pattern: String,
    values: String,
    names: BTreeSet<String>,
}

impl Plan {
    fn import(&mut self, declaration: &ImportDecl, index: usize) -> Result<()> {
        if declaration.phase != ImportPhase::Evaluation || declaration.type_only {
            return Err(INVALID);
        }
        let source = string_literal(&declaration.src)?;
        let attributes = attributes(declaration)?;
        append(
            &mut self.calls,
            &format!("const m{index}=await import({source}{attributes});"),
        )?;
        for specifier in &declaration.specifiers {
            if specifier.is_type_only() || self.names.len() >= MAX_BINDINGS {
                return Err(LIMIT);
            }
            let local = specifier.local().sym.to_string();
            if !self.names.insert(local.clone()) {
                return Err("EVAL_IMPORT_BINDING: duplicate imported local name");
            }
            let export = match specifier {
                ImportSpecifier::Default(_) => Some(quoted("default")?),
                ImportSpecifier::Namespace(_) => None,
                ImportSpecifier::Named(named) => Some(match &named.imported {
                    None => quoted(&local)?,
                    Some(ModuleExportName::Ident(name)) => quoted(name.sym.as_ref())?,
                    Some(ModuleExportName::Str(name)) => string_literal(name)?,
                }),
            };
            let value = if let Some(export) = export {
                append(
                    &mut self.calls,
                    &format!(
                        "if(!({export} in m{index}))throw 'EVAL_IMPORT_EXPORT: requested export is unavailable';"
                    ),
                )?;
                format!("m{index}[{export}]")
            } else {
                format!("m{index}")
            };
            let key = quoted(&local)?;
            // Computed keys retain an ordinary __proto__ binding. Object
            // destructuring avoids consulting the guest's Array iterator.
            append(&mut self.pattern, &format!("[{key}]:{local},"))?;
            append(&mut self.values, &format!("[{key}]:{value},"))?;
        }
        Ok(())
    }

    fn prefix(self) -> Result<String> {
        let mut prefix = String::new();
        append(&mut prefix, ";")?;
        if !self.names.is_empty() {
            append(&mut prefix, "const {bindings:{")?;
            append(&mut prefix, &self.pattern)?;
            append(&mut prefix, "}}=")?;
        }
        append(&mut prefix, "await(async()=>{")?;
        append(&mut prefix, &self.calls)?;
        if !self.names.is_empty() {
            // An imported local named `then` must not turn this internal
            // transfer into a thenable. Null prototypes also prevent a guest
            // Object.prototype.then from intercepting promise fulfillment.
            append(
                &mut prefix,
                "return {__proto__:null,bindings:{__proto__:null,",
            )?;
            append(&mut prefix, &self.values)?;
            append(&mut prefix, "}};")?;
        }
        append(&mut prefix, "})();")?;
        Ok(prefix)
    }
}

#[derive(Default)]
struct ImportMeta(bool);

impl Visit for ImportMeta {
    fn visit_meta_prop_expr(&mut self, expression: &MetaPropExpr) {
        self.0 |= expression.kind == MetaPropKind::ImportMeta;
    }
}

/// None preserves the original QuickJS grammar/error behavior for ordinary
/// scripts, dynamic imports and unparseable input. No source is evaluated here.
pub(super) fn prepare(code: &str) -> Result<Option<String>> {
    if code.len() > MAX_SOURCE_BYTES {
        return Err(LIMIT);
    }
    let cm: Lrc<SourceMap> = Lrc::new(SourceMap::default());
    let file = cm.new_source_file(
        FileName::Custom("eval imports".into()).into(),
        code.to_owned(),
    );
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax {
            import_attributes: true,
            ..Default::default()
        }),
        Default::default(),
        StringInput::from(&*file),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let Ok(module) = parser.parse_module() else {
        return Ok(None);
    };
    if !parser.take_errors().is_empty() {
        return Ok(None);
    }
    let imports = module
        .body
        .iter()
        .filter_map(|item| match item {
            ModuleItem::ModuleDecl(ModuleDecl::Import(declaration)) => Some(declaration),
            _ => None,
        })
        .collect::<Vec<_>>();
    if imports.is_empty() {
        return Ok(None);
    }
    if imports.len() > MAX_IMPORTS {
        return Err(LIMIT);
    }
    let mut import_meta = ImportMeta::default();
    module.visit_with(&mut import_meta);
    if import_meta.0 || module.body.iter().any(|item| {
        matches!(item, ModuleItem::ModuleDecl(declaration) if !matches!(declaration, ModuleDecl::Import(_)))
    }) { return Err(INVALID); }

    let mut insertion = if code.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    if code[insertion..].starts_with("#!") {
        insertion = code
            .find(['\r', '\n', '\u{2028}', '\u{2029}'])
            .map_or(code.len(), |index| {
                index
                    + if code[index..].starts_with("\r\n") {
                        2
                    } else {
                        code[index..].chars().next().map_or(0, char::len_utf8)
                    }
            });
    }
    // Keep the user's original directive prologue effective. All imported
    // modules finish before any non-directive body statement is evaluated.
    for item in &module.body {
        if let ModuleItem::Stmt(Stmt::Expr(statement)) = item
            && matches!(statement.expr.as_ref(), Expr::Lit(Lit::Str(_)))
        {
            insertion = range(statement.span(), file.start_pos.0, code)?.end;
        } else {
            break;
        }
    }
    let mut plan = Plan::default();
    let mut removed = Vec::with_capacity(imports.len());
    for (index, declaration) in imports.into_iter().enumerate() {
        plan.import(declaration, index)?;
        removed.push(range(declaration.span, file.start_pos.0, code)?);
    }
    let prefix = plan.prefix()?;
    if prefix.len() > MAX_OUTPUT_BYTES.saturating_sub(code.len()) {
        return Err(LIMIT);
    }
    // Preserve statement bytes and original newline locations; only import
    // declarations are blanked. Generated code has no physical newlines.
    let mut body = code.as_bytes().to_vec();
    for range in removed {
        for (offset, character) in code[range.clone()].char_indices() {
            if !matches!(character, '\r' | '\n' | '\u{2028}' | '\u{2029}') {
                body[range.start + offset..range.start + offset + character.len_utf8()].fill(b' ');
            }
        }
    }
    let mut body = String::from_utf8(body).map_err(|_| INVALID)?;
    body.insert_str(insertion, &prefix);
    Ok(Some(body))
}

#[cfg(test)]
mod tests;
