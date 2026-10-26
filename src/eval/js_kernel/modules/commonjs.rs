//! Synchronous CommonJS dependencies inside the existing retained JS realm.
//!
//! The JavaScript closure owns only module values. Every resolution and read
//! is admitted by ModuleFiles under the active cell, including cached edges.
//! ESM imports get a native default export, evaluated after linking, not a
//! generated source string holding serialized exports or a separate Node VM.

use std::path::Path;

use rquickjs::module::{Declarations, Declared, Exports, ModuleDef};
use rquickjs::{Ctx, Exception, Function, Module, Undefined, Value};

use super::{Admission, ModuleFiles, ModuleLanguage, data, module_language};

const ENGINE_SLOT: &str = "__pi_eval_commonjs_loader__";
const ID_SLOT: &str = "pi.eval.commonjs.id";
const LOADER_SLOT: &str = "pi.eval.commonjs.loader";

struct CommonJsModule;

impl ModuleDef for CommonJsModule {
    fn declare(declarations: &Declarations<'_>) -> rquickjs::Result<()> {
        declarations.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(_ctx: &Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let metadata = exports.module().meta()?;
        let identity: String = metadata.get(ID_SLOT)?;
        let loader: Function<'js> = metadata.get(LOADER_SLOT)?;
        let value: Value<'js> = loader.call((identity,))?;
        // Undefined, null, false and callable module.exports are all valid.
        exports.export("default", value)?;
        metadata.set(ID_SLOT, Undefined)?;
        metadata.set(LOADER_SLOT, Undefined)?;
        Ok(())
    }
}

pub(super) fn declare<'js>(
    ctx: &Ctx<'js>,
    name: &str,
) -> rquickjs::Result<Module<'js, Declared>> {
    let loader: Function<'js> = ctx.globals().get(ENGINE_SLOT)?;
    let module = Module::declare_def::<CommonJsModule, _>(ctx.clone(), name)?;
    let metadata = module.meta()?;
    metadata.set(ID_SLOT, name)?;
    metadata.set(LOADER_SLOT, loader)?;
    Ok(module)
}

impl ModuleFiles {
    pub(super) fn install_commonjs(&self, ctx: &Ctx<'_>) -> rquickjs::Result<()> {
        let files = self.clone();
        let host = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, operation: String, base: String, name: String| {
                files
                    .commonjs_request(&operation, &base, &name)
                    .map_err(|error| Exception::throw_message(&ctx, &error))
            },
        )?;
        let factory: Function<'_> = ctx.eval(include_str!("commonjs.js"))?;
        let _: Function<'_> = factory.call((host, ENGINE_SLOT))?;
        Ok(())
    }

    fn commonjs_request(&self, operation: &str, base: &str, name: &str) -> Admission<String> {
        // Base identities come only from our admitted origin table. A guessed
        // absolute path or an old cell cannot manufacture module read authority.
        let path = self.module_path(base)?;
        match operation {
            "check" => Ok(String::new()),
            "path" => Ok(path),
            "resolve" => self.resolve_commonjs(base, name),
            "read" => {
                let kind = match module_language(Path::new(&path)) {
                    Some(ModuleLanguage::CommonJs) => "commonjs",
                    Some(ModuleLanguage::Json) => "json",
                    _ => {
                        return Err(
                            "EVAL_COMMONJS_FORMAT: require accepts CommonJS or JSON, not ESM".into(),
                        );
                    }
                };
                let source = self.read_module(base)?;
                if kind == "json" {
                    data::check_json_depth(&source.code).map_err(str::to_owned)?;
                }
                let directory = Path::new(&path)
                    .parent()
                    .and_then(Path::to_str)
                    .ok_or("EVAL_MODULE_PATH: module has no UTF-8 parent directory")?
                    .to_owned();
                let response = serde_json::json!({
                    "path": path,
                    "directory": directory,
                    "kind": kind,
                    "source": source.code,
                });
                self.0.borrow().check()?;
                serde_json::to_string(&response)
                    .map_err(|_| "EVAL_COMMONJS_SOURCE: cannot encode admitted source".into())
            }
            _ => Err("EVAL_COMMONJS_PROTOCOL: unsupported loader operation".into()),
        }
    }

    fn resolve_commonjs(&self, base: &str, name: &str) -> Admission<String> {
        if !(name.starts_with("./") || name.starts_with("../")) {
            return Err("EVAL_COMMONJS_SPECIFIER: require needs an explicit relative .cjs or .json path; Node built-ins and remote modules are unavailable".into());
        }
        let identity = self.resolve_path(base, name)?;
        let path = self.module_path(&identity)?;
        if !matches!(
            module_language(Path::new(&path)),
            Some(ModuleLanguage::CommonJs | ModuleLanguage::Json)
        ) {
            return Err("EVAL_COMMONJS_FORMAT: synchronous require cannot load an ES module".into());
        }
        Ok(identity)
    }
}

#[cfg(test)]
mod tests;
