//! Real offline wheel installs through EvalTool; no package-index dependency.
//! Run via `dsr quality --tool pi_agent_rust`. Missing Python/pip is explicit.
#![recursion_limit = "512"]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::eval::EvalTool;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const FIXTURES: &str = r#"
import base64, csv, hashlib, io, json, os, sys, zipfile
from pathlib import Path
seed = 40
original_pid = os.getpid()
original_prefix = sys.prefix
workspace = Path.cwd()
(workspace / 'workspace_import_probe.py').write_text('value = 40\n')
import workspace_import_probe
assert workspace_import_probe.value == seed
wheelhouse = workspace / 'wheel house'
wheelhouse.mkdir()
(workspace / 'elsewhere').mkdir()
# An unisolated `python -m pip` would execute this instead of the real driver.
(workspace / 'pip.py').write_text("raise RuntimeError('workspace pip shadow executed')\n")
def make_wheel(name, version, value, dependency=None):
    info = f'{name}-{version}.dist-info'
    metadata = f'Metadata-Version: 2.1\nName: {name.replace("_", "-")}\nVersion: {version}\n'
    if dependency:
        metadata += f'Requires-Dist: {dependency}\n'
    files = {
        f'{name}.py': f'value = {value}\n'.encode(),
        f'{info}/METADATA': metadata.encode(),
        f'{info}/WHEEL': b'Wheel-Version: 1.0\nGenerator: pi-eval-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n',
        f'{name}.pth': b"import pathlib; pathlib.Path('pth-side-effect').write_text('unexpected')\n",
    }
    record = io.StringIO()
    writer = csv.writer(record)
    for path, data in files.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).decode().rstrip('=')
        writer.writerow((path, 'sha256=' + digest, len(data)))
    writer.writerow((f'{info}/RECORD', '', ''))
    files[f'{info}/RECORD'] = record.getvalue().encode()
    with zipfile.ZipFile(wheelhouse / f'{name}-{version}-py3-none-any.whl', 'w') as archive:
        for path, data in files.items():
            archive.writestr(path, data)
make_wheel('pi_eval_dep_probe', '1.0', 2, 'pi-eval-transitive-probe==1.0')
make_wheel('pi_eval_dep_probe', '2.0', 3)
make_wheel('pi_eval_transitive_probe', '1.0', 8)
None
"#;

fn prerequisites(pip: bool) -> bool {
    let executable = std::env::var("PI_EVAL_PYTHON").unwrap_or_else(|_| "python3".into());
    let mut command = std::process::Command::new(executable);
    if pip {
        command.args(["-I", "-m", "pip", "--version"]);
    } else {
        command.arg("--version");
    }
    let available = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        eprintln!("Skipping eval package scenario: configured Python/pip unavailable (pip={pip})");
    }
    available
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("test runtime");
    runtime.block_on(future)
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cell(tool: &EvalTool, code: &str) -> ToolOutput {
    block_on(tool.execute(
        "packages",
        json!({"kernel": "python", "code": code, "timeout_secs": 30}),
        None,
    ))
    .expect("cell protocol")
}

fn success(tool: &EvalTool, code: &str) -> String {
    let output = cell(tool, code);
    assert!(!output.is_error, "{}", text(&output));
    text(&output)
}

fn finish(harness: &TestHarness) {
    harness
        .log()
        .info("verify", "eval package assertions passed");
    let path = harness.temp_path("eval-packages.jsonl");
    harness.write_jsonl_logs(&path).expect("write test log");
    let payload = std::fs::read_to_string(path).expect("read test log");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "invalid log: {errors:?}");
}

#[test]
fn offline_packages_stream_resolve_dependencies_and_keep_the_kernel() {
    if !prerequisites(true) {
        return;
    }
    let harness = TestHarness::new("eval_packages_offline");
    let tool = EvalTool::new(&harness.temp_path("."));
    success(&tool, FIXTURES);
    let updates = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&updates);
    let output = block_on(tool.execute(
        "install",
        json!({"code": "%pip install --managed --no-index --find-links 'wheel house' pi-eval-dep-probe==1.0"}),
        Some(Box::new(move |update| {
            let mut captured = captured.lock().expect("captured output");
            for block in update.content {
                if let ContentBlock::Text(text) = block {
                    captured.push_str(&text.text);
                }
            }
        })),
    ))
    .expect("install protocol");
    assert!(!output.is_error, "{}", text(&output));
    assert!(text(&output).contains("EVAL_PACKAGES_READY"));
    assert!(
        updates
            .lock()
            .expect("updates")
            .contains("EVAL_PIP_INSTALLING")
    );
    assert_eq!(
        success(
            &tool,
            concat!(
                "import pi_eval_dep_probe, pi_eval_transitive_probe\n",
                "(seed + pi_eval_dep_probe.value, pi_eval_transitive_probe.value, ",
                "os.getpid() == original_pid, sys.prefix == original_prefix, ",
                "(workspace / 'pth-side-effect').exists())"
            )
        )
        .trim(),
        "(42, 8, True, True, False)"
    );
    let package_root: String = serde_json::from_str(
        success(
            &tool,
            "print(json.dumps(str(Path(pi_eval_dep_probe.__file__).parents[2])))",
        )
        .trim(),
    )
    .expect("package root");
    let package_root = PathBuf::from(package_root);
    assert!(package_root.is_dir());
    drop(tool);
    assert!(
        !package_root.exists(),
        "kernel-owned packages survived disposal"
    );
    finish(&harness);
}

