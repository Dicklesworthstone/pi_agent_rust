//! Offline package-installation workflows through the public retained EvalTool.
//! Installs only an inert local wheel into each test's private target directory.
#![recursion_limit = "512"]

use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput, ToolUpdate};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("runtime")
        .block_on(future)
}

fn available(pip: bool) -> bool {
    let python = std::env::var("PI_EVAL_PYTHON").unwrap_or_else(|_| "python3".into());
    let mut command = std::process::Command::new(python);
    if pip {
        command.args(["-I", "-m", "pip", "--version"]);
    } else {
        command.arg("--version");
    }
    let found = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !found {
        eprintln!("Skipping Python package workflow: configured interpreter/pip unavailable (pip required: {pip})");
    }
    found
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }).collect::<Vec<_>>().join("\n")
}

async fn cell(tool: &EvalTool, code: &str) -> ToolOutput {
    tool.execute("package-cell", json!({"kernel":"python", "code":code, "timeout_secs":60}), None)
        .await.expect("cell transport")
}

// Stored wheel, no backend/build/network dependency. A target inside the temp
// workspace avoids modifying the interpreter running the test suite.
const WHEEL: &str = r#"
import asyncio, contextvars, os, sys, zipfile
from pathlib import Path
workspace = Path.cwd()
keep = {'answer': 41}
original_pid = os.getpid()
original_loop = id(asyncio.get_running_loop())
marker = contextvars.ContextVar('package-workflow')
marker.set('retained')
Path('wheel house').mkdir()
Path('elsewhere').mkdir()
info = 'pi_eval_package_fixture-0.0.1.dist-info'
entries = {
    'pi_eval_package_fixture/__init__.py': 'VALUE = 42\n',
    info + '/METADATA': 'Metadata-Version: 2.1\nName: pi-eval-package-fixture\nVersion: 0.0.1\n',
    info + '/WHEEL': 'Wheel-Version: 1.0\nGenerator: pi-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n',
}
entries[info + '/RECORD'] = ''.join(name + ',,\n' for name in entries) + info + '/RECORD,,\n'
with zipfile.ZipFile('wheel house/pi_eval_package_fixture-0.0.1-py3-none-any.whl', 'w') as archive:
    for name, data in entries.items():
        archive.writestr(name, data)
os.chdir('elsewhere')
"#;

const INSTALL: &str = "%pip install --no-index --no-deps --target 'private packages' 'wheel house/pi_eval_package_fixture-0.0.1-py3-none-any.whl'";

#[test]
fn local_wheel_install_streams_without_replacing_retained_python_state() {
    if !available(true) { return; }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    let updates = Arc::new(Mutex::new(Vec::<ToolUpdate>::new()));
    run(async {
        let seeded = cell(&tool, WHEEL).await;
        assert!(!seeded.is_error, "{}", text(&seeded));
        let collected = Arc::clone(&updates);
        let installed = tool.execute("install", json!({
            "kernel":"python", "code":INSTALL, "timeout_secs":60
        }), Some(Box::new(move |update| collected.lock().unwrap().push(update))))
            .await.unwrap();
        assert!(!installed.is_error, "{}", text(&installed));
        assert!(text(&installed).contains("EVAL_PIP_DONE"));
        assert_eq!(installed.details.as_ref().unwrap()["cell"], 2);
        assert_eq!(installed.details.as_ref().unwrap()["restarted"], false);
        assert!(dir.path().join("private packages/pi_eval_package_fixture/__init__.py").is_file());
        assert!(!dir.path().join("elsewhere/private packages").exists());
        let retained = cell(&tool, r#"
sys.path.insert(0, str(workspace / 'private packages'))
import pi_eval_package_fixture
assert pi_eval_package_fixture.VALUE == 42
assert keep['answer'] == 41
assert os.getpid() == original_pid
assert id(asyncio.get_running_loop()) == original_loop
assert marker.get() == 'retained'
assert Path.cwd() == workspace / 'elsewhere'
display({'installed': pi_eval_package_fixture.VALUE, 'retained': keep['answer']})
"#).await;
        assert!(!retained.is_error, "{}", text(&retained));
        assert_eq!(retained.details.as_ref().unwrap()["jsonOutputs"], json!([{"installed":42,"retained":41}]));
        assert_eq!(retained.details.as_ref().unwrap()["cell"], 3);
    });
    let updates = updates.lock().unwrap();
    assert!(!updates.is_empty(), "installer output must enter the native update path");
    assert!(updates.iter().any(|update| update.content.iter().any(|block|
        matches!(block, ContentBlock::Text(text) if text.text.contains("EVAL_PIP_START"))
    )));
}

#[test]
fn invalid_package_commands_do_not_launch_an_installer_or_erase_variables() {
    if !available(false) { return; }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        assert!(!cell(&tool, "keep = 42").await.is_error);
        for code in [
            "%pip", "%pip install", "%pip uninstall fixture", "%pip install \"unterminated",
            "%pip install foo\nkeep = 99", "%pip install \0", "%pip install ''",
        ] {
            let output = cell(&tool, code).await;
            assert!(output.is_error, "accepted {code:?}");
            assert!(text(&output).contains("EVAL_PIP_"));
            assert!(!text(&output).contains("EVAL_PIP_START:"));
        }
        let long = cell(&tool, &format!("%pip install {}", "x".repeat(16_384))).await;
        assert!(long.is_error);
        assert!(text(&long).contains("EVAL_PIP_LIMIT"));
        let retained = cell(&tool, "keep").await;
        assert!(!retained.is_error);
        assert_eq!(text(&retained).trim(), "42");
    });
}

