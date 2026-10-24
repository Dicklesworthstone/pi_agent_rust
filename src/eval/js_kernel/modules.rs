//! Local ES modules for the retained eval realm, not the extension loader.
//!
//! QuickJS owns linking, cycles, live bindings and evaluation caching. This
//! adapter owns file admission and the current cell's authority. JavaScript
//! and TypeScript modules share one linker/cache; TypeScript is lowered to
//! JavaScript before QuickJS sees it. JSON and text imports require their
//! explicit type and export native values, never executable source.
//! Workspace-installed ESM packages share that boundary. No native libraries,
//! remote URLs, package installation or implicit file-extension search.

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

mod data;
mod packages;
mod reload;
mod typescript;

const MAX_PATH_BYTES: usize = 4096;
const MAX_RESOLUTIONS: usize = 4096;
const MAX_MODULES: usize = 128;
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

type Admission<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModuleLanguage {
    JavaScript,
    TypeScript,
    Json,
    Text,
}

impl ModuleLanguage {
    const fn data_type(self) -> Option<&'static str> {
        match self {
            Self::Json => Some("json"),
            Self::Text => Some("text"),
            Self::JavaScript | Self::TypeScript => None,
        }
    }
}

struct Active {
    owner: AgentCx,
    deadline: Instant,
    resolutions: usize,
    reloaded: bool,
}

struct State {
    workspace: PathBuf,
    canonical_root: Option<PathBuf>,
    active: Option<Active>,
    stopped: Arc<AtomicBool>,
    approved: BTreeSet<String>,
    declarations: usize,
    source_bytes: usize,
    packages: packages::Catalog,
    versions: reload::Versions,
}

