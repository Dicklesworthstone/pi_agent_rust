//! Explicit graph generations, without discarding the retained eval realm.
//!
//! QuickJS does not unload evaluated modules. New imports receive new native
//! identities, while existing namespaces, bindings and function origins stay
//! intact. All generations share the original realm admission budgets.

use super::{Admission, MAX_MODULES, MAX_SOURCE_BYTES, ModuleFiles};
use rquickjs::{Ctx, Exception, function::Func};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct Origin {
    path: String,
    generation: u32,
}

#[derive(Default)]
pub(super) struct Versions {
    generation: u32,
    // Exactly the admitted identities; bounded by MAX_MODULES across all
    // generations, not a second unbounded cache indexed by caller strings.
    origins: BTreeMap<String, Origin>,
}

impl Versions {
    pub(super) fn remember(&mut self, identity: String, path: String, generation: u32) {
        self.origins
            .entry(identity)
            .or_insert(Origin { path, generation });
    }
}

pub(super) fn identity(path: &str, generation: u32) -> String {
    // Namespace generation zero too: otherwise importing a file as a module
    // could capture the origin of a later %load script with that same path.
    // A colon-prefixed name cannot be an absolute filesystem path or pass the
    // public import-specifier validator. No user-controlled suffix is parsed.
    format!("pi-eval-module:{generation}:{path}")
}

