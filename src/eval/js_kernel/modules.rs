//! Local ES modules for the retained eval realm, not the extension loader.
//!
//! QuickJS owns linking, cycles, live bindings and evaluation caching. This
//! adapter owns file admission and the current cell's authority. No packages,
//! virtual modules, native libraries, remote URLs or implicit extension search.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{Ctx, Exception, Module};

use crate::agent_cx::AgentCx;
use crate::eval::lifecycle::source;

const MAX_PATH_BYTES: usize = 4096;
const MAX_RESOLUTIONS: usize = 4096;
const MAX_MODULES: usize = 128;
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

type Admission<T> = std::result::Result<T, String>;

struct Active {
    owner: AgentCx,
    deadline: Instant,
    resolutions: usize,
}

struct State {
    workspace: PathBuf,
    canonical_root: Option<PathBuf>,
    active: Option<Active>,
    stopped: Arc<AtomicBool>,
    approved: BTreeSet<String>,
    declarations: usize,
    source_bytes: usize,
}

impl State {
    fn check(&self) -> Admission<()> {
        let active = self.active.as_ref().ok_or("EVAL_MODULE_CLOSED: no active cell")?;
        if self.stopped.load(Ordering::SeqCst) || active.owner.checkpoint().is_err() {
            return Err("EVAL_CANCELLED: module owner stopped".into());
        }
        if Instant::now() >= active.deadline {
            return Err("EVAL_TIMEOUT: module load exceeded the cell budget".into());
        }
        if !active.owner.capabilities().io {
            return Err("EVAL_MODULE_DENIED: cell owner does not permit filesystem I/O".into());
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct ModuleFiles(Rc<RefCell<State>>);

impl ModuleFiles {
    pub(super) fn new(workspace: PathBuf, stopped: Arc<AtomicBool>) -> Self {
        Self(Rc::new(RefCell::new(State {
            workspace,
            canonical_root: None,
            active: None,
            stopped,
            approved: BTreeSet::new(),
            declarations: 0,
            source_bytes: 0,
        })))
    }

    pub(super) fn begin(&self, owner: AgentCx, deadline: Instant) {
        self.0.borrow_mut().active = Some(Active { owner, deadline, resolutions: 0 });
    }

    pub(super) fn finish(&self) {
        self.0.borrow_mut().active = None;
    }

    fn root(&self) -> Admission<PathBuf> {
        let workspace = {
            let state = self.0.borrow();
            state.check()?;
            if let Some(root) = &state.canonical_root {
                return Ok(root.clone());
            }
            state.workspace.clone()
        };
        let root = std::fs::canonicalize(workspace)
            .map_err(|error| format!("EVAL_MODULE_PATH: workspace unavailable: {error}"))?;
        let mut state = self.0.borrow_mut();
        state.check()?;
        state.canonical_root = Some(root.clone());
        Ok(root)
    }

    fn resolve_path(&self, base: &str, name: &str) -> Admission<String> {
        {
            let mut state = self.0.borrow_mut();
            state.check()?;
            let active = state.active.as_mut().expect("checked active owner");
            if active.resolutions >= MAX_RESOLUTIONS {
                return Err("EVAL_MODULE_LIMIT: too many resolutions in this cell".into());
            }
            active.resolutions += 1;
        }
        // This is a filesystem specifier, not a URL decoder or package name.
        // Requiring explicit relative spelling also rules out drive-relative
        // paths, UNC endpoints and every protocol on Windows and Unix alike.
        if !valid_spelling(name)
            || !(name.starts_with("./") || name.starts_with("../"))
            || !javascript_extension(Path::new(name))
        {
            return Err("EVAL_MODULE_SPECIFIER: use an explicit relative .js or .mjs path without URL/query/fragment syntax".into());
        }
        let root = self.root()?;
        let base_path = if base == "eval_script" {
            root.join("__pi_eval_cell__.js")
        } else {
            PathBuf::from(base)
        };
        if !base_path.is_absolute() || !base_path.starts_with(&root) {
            return Err("EVAL_MODULE_BASE: importer is not a workspace script".into());
        }
        let parent = base_path.parent().ok_or("EVAL_MODULE_BASE: importer has no directory")?;
        let resolved = std::fs::canonicalize(parent.join(name))
            .map_err(|error| format!("EVAL_MODULE_PATH: module unavailable: {error}"))?;
        self.0.borrow().check()?;
        if !resolved.starts_with(&root) || !javascript_extension(&resolved) {
            return Err("EVAL_MODULE_PATH: module must be a .js or .mjs file inside the workspace".into());
        }
        let resolved = resolved.to_str()
            .filter(|path| path.len() <= MAX_PATH_BYTES && !path.chars().any(char::is_control))
            .ok_or("EVAL_MODULE_PATH: module path is not bounded UTF-8")?.to_owned();
        let mut state = self.0.borrow_mut();
        if !state.approved.contains(&resolved) && state.approved.len() >= MAX_MODULES {
            return Err("EVAL_MODULE_LIMIT: module catalog exhausted; reset JavaScript to clear it".into());
        }
        state.approved.insert(resolved.clone());
        Ok(resolved)
    }

    fn read_module(&self, name: &str) -> Admission<source::Source> {
        {
            let mut state = self.0.borrow_mut();
            state.check()?;
            if !state.approved.contains(name) {
                return Err("EVAL_MODULE_PATH: module was not admitted by the resolver".into());
            }
            if state.declarations >= MAX_MODULES {
                return Err("EVAL_MODULE_LIMIT: module compilation budget exhausted; reset JavaScript".into());
            }
            // Failed attempts count too. QuickJS can ask again after a parse
            // failure; repeating it must not create an unbounded host loop.
            state.declarations += 1;
        }
        let root = self.root()?;
        let source = source::load_sync(&root, Path::new(name))
            .map_err(|error| format!("EVAL_MODULE_READ: {error}"))?;
        let mut state = self.0.borrow_mut();
        state.check()?;
        if source.path != name {
            return Err("EVAL_MODULE_CHANGED: module target changed after resolution".into());
        }
        let total = state.source_bytes.checked_add(source.code.len())
            .filter(|total| *total <= MAX_SOURCE_BYTES)
            .ok_or("EVAL_MODULE_LIMIT: realm source budget exceeds 16 MiB; reset JavaScript")?;
        state.source_bytes = total;
        Ok(source)
    }
}

fn valid_spelling(name: &str) -> bool {
    name.len() <= MAX_PATH_BYTES
        && !name.chars().any(|ch| ch.is_control() || matches!(ch, ':' | '?' | '#' | '\\'))
}

fn javascript_extension(path: &Path) -> bool {
    matches!(path.extension().and_then(|extension| extension.to_str()), Some("js" | "mjs"))
}

fn reject_attributes(ctx: &Ctx<'_>, attributes: Option<ImportAttributes<'_>>) -> rquickjs::Result<()> {
    if attributes.is_some_and(|attributes| attributes.keys().next().is_some()) {
        return Err(Exception::throw_message(ctx, "EVAL_MODULE_ATTRIBUTES: only JavaScript modules without import attributes are supported"));
    }
    Ok(())
}

impl Resolver for ModuleFiles {
    fn resolve<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        reject_attributes(ctx, attributes)?;
        self.resolve_path(base, name).map_err(|error| Exception::throw_message(ctx, &error))
    }
}

impl Loader for ModuleFiles {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        reject_attributes(ctx, attributes)?;
        let source = self.read_module(name).map_err(|error| Exception::throw_message(ctx, &error))?;
        // Linking can recursively invoke our callbacks. Do not retain a
        // RefCell borrow (or any open file) across the engine boundary.
        let module = Module::declare(ctx.clone(), name, source.code)?;
        let url = url::Url::from_file_path(name)
            .map_err(|()| Exception::throw_message(ctx, "EVAL_MODULE_PATH: cannot represent module file URL"))?;
        module.meta()?.set("url", url.as_str())?;
        Ok(module)
    }
}

#[cfg(test)]
mod tests;