impl State {
    fn check(&self) -> Admission<()> {
        let active = self
            .active
            .as_ref()
            .ok_or("EVAL_MODULE_CLOSED: no active cell")?;
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
            packages: packages::Catalog::default(),
            versions: reload::Versions::default(),
        })))
    }

    pub(super) fn begin(&self, owner: AgentCx, deadline: Instant) {
        self.0.borrow_mut().active = Some(Active {
            owner,
            deadline,
            resolutions: 0,
            reloaded: false,
        });
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
        let relative = name.starts_with("./") || name.starts_with("../");
        // Package spelling has its own strict admission. Neither route is a
        // URL decoder, external directory search or implicit package installer.
        if !(valid_spelling(name) || packages::private_specifier(name))
            || relative && module_language(Path::new(name)).is_none()
        {
            return Err(
                "EVAL_MODULE_SPECIFIER: use an explicit relative module path or workspace-installed ESM package without URL/query/fragment syntax"
                    .into(),
            );
        }
        let root = self.root()?;
        // Native module identities can include a reload generation. Recover
        // importer paths only from our admitted table, never by stripping a
        // guest-supplied URL/query suffix. Old modules retain their graph.
        let (base_path, generation) = self.importer(base, &root);
        if !base_path.is_absolute() || !base_path.starts_with(&root) {
            return Err("EVAL_MODULE_BASE: importer is not a workspace script".into());
        }
        let parent = base_path
            .parent()
            .ok_or("EVAL_MODULE_BASE: importer has no directory")?;
        let (requested, package_root) = if relative {
            (parent.join(name), None)
        } else {
            let (path, package_root) = packages::resolve(self, &root, parent, name, generation)?;
            (path, Some(package_root))
        };
        let resolved = std::fs::canonicalize(&requested)
            .map_err(|error| format!("EVAL_MODULE_PATH: module unavailable: {error}"))?;
        self.0.borrow().check()?;
        if package_root
            .as_ref()
            .is_some_and(|root| !resolved.starts_with(root))
        {
            return Err(
                "EVAL_PACKAGE_PATH: exported target changed outside its package directory".into(),
            );
        }
        if !resolved.starts_with(&root) || module_language(&resolved).is_none() {
            return Err(
                "EVAL_MODULE_PATH: module must be a supported code or data file inside the workspace"
                    .into(),
            );
        }
        // Canonical aliases may share a JavaScript/TypeScript module, but
        // cannot turn a declared data import into executable source (or vice
        // versa), even when QuickJS already cached the canonical target.
        if module_language(&requested).and_then(ModuleLanguage::data_type)
            != module_language(&resolved).and_then(ModuleLanguage::data_type)
        {
            return Err("EVAL_MODULE_TYPE: module alias changes its data/code type".into());
        }
        packages::validate_javascript(self, &root, &resolved, generation)?;
        let resolved = resolved
            .to_str()
            .filter(|path| path.len() <= MAX_PATH_BYTES && !path.chars().any(char::is_control))
            .ok_or("EVAL_MODULE_PATH: module path is not bounded UTF-8")?
            .to_owned();
        let mut state = self.0.borrow_mut();
        let identity = reload::identity(&resolved, generation);
        if !state.approved.contains(&identity) && state.approved.len() >= MAX_MODULES {
            return Err(
                "EVAL_MODULE_LIMIT: module catalog exhausted; reset JavaScript to clear it".into(),
            );
        }
        state
            .versions
            .remember(identity.clone(), resolved, generation);
        state.approved.insert(identity.clone());
        Ok(identity)
    }

    fn read_module(&self, name: &str) -> Admission<source::Source> {
        {
            let mut state = self.0.borrow_mut();
            state.check()?;
            if !state.approved.contains(name) {
                return Err("EVAL_MODULE_PATH: module was not admitted by the resolver".into());
            }
            if state.declarations >= MAX_MODULES {
                return Err(
                    "EVAL_MODULE_LIMIT: module compilation budget exhausted; reset JavaScript"
                        .into(),
                );
            }
            // Failed attempts count too. QuickJS can ask again after a parse
            // failure; repeating it must not create an unbounded host loop.
            state.declarations += 1;
        }
        let path = self.module_path(name)?;
        let root = self.root()?;
        let loaded = if module_language(Path::new(&path)) == Some(ModuleLanguage::Text) {
            source::load_verbatim_sync(&root, Path::new(&path))
        } else {
            source::load_sync(&root, Path::new(&path))
        };
        let mut source = loaded.map_err(|error| format!("EVAL_MODULE_READ: {error}"))?;
        {
            let state = self.0.borrow();
            state.check()?;
        }
        if source.path != path {
            return Err("EVAL_MODULE_CHANGED: module target changed after resolution".into());
        }
        let original_bytes = source.code.len();
        if original_bytes > MAX_SOURCE_BYTES.saturating_sub(self.0.borrow().source_bytes) {
            return Err(
                "EVAL_MODULE_LIMIT: realm source budget exceeds 16 MiB; reset JavaScript".into(),
            );
        }
        if module_language(Path::new(&path)) == Some(ModuleLanguage::TypeScript) {
            // Reuse the same compiler as typed scripts, retaining native ESM
            // linking. No registry borrow crosses the compiler boundary.
            source.code = typescript::transpile(Path::new(&path), &source.code)?;
            self.0.borrow().check()?;
        }
        let mut state = self.0.borrow_mut();
        // Charge the larger representation: erased types cannot bypass input
        // budgets, and lowered enums cannot bypass generated-output budgets.
        let total = state
            .source_bytes
            .checked_add(source.code.len().max(original_bytes))
            .filter(|total| *total <= MAX_SOURCE_BYTES)
            .ok_or("EVAL_MODULE_LIMIT: realm source budget exceeds 16 MiB; reset JavaScript")?;
        state.source_bytes = total;
        Ok(source)
    }
}

fn valid_spelling(name: &str) -> bool {
    name.len() <= MAX_PATH_BYTES
        && !name
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, ':' | '?' | '#' | '\\'))
}

fn module_language(path: &Path) -> Option<ModuleLanguage> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("js" | "mjs") => Some(ModuleLanguage::JavaScript),
        Some("ts" | "mts") => Some(ModuleLanguage::TypeScript),
        Some("json") => Some(ModuleLanguage::Json),
        Some(
            "txt" | "md" | "csv" | "tsv" | "svg" | "html" | "css" | "xml" | "yaml" | "yml" | "toml"
            | "jsonl" | "ndjson",
        ) => Some(ModuleLanguage::Text),
        _ => None,
    }
}