impl ModuleFiles {
    pub(in crate::eval::js_kernel) fn install_reload(&self, ctx: &Ctx<'_>) -> rquickjs::Result<()> {
        // All module helpers capture the pristine realm before cells run.
        // This does not read files or acquire a cell's authority at install.
        self.install_commonjs(ctx)?;
        let files = self.clone();
        ctx.globals().set(
            "reloadModules",
            Func::from(move |ctx: Ctx<'_>| {
                files
                    .reload()
                    .map_err(|error| Exception::throw_message(&ctx, &error))
            }),
        )
    }

    pub(super) fn importer(&self, base: &str, root: &Path) -> (PathBuf, u32) {
        let state = self.0.borrow();
        if let Some(origin) = state.versions.origins.get(base) {
            (PathBuf::from(&origin.path), origin.generation)
        } else {
            let path = if base == "eval_script" {
                root.join("__pi_eval_cell__.js")
            } else {
                PathBuf::from(base)
            };
            (path, state.versions.generation)
        }
    }

    pub(super) fn module_path(&self, name: &str) -> Admission<String> {
        let state = self.0.borrow();
        state.check()?;
        state
            .versions
            .origins
            .get(name)
            .map(|origin| origin.path.clone())
            .ok_or_else(|| "EVAL_MODULE_PATH: unknown native module identity".to_owned())
    }

    pub(super) fn reload(&self) -> Admission<u32> {
        let mut state = self.0.borrow_mut();
        state.check()?;
        let active = state.active.as_ref().expect("checked active owner");
        // Never rotate a graph while linking/evaluating one of its modules.
        // Even an unsuccessful import consumes this cell's resolution fence.
        if active.resolutions != 0 || active.reloaded {
            return Err("EVAL_MODULE_RELOAD_ORDER: call reloadModules() once, before this cell's first import".into());
        }
        if state.approved.len() >= MAX_MODULES
            || state.declarations >= MAX_MODULES
            || state.source_bytes >= MAX_SOURCE_BYTES
        {
            return Err("EVAL_MODULE_LIMIT: reload cannot recover exhausted realm budgets; reset JavaScript".into());
        }
        let generation = state
            .versions
            .generation
            .checked_add(1)
            .ok_or("EVAL_MODULE_LIMIT: reload generation exhausted; reset JavaScript")?;
        state.versions.generation = generation;
        state
            .active
            .as_mut()
            .expect("checked active owner")
            .reloaded = true;
        Ok(generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_cx::AgentCx;
    use crate::eval::js_kernel::{JsCellResponse, JsKernel};
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::{Duration, Instant};

    fn files(root: &Path) -> ModuleFiles {
        let files = ModuleFiles::new(root.to_path_buf(), Arc::new(AtomicBool::new(false)));
        begin(&files);
        files
    }

    fn begin(files: &ModuleFiles) {
        files.begin(
            AgentCx::for_testing_with_io(),
            Instant::now() + Duration::from_secs(5),
        );
    }

    fn run(kernel: &JsKernel, code: &str) -> JsCellResponse {
        let owner = AgentCx::for_testing_with_io();
        let _current = owner.cx().clone().set_current_restricted();
        kernel
            .submit(code.into(), Instant::now() + Duration::from_secs(5))
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("cell must settle")
    }

    fn succeeds(kernel: &JsKernel, code: &str) -> String {
        let output = run(kernel, code);
        assert!(output.ok, "{code}: {:?}", output.error);
        output.result.unwrap_or_default()
    }

    #[test]
    fn reload_preserves_variables_old_namespaces_and_refreshes_transitive_code() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("leaf.mjs"),
            "export let n = 40; export function step() { return ++n; }",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("entry.mjs"),
            "export {n,step} from './leaf.mjs';",
        )
        .unwrap();
        let kernel = JsKernel::spawn_in(dir.path()).unwrap();
        succeeds(
            &kernel,
            "globalThis.keep = 7; globalThis.old = await import('./entry.mjs'); old.step()",
        );
        std::fs::write(
            dir.path().join("leaf.mjs"),
            "export let n = 90; export function step() { return ++n; }",
        )
        .unwrap();
        assert_eq!(succeeds(&kernel, "(await import('./entry.mjs')).n"), "41");
        succeeds(
            &kernel,
            "reloadModules(); globalThis.fresh = await import('./entry.mjs')",
        );
        assert_eq!(
            succeeds(&kernel, "[keep, old.n, fresh.n, old === fresh]"),
            "[7,41,90,false]"
        );
        assert_eq!(
            succeeds(&kernel, "old.step(); fresh.step(); [old.n,fresh.n]"),
            "[42,91]"
        );
        assert_eq!(
            succeeds(&kernel, "(await import('./entry.mjs')) === fresh"),
            "true"
        );
    }

    #[test]
    fn cycles_and_old_dynamic_importers_keep_their_native_graph_generation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mjs"), "import {b} from './b.mjs'; export function a(){return 40;} export const n = b(); export async function later(){return (await import('./b.mjs')).b();}").unwrap();
        std::fs::write(
            dir.path().join("b.mjs"),
            "import {a} from './a.mjs'; export function b(){return a()+2;}",
        )
        .unwrap();
        let kernel = JsKernel::spawn_in(dir.path()).unwrap();
        succeeds(&kernel, "globalThis.old = await import('./a.mjs')");
        std::fs::write(
            dir.path().join("b.mjs"),
            "import {a} from './a.mjs'; export function b(){return a()+9;}",
        )
        .unwrap();
        succeeds(
            &kernel,
            "reloadModules(); globalThis.fresh = await import('./a.mjs')",
        );
        assert_eq!(
            succeeds(&kernel, "[await old.later(), await fresh.later()]"),
            "[42,49]"
        );
    }

    #[test]
    fn malformed_reloaded_source_does_not_erase_working_values_and_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.mjs");
        std::fs::write(&path, "export const n = 42;").unwrap();
        let kernel = JsKernel::spawn_in(dir.path()).unwrap();
        succeeds(&kernel, "globalThis.old = await import('./data.mjs')");
        std::fs::write(&path, "globalThis.badPrefix = true; export const = 1;").unwrap();
        assert!(!run(&kernel, "reloadModules(); await import('./data.mjs')").ok);
        assert_eq!(
            succeeds(&kernel, "[old.n, typeof badPrefix]"),
            "[42,\"undefined\"]"
        );
        std::fs::write(&path, "export const n = 99;").unwrap();
        assert_eq!(
            succeeds(&kernel, "reloadModules(); (await import('./data.mjs')).n"),
            "99"
        );
    }

    #[test]
    fn reload_order_and_owner_failures_do_not_advance_generation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.mjs"), "export default 42;").unwrap();
        let files = files(dir.path());
        let original = files.resolve_path("eval_script", "./data.mjs").unwrap();
        assert!(files.reload().unwrap_err().contains("RELOAD_ORDER"));
        assert_eq!(files.0.borrow().versions.generation, 0);
        begin(&files);
        assert_eq!(files.reload().unwrap(), 1);
        assert!(files.reload().unwrap_err().contains("RELOAD_ORDER"));
        files.finish();
        assert!(files.reload().unwrap_err().contains("CLOSED"));
        files.begin(
            super::super::owner_without_io(),
            Instant::now() + Duration::from_secs(5),
        );
        assert!(files.reload().unwrap_err().contains("DENIED"));
        let owner = AgentCx::for_testing_with_io();
        owner.set_cancel_requested(true);
        files.begin(owner, Instant::now() + Duration::from_secs(5));
        assert!(files.reload().unwrap_err().contains("CANCELLED"));
        files.begin(AgentCx::for_testing_with_io(), Instant::now());
        assert!(files.reload().unwrap_err().contains("TIMEOUT"));
        assert_eq!(files.0.borrow().versions.generation, 1);
        begin(&files);
        assert!(files.read_module(&original).is_ok());
    }

    #[test]
    fn reload_cannot_bypass_identity_or_compiled_source_budgets() {
        let dir = tempfile::tempdir().unwrap();
        let input = "export default 42;";
        std::fs::write(dir.path().join("data.mjs"), input).unwrap();
        let files = files(dir.path());
        for generation in 0..MAX_MODULES {
            begin(&files);
            if generation != 0 {
                files.reload().unwrap();
            }
            let path = files.resolve_path("eval_script", "./data.mjs").unwrap();
            assert_eq!(files.read_module(&path).unwrap().code, input);
        }
        begin(&files);
        assert!(files.reload().unwrap_err().contains("LIMIT"));
        let state = files.0.borrow();
        assert_eq!(state.approved.len(), MAX_MODULES);
        assert_eq!(state.versions.origins.len(), MAX_MODULES);
        assert_eq!(state.declarations, MAX_MODULES);
        assert_eq!(state.source_bytes, MAX_MODULES * input.len());
        drop(state);
        let other = ModuleFiles::new(dir.path().to_path_buf(), Arc::new(AtomicBool::new(false)));
        begin(&other);
        other.0.borrow_mut().source_bytes = MAX_SOURCE_BYTES;
        assert!(other.reload().unwrap_err().contains("LIMIT"));
        assert_eq!(other.0.borrow().versions.generation, 0);
    }

    #[test]
    fn internal_identities_cannot_be_supplied_as_public_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.mjs"), "export default 42;").unwrap();
        let files = files(dir.path());
        files.reload().unwrap();
        let key = files.resolve_path("eval_script", "./data.mjs").unwrap();
        assert!(key.starts_with("pi-eval-module:1:"));
        assert!(
            files
                .resolve_path("eval_script", &key)
                .unwrap_err()
                .contains("SPECIFIER")
        );
        assert!(
            files
                .module_path("pi-eval-module:999:/outside.mjs")
                .is_err()
        );
        assert!(
            files
                .resolve_path("pi-eval-module:999:/outside.mjs", "./data.mjs")
                .unwrap_err()
                .contains("BASE")
        );
        assert_eq!(
            files.read_module(&key).unwrap().path,
            std::fs::canonicalize(dir.path().join("data.mjs"))
                .unwrap()
                .to_str()
                .unwrap()
        );
    }
}
