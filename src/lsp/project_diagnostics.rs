//! Agent-visible native project diagnostics, distinct from a bounded file scan.
//!
//! An anchor chooses one server/workspace. The server decides which documents
//! to report; returned URIs are never used to read files or start other servers.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{LspInput, LspTool, MAX_PAYLOAD_BYTES, Result, ToolOutput, resolve_tool_path, text_output, tool_err};
use crate::agent_cx::AgentCx;

fn checkpoint(owner: &AgentCx) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| tool_err("LSP_CANCELLED", "project diagnostics cancelled"))?;
    if !owner.capabilities().io {
        return Err(tool_err("LSP_IO_PERMISSION", "project diagnostics requires filesystem I/O"));
    }
    Ok(())
}

fn validate_input(input: &LspInput) -> Result<&str> {
    if input.symbol.is_some()
        || input.line.is_some()
        || input.query.is_some()
        || input.range.is_some()
        || input.after.is_some()
        || input.position.is_some()
        || input.method.is_some()
        || input.payload.is_some()
        || input.apply.is_some()
        || input.new_name.is_some()
        || input.new_file.is_some()
        || input.hierarchy_id.is_some()
        || input.action_id.is_some()
        || input.refactor_id.is_some()
        || input.resolve.is_some()
        || input.only.is_some()
        || input.format_options.is_some()
        || input.completion_id.is_some()
        || input.snippet_values.is_some()
    {
        return Err(tool_err("LSP_USAGE", "project_diagnostics accepts only file, timeout and limit"));
    }
    if input.limit == Some(0) {
        return Err(tool_err("LSP_USAGE", "project diagnostic display limit must be positive"));
    }
    input
        .file
        .as_deref()
        .filter(|file| !file.is_empty() && file.len() <= 4096 && !file.chars().any(char::is_control))
        .ok_or_else(|| tool_err("LSP_USAGE", "project_diagnostics requires a bounded anchor file path"))
}

fn render_report(server: &str, root: &Path, reports: Vec<Value>, limit: usize) -> Result<ToolOutput> {
    let total_documents = reports.len();
    let mut total_diagnostics = 0_usize;
    let mut errors = 0_usize;
    let mut warnings = 0_usize;
    for report in &reports {
        if let Some(items) = report["items"].as_array() {
            total_diagnostics += items.len();
            for item in items {
                match item["severity"].as_u64() {
                    Some(1) => errors += 1,
                    Some(2) => warnings += 1,
                    _ => {}
                }
            }
        }
    }
    let mut payload = json!({
        "action": "project_diagnostics", "method": "workspace/diagnostic",
        "server": server, "root": root.display().to_string(), "cachedOnly": false,
        "responseComplete": true, "complete": false, "truncated": true,
        "totalDocuments": total_documents, "returnedDocuments": 0,
        "totalDiagnostics": total_diagnostics, "errorCount": errors, "warningCount": warnings,
        "documentLimit": limit, "reports": [],
        "note": "One server's full workspace report, not an atomic disk snapshot or proof that every project file was checked. Totals include omitted reports. When truncated, use diagnostics with an individual file or workspace_diagnostics with a glob. Returned URIs are metadata only."
    });
    // Reserve space for the changing numeric/boolean metadata. Never cut JSON
    // or a diagnostic halfway through, nor silently change a count to zero.
    let mut remaining = MAX_PAYLOAD_BYTES
        .checked_sub(payload.to_string().len().saturating_add(128))
        .ok_or_else(|| tool_err("LSP_OUTPUT_LIMIT", "project diagnostic metadata exceeds the output budget"))?;
    let mut retained = Vec::new();
    for report in reports.into_iter().take(limit) {
        let bytes = report.to_string().len().saturating_add(1);
        if bytes > remaining {
            break;
        }
        remaining -= bytes;
        retained.push(report);
    }
    let complete = retained.len() == total_documents;
    payload["returnedDocuments"] = json!(retained.len());
    payload["complete"] = json!(complete);
    payload["truncated"] = json!(!complete);
    payload["reports"] = Value::Array(retained);
    let text = payload.to_string();
    if text.len() > MAX_PAYLOAD_BYTES {
        return Err(tool_err("LSP_OUTPUT_LIMIT", "project diagnostic result exceeds the output budget"));
    }
    Ok(text_output(text, payload))
}