fn validate_attributes(
    ctx: &Ctx<'_>,
    name: &str,
    attributes: Option<ImportAttributes<'_>>,
) -> rquickjs::Result<()> {
    if let Some(kind) = module_language(Path::new(name)).and_then(ModuleLanguage::data_type) {
        let valid = if let Some(attributes) = attributes {
            let mut keys = attributes.keys();
            keys.next().transpose()?.as_deref() == Some("type")
                && keys.next().is_none()
                && attributes.get_type()?.as_deref() == Some(kind)
        } else {
            false
        };
        return if valid {
            Ok(())
        } else {
            Err(Exception::throw_message(
                ctx,
                &format!(
                    "EVAL_MODULE_ATTRIBUTES: data imports require exactly with {{ type: '{kind}' }}"
                ),
            ))
        };
    }
    if attributes.is_some_and(|attributes| attributes.keys().next().is_some()) {
        return Err(Exception::throw_message(
            ctx,
            "EVAL_MODULE_ATTRIBUTES: only JavaScript/TypeScript modules without import attributes are supported",
        ));
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
        let resolved = self
            .resolve_path(base, name)
            .map_err(|error| Exception::throw_message(ctx, &error))?;
        // A package subpath can export data without a suffix in its public
        // name. Validate the actual target, including on cached resolution.
        let path = self
            .module_path(&resolved)
            .map_err(|error| Exception::throw_message(ctx, &error))?;
        validate_attributes(ctx, &path, attributes)?;
        Ok(resolved)
    }
}

impl Loader for ModuleFiles {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        let path = self
            .module_path(name)
            .map_err(|error| Exception::throw_message(ctx, &error))?;
        validate_attributes(ctx, &path, attributes)?;
        let source = self
            .read_module(name)
            .map_err(|error| Exception::throw_message(ctx, &error))?;
        // Linking can recursively invoke our callbacks. Do not retain a
        // RefCell borrow (or any open file) across the engine boundary.
        let module = match module_language(Path::new(&path)) {
            Some(ModuleLanguage::Json) => data::declare_json(ctx, name, &source.code)?,
            Some(ModuleLanguage::Text) => data::declare_text(ctx, name, &source.code)?,
            _ => Module::declare(ctx.clone(), name, source.code)?,
        };
        self.0
            .borrow()
            .check()
            .map_err(|error| Exception::throw_message(ctx, &error))?;
        let url = url::Url::from_file_path(&path).map_err(|()| {
            Exception::throw_message(ctx, "EVAL_MODULE_PATH: cannot represent module file URL")
        })?;
        module.meta()?.set("url", url.as_str())?;
        Ok(module)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod data_tests;

#[cfg(test)]
mod typescript_tests {
    use super::*;
    use std::time::Duration;

    fn files(root: &Path) -> ModuleFiles {
        let files = ModuleFiles::new(root.to_path_buf(), Arc::new(AtomicBool::new(false)));
        files.begin(
            AgentCx::for_testing_with_io(),
            Instant::now() + Duration::from_secs(10),
        );
        files
    }

    #[test]
    fn erased_types_cannot_bypass_the_original_source_budget() {
        let dir = tempfile::tempdir().unwrap();
        let fields = (0..100)
            .map(|index| format!("field{index}: number;"))
            .collect::<String>();
        let input = format!("interface Padding {{ {fields} }}\nexport const answer: number = 42;");
        std::fs::write(dir.path().join("types.ts"), &input).unwrap();
        let files = files(dir.path());
        let path = files.resolve_path("eval_script", "./types.ts").unwrap();
        files.0.borrow_mut().source_bytes = MAX_SOURCE_BYTES - input.len() + 1;
        assert!(
            files
                .read_module(&path)
                .err()
                .unwrap()
                .contains("EVAL_MODULE_LIMIT")
        );
        files.0.borrow_mut().source_bytes = MAX_SOURCE_BYTES - input.len();
        let loaded = files.read_module(&path).unwrap();
        assert!(
            loaded.code.len() < input.len(),
            "type declarations should be erased"
        );
        assert_eq!(files.0.borrow().source_bytes, MAX_SOURCE_BYTES);
    }

    #[test]
    fn enum_lowering_cannot_bypass_the_emitted_source_budget() {
        let dir = tempfile::tempdir().unwrap();
        let input = "export enum Code { A, B, C, D }";
        std::fs::write(dir.path().join("enum.ts"), input).unwrap();
        let files = files(dir.path());
        let path = files.resolve_path("eval_script", "./enum.ts").unwrap();
        files.0.borrow_mut().source_bytes = MAX_SOURCE_BYTES - input.len();
        assert!(
            files
                .read_module(&path)
                .err()
                .unwrap()
                .contains("EVAL_MODULE_LIMIT")
        );
        assert_eq!(
            files.0.borrow().source_bytes,
            MAX_SOURCE_BYTES - input.len()
        );
        files.0.borrow_mut().source_bytes = 0;
        let loaded = files.read_module(&path).unwrap();
        assert!(loaded.code.len() > input.len());
        assert_eq!(files.0.borrow().source_bytes, loaded.code.len());
    }
}