#[test]
fn installer_errors_keep_logs_and_do_not_become_success_or_reset_state() {
    if !available(true) { return; }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    run(async {
        assert!(!cell(&tool, "keep = 42").await.is_error);
        let output = cell(&tool, "%pip install --no-index --not-a-pip-option").await;
        assert!(output.is_error);
        assert!(text(&output).contains("EVAL_PIP_FAILED"));
        assert!(text(&output).contains("EVAL_PIP_START"));
        assert!(!text(&output).contains("EVAL_PIP_DONE"));
        let retained = cell(&tool, "keep").await;
        assert!(!retained.is_error);
        assert_eq!(text(&retained).trim(), "42");
        assert_eq!(retained.details.as_ref().unwrap()["restarted"], false);
    });
}

#[test]
fn installer_does_not_import_a_workspace_pip_module() {
    if !available(true) { return; }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pip.py"), "raise RuntimeError('WORKSPACE_PIP_SHADOW_EXECUTED')\n").unwrap();
    let tool = EvalTool::new(dir.path());
    let output = run(cell(&tool, "%pip install --help"));
    assert!(!output.is_error, "{}", text(&output));
    assert!(text(&output).contains("EVAL_PIP_DONE"));
    assert!(!text(&output).contains("WORKSPACE_PIP_SHADOW_EXECUTED"));
}

#[test]
fn missing_import_only_suggests_explicit_installation() {
    if !available(false) { return; }
    let dir = tempfile::tempdir().unwrap();
    let tool = EvalTool::new(dir.path());
    let output = run(cell(&tool, "import pi_deliberately_absent_package_78cd921"));
    assert!(output.is_error);
    assert!(text(&output).contains("ModuleNotFoundError"));
    assert!(text(&output).contains("EVAL_PACKAGE_HINT"));
    assert!(!text(&output).contains("EVAL_PIP_START"));
    assert_eq!(output.details.as_ref().unwrap()["errorKind"], Value::String("exception".into()));
}

#[test]
fn cancellation_during_a_real_build_retires_installer_descendants_and_kernel() {
    use std::time::{Duration, Instant};
    if !available(true) { return; }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pyproject.toml"), r#"
[build-system]
requires = []
build-backend = "fixture_backend"
backend-path = ["."]
"#).unwrap();
    std::fs::write(dir.path().join("fixture_backend.py"), r#"
import os, time
from pathlib import Path

def get_requires_for_build_wheel(config_settings=None):
    return []

def prepare_metadata_for_build_wheel(metadata_directory, config_settings=None):
    Path(__file__).with_name('installer-pid.txt').write_text(str(os.getpid()))
    print('PI_PIP_BACKEND_WAITING', flush=True)
    time.sleep(60)
    raise RuntimeError('cancellation failed to stop the build')
"#).unwrap();
    let tool = EvalTool::new(dir.path());
    assert!(!run(cell(&tool, "keep = 42")).is_error);
    let owner = asupersync::Cx::for_testing();
    let cancel = owner.clone();
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_backend = Arc::clone(&observed);
    let mut request = tool.execute("cancel-install", json!({
        "kernel":"python", "timeout_secs":30,
        "code":"%pip install --no-index --no-deps --no-build-isolation -v --target 'private packages' ."
    }), Some(Box::new(move |update| {
        if update.content.iter().any(|block|
            matches!(block, ContentBlock::Text(text) if text.text.contains("PI_PIP_BACKEND_WAITING"))
        ) {
            saw_backend.store(true, std::sync::atomic::Ordering::SeqCst);
            cancel.set_cancel_requested(true);
        }
    })));
    let stopped = run(futures::future::poll_fn(|task| {
        // Install the test owner for each poll, never across an executor wait.
        let _owner = owner.clone().set_current_restricted();
        std::future::Future::poll(request.as_mut(), task)
    })).expect_err("cancelled installation must fail");
    drop(request);
    assert!(observed.load(std::sync::atomic::Ordering::SeqCst), "real build must start");
    assert!(stopped.to_string().contains("EVAL_CANCELLED"), "{stopped}");
    let pid: u32 = std::fs::read_to_string(dir.path().join("installer-pid.txt"))
        .unwrap().parse().unwrap();
    let pid = sysinfo::Pid::from_u32(pid);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if system.process(pid).is_none_or(|process|
            cfg!(unix) && process.status() == sysinfo::ProcessStatus::Zombie
        ) { break; }
        assert!(Instant::now() < deadline, "installer descendant survived cancellation");
        std::thread::sleep(Duration::from_millis(20));
    }
    let fresh = run(cell(&tool, "'keep' in globals()"));
    assert!(!fresh.is_error);
    assert!(text(&fresh).contains("False"));
}
