//! Exercise the actual model-facing diagnostics tool over real framed stdio.

use super::*;
use std::process::{Command, Stdio};

const SERVER: &str = r"
import json, sys
mode = sys.argv[1]
def send(value):
    body = json.dumps(value).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b'\n', b'\r\n'): break
        key, value = line.decode('ascii').split(':', 1)
        headers[key.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    method = message.get('method')
    if method == 'exit': break
    if method == 'textDocument/didOpen' and mode == 'push_empty':
        document = message['params']['textDocument']
        send({'jsonrpc':'2.0','method':'textDocument/publishDiagnostics',
              'params':{'uri':document['uri'],'version':document['version'],'diagnostics':[]}})
    if 'id' not in message: continue
    response = {'jsonrpc':'2.0','id':message['id']}
    if method == 'initialize':
        capabilities = {'textDocumentSync':1}
        if mode not in ('silent', 'push_empty'):
            capabilities['diagnosticProvider'] = {'interFileDependencies':False,'workspaceDiagnostics':False}
        response['result'] = {'capabilities':capabilities}
    elif method == 'textDocument/diagnostic':
        if mode == 'error': response['error'] = {'code':-32603,'message':'diagnostic engine failed'}
        elif mode == 'malformed': response['result'] = {'kind':'full','items':None}
        elif mode == 'unchanged_without_prior': response['result'] = {'kind':'unchanged','resultId':'unknown'}
        else:
            items = [] if mode == 'pull_empty' else [{'message':'real diagnostic',
                'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}},'severity':1}]
            response['result'] = {'kind':'full','resultId':'fixture','items':items}
    else: response['result'] = None
    send(response)
    if method == 'initialize' and mode in ('push_empty', 'pull_empty'):
        send({'jsonrpc':'2.0','method':'experimental/serverStatus','params':{'quiescent':True}})
";

fn tool(root: &Path, mode: &str) -> Option<LspTool> {
    let python = ["python3", "python"].into_iter().find(|program| {
        Command::new(program)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    });
    let Some(python) = python else {
        eprintln!("SKIP model-facing LSP diagnostics test: Python is unavailable");
        return None;
    };
    std::fs::write(root.join("source.pidiag"), "source").expect("real source file");
    let config: Config = serde_json::from_value(json!({"lsp":{
        "enabled":true,"servers":{"diagnostic-fixture":{
            "command":python,"args":["-u","-c",SERVER,mode],
            "languages":["plaintext"],"extensions":[".pidiag"],"rootMarkers":[]
        }}
    }}))
    .expect("server configuration");
    Some(LspTool::new(root, Some(&config)))
}

/// Run the tool with a budget that only has to outlast a healthy server.
///
/// The budget is in seconds and covers the Python server's spawn as well as
/// the request. At 1 there was no margin left for the spawn on a loaded host:
/// a full-suite run failed with "[LSP_TIMEOUT] request timed out after 7 ms" —
/// startup had consumed 993 ms of the second — while the same test passed
/// every time when run alone. 5 matches actions/refactor/tests.rs.
///
/// Use [`run_until_budget_expires`] instead for a case that is *supposed* to
/// run the clock out, so raising this bound does not silently make it slower.
fn run(tool: &LspTool, file: &str) -> Result<ToolOutput> {
    run_with_timeout(tool, file, 5)
}

/// Run the tool where exhausting the budget IS the expected outcome — a server
/// that never reports has nothing else to end the wait, since an empty result
/// stays unsettled for `WARMUP_EMPTY_RESULT_WINDOW` (180s) unless the server
/// declares quiescence. Keep this short: it is paid in full on every run.
fn run_until_budget_expires(tool: &LspTool, file: &str) -> Result<ToolOutput> {
    run_with_timeout(tool, file, 1)
}

fn run_with_timeout(tool: &LspTool, file: &str, timeout: u64) -> Result<ToolOutput> {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(tool.execute(
        "diagnostics-test",
        json!({
            "action":"diagnostics","file":file,"timeout":timeout
        }),
        None,
    ))
}

#[test]
fn model_facing_diagnostics_retrieves_a_pull_only_report() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = tool(temp.path(), "pull") else {
        return;
    };
    let output = run(&tool, "source.pidiag").expect("diagnostic report");
    assert!(!output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["count"], 1);
    assert_eq!(details["diagnostics"][0]["message"], "real diagnostic");
    assert_eq!(details["server"], "diagnostic-fixture");
}

#[test]
fn model_facing_diagnostics_propagates_pull_errors_instead_of_zero_issues() {
    for mode in ["error", "malformed", "unchanged_without_prior"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = tool(temp.path(), mode) else {
            return;
        };
        let error = run(&tool, "source.pidiag").expect_err("no clean result on failure");
        let message = error.to_string();
        if mode == "error" {
            assert!(message.contains("diagnostic engine failed"), "{message}");
        } else {
            assert!(message.contains("LSP_DIAGNOSTIC_REPORT"), "{message}");
        }
    }
}

#[test]
fn model_facing_diagnostics_requires_an_actual_push_report() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = tool(temp.path(), "silent") else {
        return;
    };
    // The `silent` server never reports and never declares quiescence, so the
    // wait can only end by running out of budget. That is the behaviour under
    // test, so it takes the short one.
    let error =
        run_until_budget_expires(&tool, "source.pidiag").expect_err("missing report is not clean");
    assert!(
        error.to_string().contains("LSP_DIAGNOSTICS_PENDING"),
        "{error}"
    );
}

#[test]
fn explicitly_empty_push_and_pull_reports_are_successful_empty_results() {
    for mode in ["push_empty", "pull_empty"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = tool(temp.path(), mode) else {
            return;
        };
        let output = run(&tool, "source.pidiag").expect("explicit empty report");
        assert!(!output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["count"], 0);
        assert_eq!(details["diagnostics"], json!([]));
    }
}

#[test]
fn glob_view_explicitly_declares_cached_partial_coverage_without_spawning() {
    let temp = tempfile::tempdir().unwrap();
    let tool = LspTool::new(temp.path(), None);
    let output = run(&tool, "**/*.rs").expect("cache view without any server");
    let details = output.details.unwrap();
    assert_eq!(details["files"], 0);
    assert_eq!(details["cachedOnly"], true);
    assert_eq!(details["complete"], false);
    assert!(
        details["note"]
            .as_str()
            .unwrap()
            .contains("have not been checked")
    );
    assert!(tool.registry.status().is_empty());
}
