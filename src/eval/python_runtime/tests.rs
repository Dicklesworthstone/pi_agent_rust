use super::*;
use crate::eval::EvalTool;
use crate::model::ContentBlock;
use crate::tools::{Tool, ToolOutput};
use serde_json::json;

fn environment(path: &Path) -> Environment {
    Environment {
        path: Some(path.as_os_str().into()),
        virtual_env: None,
        conda_prefix: None,
        home: None,
        empty_override: false,
    }
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8).build().unwrap().block_on(future)
}

fn executable(root: &Path, conda: bool) -> PathBuf {
    if cfg!(windows) {
        if conda { root.join("python.exe") } else { root.join("Scripts/python.exe") }
    } else {
        root.join("bin/python")
    }
}

#[test]
fn discovery_order_is_explicit_and_does_not_canonicalize_venv_entries() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let mut env = environment(&cwd.join("system"));
    env.virtual_env = Some("active".into());
    env.conda_prefix = Some("conda".into());
    let plan = env.candidates(OsStr::new(""), cwd).unwrap();
    let mut sources: Vec<_> = plan.iter().map(|candidate| candidate.source).collect();
    sources.dedup();
    assert_eq!(sources, [Source::ActiveVenv, Source::Conda, Source::DotVenv, Source::Venv, Source::Path]);
    assert_eq!(plan[0].executable, executable(&cwd.join("active"), false));
    assert!(plan.iter().any(|candidate| candidate.executable == executable(&cwd.join("conda"), true)));
    // Selection cannot require an environment to exist while constructing a
    // registry: it may be created before that session's first Python cell.
    assert!(!plan[0].executable.exists());
}

#[test]
fn explicit_paths_names_and_home_expansion_never_fall_back_to_other_sources() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let mut env = environment(&cwd.join("bin"));
    env.home = Some(cwd.join("home"));
    env.virtual_env = Some(cwd.join("active").into_os_string());
    for (requested, expected) in [
        (OsString::from("./python with spaces"), cwd.join("./python with spaces")),
        (OsString::from("~/bin/python"), cwd.join("home/bin/python")),
        (cwd.join("absolute").into_os_string(), cwd.join("absolute")),
    ] {
        let plan = env.candidates(&requested, cwd).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].source, Source::Explicit);
        assert_eq!(plan[0].executable, expected);
    }
    let plan = env.candidates(OsStr::new("custom-python"), cwd).unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].source, Source::Explicit);
    assert_eq!(plan[0].executable, cwd.join(if cfg!(windows) { "bin/custom-python.exe" } else { "bin/custom-python" }));
}

#[test]
fn candidate_dedup_and_path_search_do_not_adopt_relative_or_empty_entries() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let bin = cwd.join("system");
    let mut env = environment(&bin);
    env.path = Some(std::env::join_paths([Path::new(""), Path::new("."), Path::new("relative"), &bin, &bin]).unwrap());
    env.virtual_env = Some(cwd.join(".venv").into_os_string());
    let plan = env.candidates(OsStr::new(""), cwd).unwrap();
    let defaults: Vec<_> = plan.iter().filter(|candidate| candidate.source == Source::Path).collect();
    assert_eq!(defaults.len(), 2);
    assert!(defaults.iter().all(|candidate| candidate.executable.parent() == Some(bin.as_path())));
    // The project .venv cannot be reprobed through its second spelling/source.
    assert!(!plan.iter().any(|candidate| candidate.source == Source::DotVenv));
}

#[test]
fn empty_override_bad_paths_and_excessive_discovery_fail_before_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let mut env = environment(cwd);
    env.empty_override = true;
    assert!(env.candidates(OsStr::new(""), cwd).unwrap_err().to_string().contains("EVAL_PY_INPUT"));
    env.empty_override = false;
    assert!(env.candidates(OsStr::new("./bad\0name"), cwd).is_err());
    assert!(env.candidates(OsStr::new("~/python"), cwd).is_err());
    env.path = Some(std::env::join_paths((0..MAX_CANDIDATES).map(|i| cwd.join(format!("bin-{i}")))).unwrap());
    assert!(env.candidates(OsStr::new(""), cwd).unwrap_err().to_string().contains("128"));
}

#[test]
fn command_environment_matches_the_selected_venv_and_never_mutates_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let env = environment(&cwd.join("system"));
    let plan = env.candidates(OsStr::new(""), cwd).unwrap();
    let chosen = &plan[0];
    let command = chosen.command();
    let entries: std::collections::BTreeMap<_, _> = command.get_envs().collect();
    assert_eq!(entries[OsStr::new("VIRTUAL_ENV")], Some(cwd.join(".venv").as_os_str()));
    assert_eq!(entries[OsStr::new("PYTHONHOME")], None);
    assert_eq!(entries[OsStr::new("CONDA_PREFIX")], None);
    assert_eq!(entries[OsStr::new("PYTHONIOENCODING")], Some(OsStr::new("utf-8")));
    assert_eq!(std::env::split_paths(entries[OsStr::new("PATH")].unwrap()).next().unwrap(), chosen.executable.parent().unwrap());
    assert_eq!(command.get_program(), chosen.executable.as_os_str());
    assert_eq!(command.get_args().count(), 0);
}