#[test]
fn failed_install_preserves_packages_and_upgrade_requires_explicit_reload() {
    if !prerequisites(true) {
        return;
    }
    let harness = TestHarness::new("eval_packages_failure_upgrade");
    let tool = EvalTool::new(&harness.temp_path("."));
    success(&tool, FIXTURES);
    success(
        &tool,
        "%pip install --managed --no-index --find-links 'wheel house' pi-eval-dep-probe==1.0",
    );
    success(
        &tool,
        "import pi_eval_dep_probe\nbefore_failure = list(sys.path)",
    );
    let unchanged = success(
        &tool,
        "%pip install --managed --no-index \"example; python_version < '0'\"",
    );
    assert!(unchanged.contains("EVAL_PACKAGES_UNCHANGED"));
    assert_eq!(success(&tool, "sys.path == before_failure").trim(), "True");
    let failed = cell(
        &tool,
        "%pip install --managed --no-index pi-eval-nonexistent-probe==99",
    );
    assert!(failed.is_error);
    assert!(text(&failed).contains("EVAL_PIP_FAILED"));
    assert_eq!(
        success(
            &tool,
            "(sys.path == before_failure, seed, pi_eval_dep_probe.value)"
        )
        .trim(),
        "(True, 40, 2)"
    );
    success(&tool, "os.chdir('elsewhere')");
    success(
        &tool,
        "%pip install --managed --no-index --find-links='wheel house' pi-eval-dep-probe==2.0",
    );
    assert_eq!(
        success(
            &tool,
            concat!(
                "import importlib\ncached = pi_eval_dep_probe.value\n",
                "importlib.reload(pi_eval_dep_probe)\n",
                "(cached, pi_eval_dep_probe.value, seed, os.getpid() == original_pid)"
            )
        )
        .trim(),
        "(2, 3, 40, True)"
    );
    success(
        &tool,
        "(workspace / 'use_packages.py').write_text('import asyncio, pi_eval_dep_probe\\nawait asyncio.sleep(0)\\nseed + pi_eval_dep_probe.value\\n')\nNone",
    );
    assert_eq!(success(&tool, "%load use_packages.py").trim(), "43");
    finish(&harness);
}

#[test]
fn invalid_package_commands_do_not_spawn_and_reset_discards_installed_packages() {
    if !prerequisites(true) {
        return;
    }
    let harness = TestHarness::new("eval_packages_validation_reset");
    let tool = EvalTool::new(&harness.temp_path("."));
    success(&tool, FIXTURES);
    for code in [
        "%pip",
        "%pip install --managed",
        "%pip uninstall sample",
        "%pip install --managed sample --target /tmp/wrong",
        "%pip install --managed --user sample",
        "%pip install --managed --prefix=wrong sample",
        "%pip install --managed --python other sample",
        "%pip install --managed --editable .",
        "%pip install --managed --no-index",
        "%pip install --managed --find-links --no-index sample",
        "%pip install --managed 'unterminated",
        "%pip install --managed sample\nseed = 0",
    ] {
        let output = cell(&tool, code);
        assert!(output.is_error, "{code}");
        assert!(text(&output).contains("EVAL_PIP_"), "{}", text(&output));
        assert!(!text(&output).contains("EVAL_PIP_INSTALLING"), "{code}");
    }
    assert_eq!(success(&tool, "seed").trim(), "40");
    success(
        &tool,
        "%pip install --managed --no-index --find-links 'wheel house' pi-eval-dep-probe==1.0",
    );
    let root: String = serde_json::from_str(success(
        &tool,
        "import pi_eval_dep_probe\nprint(json.dumps(str(Path(pi_eval_dep_probe.__file__).parents[2])))",
    ).trim()).expect("package root");
    let reset = block_on(tool.execute(
        "reset",
        json!({"kernel": "python", "reset": true,
            "code": "import importlib.util\nimportlib.util.find_spec('pi_eval_dep_probe') is None"}),
        None,
    )).expect("reset protocol");
    assert!(!reset.is_error, "{}", text(&reset));
    assert!(text(&reset).contains("True"));
    assert!(
        !PathBuf::from(root).exists(),
        "reset did not retire package storage"
    );
    finish(&harness);
}

#[test]
fn package_install_uses_the_existing_cell_timeout_and_retires_storage() {
    if !prerequisites(true) {
        return;
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    let harness = TestHarness::new("eval_packages_timeout");
    let tool = EvalTool::new(&harness.temp_path("."));
    success(&tool, FIXTURES);
    success(
        &tool,
        "%pip install --managed --no-index --find-links 'wheel house' pi-eval-dep-probe==1.0",
    );
    let package_root: String = serde_json::from_str(success(
        &tool,
        "import pi_eval_dep_probe\nprint(json.dumps(str(Path(pi_eval_dep_probe.__file__).parents[2])))",
    ).trim()).expect("package root");
    // A real loopback index accepts pip's connection but never answers it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback index");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("index address");
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let (tx, rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut connection = None;
        while !server_stop.load(Ordering::Acquire) && Instant::now() < deadline {
            if connection.is_none() {
                match listener.accept() {
                    Ok((stream, _)) => {
                        connection = Some(stream);
                        let _ = tx.send(());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("index accept: {error}"),
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let started = Instant::now();
    let result = block_on(tool.execute(
        "timed-install",
        json!({"code": format!("%pip install --managed --index-url http://{address}/simple pi-eval-timeout-probe"),
            "timeout_secs": 3}),
        None,
    ));
    stop.store(true, Ordering::Release);
    server.join().expect("join index server");
    rx.try_recv()
        .expect("pip reached the loopback index before timeout");
    let error = result.expect_err("package installation must honor the cell timeout");
    assert!(error.to_string().contains("EVAL_TIMEOUT"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(
        !PathBuf::from(package_root).exists(),
        "timed-out packages survived"
    );
    assert_eq!(success(&tool, "'seed' in globals()").trim(), "False");
    finish(&harness);
}
