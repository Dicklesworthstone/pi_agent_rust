//! Data-only default exports over the same admitted file and realm lifetime.
//!
//! Parse using QuickJS's native JSON entrypoint, not a guest-replaceable
//! JSON.parse, generated JavaScript, object-literal evaluation or a host-wide
//! data registry. Text assets become length-delimited native strings, retaining
//! their BOM, CRLF and embedded NUL. Native module metadata holds each value
//! only until module evaluation transfers it to its default export.

use rquickjs::module::{Declarations, Declared, Exports, ModuleDef};
use rquickjs::{Ctx, Exception, Module, Undefined, Value};

const MAX_JSON_DEPTH: usize = 64;
const DATA_SLOT: &str = "pi.eval.module.data";

struct DataModule;

impl ModuleDef for DataModule {
    fn declare<'js>(declarations: &Declarations<'js>) -> rquickjs::Result<()> {
        declarations.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let metadata = exports.module().meta()?;
        let value: Value<'js> = metadata.get(DATA_SLOT)?;
        // Neither JSON nor a text string is undefined; initialization must fail,
        // not publish a default value from a partially declared module.
        if value.is_undefined() {
            return Err(Exception::throw_message(
                ctx,
                "EVAL_MODULE_DATA: module data unavailable",
            ));
        }
        exports.export("default", value)?;
        metadata.set(DATA_SLOT, Undefined)?;
        Ok(())
    }
}

/// Bound nesting before entering the engine parser. This scanner is only a
/// depth gate; QuickJS still validates the complete JSON grammar and retains
/// its native duplicate-key, number, key-order and __proto__ data semantics.
fn check_json_depth(source: &str) -> Result<(), &'static str> {
    let mut depth = 0_usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in source.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > MAX_JSON_DEPTH {
                        return Err("EVAL_MODULE_JSON_LIMIT: JSON nesting exceeds 64 containers");
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}

pub(super) fn declare_json<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    source: &str,
) -> rquickjs::Result<Module<'js, Declared>> {
    check_json_depth(source).map_err(|error| Exception::throw_message(ctx, error))?;
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let value = ctx.json_parse(source.as_bytes()).map_err(|error| {
        if error.is_exception() {
            // Parser diagnostics can quote file contents. Keep invalid data
            // out of the transcript, including when guest code catches it.
            let _ = ctx.catch();
        }
        Exception::throw_message(ctx, "EVAL_MODULE_JSON: file is not valid JSON")
    })?;
    declare_value(ctx, name, value)
}

pub(super) fn declare_text<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    source: &str,
) -> rquickjs::Result<Module<'js, Declared>> {
    // No source interpolation or guest String constructor. Arbitrary UTF-8
    // text, including script-looking content, remains inert string data.
    let value = rquickjs::String::from_str(ctx.clone(), source)?.into_value();
    declare_value(ctx, name, value)
}

fn declare_value<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    value: Value<'js>,
) -> rquickjs::Result<Module<'js, Declared>> {
    let module = Module::declare_def::<DataModule, _>(ctx.clone(), name)?;
    module.meta()?.set(DATA_SLOT, value)?;
    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting_is_bounded_without_counting_string_contents() {
        let accepted = format!(
            "{}0{}",
            "[".repeat(MAX_JSON_DEPTH),
            "]".repeat(MAX_JSON_DEPTH)
        );
        assert!(check_json_depth(&accepted).is_ok());
        assert!(check_json_depth(&format!("[{accepted}]")).is_err());
        let strings = serde_json::to_string(&vec!["[{}]".repeat(1000), "\\\"[".into()]).unwrap();
        assert!(check_json_depth(&strings).is_ok());
        assert!(check_json_depth(r#"{"quoted":"\\\"[{}]", "nested":[1]}"#).is_ok());
    }
}