fn python() -> Option<PathBuf> {
    let selected = std::env::var_os("PI_EVAL_PYTHON").unwrap_or_else(|| "python3".into());
    match Command::new(selected).args(["-c", "import sys; print(sys.executable)"]).output() {
        Ok(output) => {
            assert!(output.status.success(), "Python fixture discovery failed");
            Some(PathBuf::from(String::from_utf8(output.stdout).unwrap().trim()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("SKIP: Python fixture interpreter unavailable");
            None
        }
        Err(error) => panic!("Python fixture discovery: {error}"),
    }
}

fn make_venv(python: &Path, root: &Path, value: u64) -> PathBuf {
    let setup = Command::new(python).args(["-m", "venv", "--without-pip"]).arg(root).output().unwrap();
    assert!(setup.status.success(), "venv fixture failed: {}", String::from_utf8_lossy(&setup.stderr));
    let interpreter = executable(root, false);
    let output = Command::new(&interpreter).args(["-c", "import sysconfig; print(sysconfig.get_path('purelib'))"]).output().unwrap();
    assert!(output.status.success());
    let site = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    std::fs::write(site.join("pi_runtime_fixture.py"), format!("value = {value}\n")).unwrap();
    interpreter
}

fn configured(cwd: &Path, python: &Path) -> EvalTool {
    let mut tool = EvalTool::new(cwd);
    tool.python_path = OsString::new();
    tool.python_environment = environment(python.parent().unwrap());
    tool
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(value) => Some(value.text.as_str()),
        _ => None,
    }).collect::<Vec<_>>().join("\n")
}

#[test]
fn real_project_dependencies_and_runtime_identity_survive_cells_and_reset() {
    let Some(python) = python() else { return; };
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let selected = make_venv(&python, &cwd.join(".venv"), 41);
    let tool = configured(cwd, &python);
    run(async {
        let first = tool.execute("import", json!({"code":"import pi_runtime_fixture; keep = pi_runtime_fixture.value; keep"}), None).await.unwrap();
        assert!(!first.is_error, "{}", text(&first));
        assert!(text(&first).contains("41"));
        assert_eq!(first.details.as_ref().unwrap()["pythonRuntime"]["source"], "project-dot-venv");
        assert_eq!(first.details.as_ref().unwrap()["pythonRuntime"]["executable"], selected.to_string_lossy().as_ref());
        let next = tool.execute("retained", json!({"code":"keep += 1; keep"}), None).await.unwrap();
        assert!(text(&next).contains("42"));
        assert_eq!(next.details.as_ref().unwrap()["cell"], 2);
        let reset = tool.execute("reset", json!({"reset":true,"code":"import pi_runtime_fixture; ('keep' in globals(), pi_runtime_fixture.value)"}), None).await.unwrap();
        assert!(!reset.is_error, "{}", text(&reset));
        assert!(text(&reset).contains("(False, 41)"));
        assert_eq!(reset.details.as_ref().unwrap()["cell"], 1);
    });
}

#[test]
fn active_environment_wins_and_explicit_failure_does_not_run_the_cell() {
    let Some(python) = python() else { return; };
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    make_venv(&python, &cwd.join(".venv"), 1);
    let active = cwd.join("active with spaces");
    make_venv(&python, &active, 2);
    let mut tool = configured(cwd, &python);
    tool.python_environment.virtual_env = Some(active.into_os_string());
    let output = run(tool.execute("active", json!({"code":"import pi_runtime_fixture; pi_runtime_fixture.value"}), None)).unwrap();
    assert!(!output.is_error, "{}", text(&output));
    assert_eq!(output.details.as_ref().unwrap()["pythonRuntime"]["source"], "active-venv");
    assert_eq!(text(&output).trim(), "2");
    let mut explicit = configured(cwd, &python);
    explicit.python_path = cwd.join("missing-python").into_os_string();
    let error = run(explicit.execute("no-fallback", json!({"code":"open('must-not-exist', 'w').write('ran')"}), None)).unwrap_err();
    assert!(error.to_string().contains("EVAL_PY_MISSING"));
    assert!(!cwd.join("must-not-exist").exists());
}

#[test]
fn interpreter_discovery_refuses_cancelled_or_changed_session_before_launch() {
    let dir = tempfile::tempdir().unwrap();
    let env = environment(dir.path());
    let owner = AgentCx::for_testing();
    let session = CellSession::new(crate::jobs::JobSessionScope::fixed("other"), "original".into());
    let error = run(env.resolve(OsStr::new(""), dir.path(), Instant::now() + Duration::from_secs(10), &session)).unwrap_err();
    assert!(error.to_string().contains("EVAL_SESSION"));
    owner.set_cancel_requested(true);
    let session = CellSession::new(crate::jobs::JobSessionScope::fixed("same"), "same".into());
    let error = run(owner.with_current(env.resolve(OsStr::new(""), dir.path(), Instant::now() + Duration::from_secs(10), &session))).unwrap_err();
    assert!(error.to_string().contains("EVAL_CANCELLED"));
}

#[cfg(unix)]
#[test]
fn non_utf8_interpreter_paths_are_not_lossily_retargeted() {
    use std::os::unix::ffi::OsStringExt as _;
    let dir = tempfile::tempdir().unwrap();
    let env = environment(dir.path());
    let name = OsString::from_vec(b"./python-\xff".to_vec());
    let plan = env.candidates(&name, dir.path()).unwrap();
    assert_eq!(plan[0].executable, dir.path().join(&name));
    assert_eq!(plan[0].command().get_program(), dir.path().join(name).as_os_str());
}