impl LspTool {
    pub(super) async fn run_project_diagnostics(&self, input: &LspInput) -> Result<ToolOutput> {
        let file = validate_input(input)?;
        let owner = AgentCx::for_current_or_request();
        checkpoint(&owner)?;
        let path = resolve_tool_path(file, &self.cwd);
        let timeout = self.request_timeout(input).min(Duration::from_secs(120));
        let limit = input.limit.unwrap_or(100).min(1000);
        let started = Instant::now();
        let now = owner
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let operation = async {
            // The anchor selects the server; it is not opened as a document.
            // In particular, a glob or nonexistent anchor must not spawn one.
            if !std::fs::metadata(&path)?.is_file() {
                return Err(tool_err("LSP_FILE_UNREADABLE", "project diagnostic anchor must be a regular file"));
            }
            checkpoint(&owner)?;
            if started.elapsed() >= timeout {
                return Err(tool_err("LSP_TIMEOUT", "project diagnostic budget expired"));
            }
            let entry = self.client_for(&path).await?;
            checkpoint(&owner)?;
            let remaining = timeout.saturating_sub(started.elapsed());
            let reports = entry.client.workspace_diagnostics(remaining).await?;
            let output = render_report(&entry.spec_name, entry.client.root(), reports, limit)?;
            checkpoint(&owner)?;
            if started.elapsed() >= timeout {
                return Err(tool_err("LSP_TIMEOUT", "project diagnostic budget expired"));
            }
            Ok(output)
        };
        asupersync::time::timeout(now, timeout, owner.with_current(operation))
            .await
            .map_err(|_| tool_err("LSP_TIMEOUT", "project diagnostic budget expired"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tools::Tool;
    use std::process::{Command, Stdio};

    const SERVER: &str = r"
import json, pathlib, sys
mode, root = sys.argv[1], pathlib.Path(sys.argv[2])
frames = []
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
    frames.append(message)
    method = message.get('method')
    if method == 'exit': break
    if 'id' not in message: continue
    response = {'jsonrpc':'2.0', 'id':message['id']}
    if method == 'initialize':
        response['result'] = {'capabilities':{'textDocumentSync':1,'diagnosticProvider':{
            'identifier':'project-fixture','workspaceDiagnostics':mode != 'unsupported',
            'interFileDependencies':True}}}
    elif method == 'test/frames': response['result'] = frames
    elif method == 'workspace/diagnostic':
        if mode == 'error': response['error'] = {'code':-32603,'message':'project analysis failed'}
        elif mode == 'malformed': response['result'] = {'items':None}
        elif mode == 'empty': response['result'] = {'items':[]}
        elif mode == 'hang': continue
        elif message['params'] != {'identifier':'project-fixture','previousResultIds':[]}:
            response['error'] = {'code':-32602,'message':'unexpected workspace request parameters'}
        else:
            diagnostic = {'message':'unopened diagnostic','severity':1,
                'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}}}
            warning = dict(diagnostic, message='virtual warning', severity=2)
            response['result'] = {'items':[
                {'uri':(root.resolve()/'never-opened.piproject').as_uri(),'version':None,'kind':'full','items':[diagnostic]},
                {'uri':'untitled:virtual-project','version':None,'kind':'full','items':[warning]}]}
    elif method == 'shutdown': response['result'] = None
    else: response['error'] = {'code':-32601,'message':'unexpected per-document request'}
    send(response)
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
            eprintln!("SKIP project diagnostic stdio test: Python is unavailable");
            return None;
        };
        std::fs::write(root.join("anchor.piproject"), "anchor").unwrap();
        let config: Config = serde_json::from_value(json!({"lsp":{
            "enabled":true,"servers":{"project-fixture":{
                "command":python,"args":["-u","-c",SERVER,mode,root.display().to_string()],
                "languages":["plaintext"],"extensions":[".piproject"],"rootMarkers":[]
            }}
        }})).unwrap();
        Some(LspTool::new(root, Some(&config)))
    }

    #[test]
    fn agent_visible_project_diagnostics_uses_one_native_request_for_unopened_sources() {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = tool(temp.path(), "full") else {
            return;
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let output = runtime.block_on(tool.execute("project-test", json!({
            "action":"project_diagnostics","file":"anchor.piproject","timeout":5
        }), None)).unwrap();
        assert!(!output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["totalDocuments"], 2);
        assert_eq!(details["returnedDocuments"], 2);
        assert_eq!(details["totalDiagnostics"], 2);
        assert_eq!(details["errorCount"], 1);
        assert_eq!(details["warningCount"], 1);
        assert_eq!(details["complete"], true);
        assert_eq!(details["truncated"], false);
        assert_eq!(details["cachedOnly"], false);
        assert!(details["reports"][0]["uri"].as_str().unwrap().contains("never-opened"));
        assert!(!temp.path().join("never-opened.piproject").exists());
        let entry = runtime.block_on(tool.client_for(&temp.path().join("anchor.piproject"))).unwrap();
        let frames = runtime.block_on(entry.client.call("test/frames", json!({}), Duration::from_secs(5))).unwrap();
        let frames = frames.as_array().unwrap();
        assert_eq!(frames.iter().filter(|frame| frame["method"] == "workspace/diagnostic").count(), 1);
        assert!(!frames.iter().any(|frame| matches!(frame["method"].as_str(),
            Some("textDocument/didOpen" | "textDocument/didChange" | "textDocument/diagnostic"))));
        assert_eq!(entry.client.open_document_count(), 0);
    }

    #[test]
    fn project_display_limit_retains_full_report_totals() {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = tool(temp.path(), "full") else {
            return;
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let output = runtime.block_on(tool.execute("project-test", json!({
            "action":"project_diagnostics","file":"anchor.piproject","timeout":5,"limit":1
        }), None)).unwrap();
        let details = output.details.unwrap();
        assert_eq!(details["responseComplete"], true);
        assert_eq!(details["complete"], false);
        assert_eq!(details["truncated"], true);
        assert_eq!(details["totalDocuments"], 2);
        assert_eq!(details["returnedDocuments"], 1);
        assert_eq!(details["totalDiagnostics"], 2);
        assert_eq!(details["warningCount"], 1);
    }

    #[test]
    fn native_project_failures_do_not_fall_back_to_empty_or_document_scans() {
        for (mode, expected) in [
            ("unsupported", "LSP_UNSUPPORTED"),
            ("error", "project analysis failed"),
            ("malformed", "LSP_WORKSPACE_DIAGNOSTIC_REPORT"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let Some(tool) = tool(temp.path(), mode) else {
                return;
            };
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
            let error = runtime.block_on(tool.execute("project-test", json!({
                "action":"project_diagnostics","file":"anchor.piproject","timeout":5
            }), None)).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn complete_empty_native_response_is_distinct_from_missing_results() {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = tool(temp.path(), "empty") else {
            return;
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let output = runtime.block_on(tool.execute("project-test", json!({
            "action":"project_diagnostics","file":"anchor.piproject","timeout":5
        }), None)).unwrap();
        let details = output.details.unwrap();
        assert_eq!(details["complete"], true);
        assert_eq!(details["responseComplete"], true);
        assert_eq!(details["totalDocuments"], 0);
        assert_eq!(details["totalDiagnostics"], 0);
    }

    #[test]
    fn byte_truncation_keeps_valid_json_and_does_not_erase_error_totals() {
        let report = json!({"uri":"untitled:large","kind":"full","version":null,"items":[{
            "message":"x".repeat(MAX_PAYLOAD_BYTES),"severity":1
        }]});
        let output = render_report("server", Path::new("/workspace"), vec![report], 100).unwrap();
        let details = output.details.unwrap();
        assert!(details.to_string().len() <= MAX_PAYLOAD_BYTES);
        assert_eq!(details["totalDocuments"], 1);
        assert_eq!(details["returnedDocuments"], 0);
        assert_eq!(details["errorCount"], 1);
        assert_eq!(details["truncated"], true);
        assert_eq!(details["complete"], false);
    }

    #[test]
    fn invalid_project_selectors_are_refused_before_starting_a_server() {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = tool(temp.path(), "full") else {
            return;
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        for extra in [json!({"limit":0}), json!({"query":"other"}), json!({"symbol":"other"}),
            json!({"apply":false}), json!({"after":"a.rs"}), json!({"payload":{}})] {
            let mut input = json!({"action":"project_diagnostics","file":"anchor.piproject"});
            input.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            assert!(runtime.block_on(tool.execute("project-test", input, None)).is_err());
            assert!(tool.registry.status().is_empty());
        }
    }
}
