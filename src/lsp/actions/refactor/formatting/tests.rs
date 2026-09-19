//! Exercise formatting through the real tool, client, stdio framing and files.

use super::*;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;
use std::collections::HashMap;
use std::process::{Command, Stdio};

const SERVER: &str = r"
import json, pathlib, sys
root, mode = pathlib.Path.cwd(), sys.argv[1]
def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b'\r\n', b'\n'):
            break
        key, value = line.decode('ascii').split(':', 1)
        headers[key.lower()] = value.strip()
    return json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
def send(message):
    body = json.dumps(message).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
def edit(start, end, text, line=0):
    return {'range':{'start':{'line':line,'character':start},'end':{'line':line,'character':end}},'newText':text}
while True:
    message = read()
    method, params = message.get('method'), message.get('params') or {}
    with (root / 'requests.jsonl').open('a', encoding='utf-8') as log:
        log.write(json.dumps(message) + '\n')
    if 'id' not in message:
        continue
    result = None
    if method == 'initialize':
        result = {'capabilities':{'textDocumentSync':1,
            'documentFormattingProvider':mode != 'unsupported',
            'documentRangeFormattingProvider':{} if mode != 'unsupported' else False}}
    elif method in ('textDocument/formatting', 'textDocument/rangeFormatting'):
        if mode == 'hang':
            continue
        if mode == 'error':
            send({'jsonrpc':'2.0','id':message['id'],'error':{'code':-32603,'message':'formatter failed'}})
            continue
        result = [edit(0, 8, 'let x = 1;')]
        if mode == 'null':
            result = None
        elif mode == 'empty':
            result = []
        elif mode == 'same':
            result = [edit(0, 8, 'let x=1;')]
        elif mode == 'invalid':
            result = {'changes':{params['textDocument']['uri']:result}}
        elif mode == 'overlap':
            result = result + result
        elif mode == 'bad-line':
            result = [edit(0, 1, 'x', 99)]
        elif mode == 'surrogate':
            result = [edit(1, 1, 'x')]
        elif mode == 'drift':
            (root / 'source.fmtfixture').write_text('external edit\n', encoding='utf-8')
        elif mode == 'range':
            result = [{'range':params['range'],'newText':'y'}]
        elif mode == 'inserts':
            result = [edit(0, 0, 'A'), edit(0, 0, 'B'), edit(0, 0, 'C')]
        elif mode == 'large-preview':
            result = [edit(0, 8, 'x' * 70000)]
        elif mode == 'oversized':
            result = [edit(0, 8, 'x' * (2 * 1024 * 1024))]
        elif mode == 'callback':
            target = (root / 'other.fmtfixture').as_uri()
            send({'jsonrpc':'2.0','id':'formatter-edit','method':'workspace/applyEdit',
                'params':{'edit':{'changes':{target:[edit(0, 4, 'changed')]}}}})
            reply = read()
            (root / 'callback.json').write_text(json.dumps(reply), encoding='utf-8')
    send({'jsonrpc':'2.0','id':message['id'],'result':result})
";

fn fixture(root: &Path, mode: &str) -> Option<(LspTool, asupersync::runtime::Runtime)> {
    let python = ["python3", "python"].into_iter().find(|program| {
        Command::new(program)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    });
    let Some(python) = python else {
        assert!(
            std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
            "Python required for formatting protocol tests"
        );
        eprintln!("SKIP formatting protocol fixture: Python unavailable");
        return None;
    };
    std::fs::write(root.join(".pi-format-root"), "").unwrap();
    std::fs::write(root.join("source.fmtfixture"), "let x=1;\n").unwrap();
    std::fs::write(root.join("other.fmtfixture"), "keep\n").unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([(
                "format-fixture".to_string(),
                LspServerSettings {
                    command: Some(python.to_string()),
                    args: Some(vec![
                        "-I".into(),
                        "-u".into(),
                        "-c".into(),
                        SERVER.into(),
                        mode.into(),
                    ]),
                    extensions: Some(vec![".fmtfixture".into()]),
                    languages: Some(vec!["plaintext".into()]),
                    root_markers: Some(vec![".pi-format-root".into()]),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        }),
        ..Default::default()
    };
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    Some((LspTool::new(root, Some(&config)), runtime))
}

fn run(tool: &LspTool, runtime: &asupersync::runtime::Runtime, input: Value) -> Result<Value> {
    let output = runtime.block_on(tool.execute("format-case", input, None))?;
    assert!(!output.is_error);
    Ok(output.details.expect("structured formatting result"))
}

fn requests(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn document_format_previews_without_writing_and_applies_only_when_requested() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else {
        return;
    };
    let preview = run(
        &tool,
        &runtime,
        json!({"action":"format","file":"source.fmtfixture"}),
    )
    .unwrap();
    assert_eq!(preview["previewOnly"], true);
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["changed"], true);
    assert_eq!(preview["edits"][0]["newText"], "let x = 1;");
    assert_eq!(
        std::fs::read(temp.path().join("source.fmtfixture")).unwrap(),
        b"let x=1;\n"
    );
    let applied = run(
        &tool,
        &runtime,
        json!({"action":"format","file":"source.fmtfixture","apply":true}),
    )
    .unwrap();
    assert_eq!(applied["applied"], true);
    assert_eq!(
        std::fs::read(temp.path().join("source.fmtfixture")).unwrap(),
        b"let x = 1;\n"
    );
    let calls = requests(temp.path());
    let formats: Vec<_> = calls
        .iter()
        .filter(|call| call["method"] == "textDocument/formatting")
        .collect();
    assert_eq!(
        formats.len(),
        2,
        "apply makes a new request, not a stale preview replay"
    );
    assert_eq!(
        formats[0]["params"]["options"],
        json!({"tabSize":4,"insertSpaces":true})
    );
}

#[test]
fn range_format_preserves_utf16_selection_and_caller_options() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "range") else {
        return;
    };
    std::fs::write(temp.path().join("source.fmtfixture"), "😀x\rnext\r\n").unwrap();
    let range = json!({"start":{"line":0,"character":2},"end":{"line":0,"character":3}});
    let opts = json!({"tabSize":2,"insertSpaces":false,"trimTrailingWhitespace":true,"customStyle":"compact"});
    let result = run(&tool, &runtime, json!({
        "action":"format","file":"source.fmtfixture","range":range,"formatOptions":opts,"apply":true
    })).unwrap();
    assert_eq!(result["mode"], "range");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("source.fmtfixture")).unwrap(),
        "😀y\rnext\r\n"
    );
    let calls = requests(temp.path());
    let call = calls
        .iter()
        .find(|call| call["method"] == "textDocument/rangeFormatting")
        .unwrap();
    assert_eq!(call["params"]["range"], range);
    assert_eq!(call["params"]["options"], opts);
}

#[test]
fn invalid_selection_is_rejected_before_server_startup() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "range") else {
        return;
    };
    std::fs::write(temp.path().join("source.fmtfixture"), "😀x").unwrap();
    for range in [
        json!({"start":{"line":0,"character":1},"end":{"line":0,"character":2}}),
        json!({"start":{"line":0,"character":0},"end":{"line":9,"character":0}}),
        json!({"start":{"line":0,"character":3},"end":{"line":0,"character":2}}),
    ] {
        assert!(
            run(
                &tool,
                &runtime,
                json!({"action":"format","file":"source.fmtfixture","range":range})
            )
            .is_err()
        );
    }
    assert!(!temp.path().join("requests.jsonl").exists());
}

#[test]
fn malformed_options_are_not_sent_or_silently_ignored() {
    for raw in [
        json!(true),
        json!([]),
        json!({"tabSize":0}),
        json!({"tabSize":33}),
        json!({"insertSpaces":"yes"}),
        json!({"insertFinalNewline":1}),
        json!({"custom":null}),
        json!({"custom":1.5}),
        json!({"custom":{}}),
    ] {
        assert!(options(Some(&raw)).is_err(), "{raw}");
    }
    assert_eq!(
        options(Some(&json!({"tabSize":8}))).unwrap(),
        json!({"tabSize":8,"insertSpaces":true})
    );
}

#[test]
fn unadvertised_formatting_is_an_explicit_error_without_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "unsupported") else {
        return;
    };
    let error = run(
        &tool,
        &runtime,
        json!({"action":"format","file":"source.fmtfixture","apply":true}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("LSP_FORMAT_UNSUPPORTED"));
    assert!(
        !requests(temp.path())
            .iter()
            .any(|call| call["method"] == "textDocument/formatting")
    );
}

#[test]
fn invalid_or_failed_formatter_responses_never_modify_the_source() {
    for mode in ["invalid", "overlap", "bad-line", "error", "oversized"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else {
            return;
        };
        assert!(
            run(
                &tool,
                &runtime,
                json!({"action":"format","file":"source.fmtfixture","apply":true})
            )
            .is_err(),
            "{mode}"
        );
        assert_eq!(
            std::fs::read(temp.path().join("source.fmtfixture")).unwrap(),
            b"let x=1;\n"
        );
    }
}

#[test]
fn preview_and_apply_reject_source_drift_while_the_server_works() {
    for apply in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), "drift") else {
            return;
        };
        let error = run(
            &tool,
            &runtime,
            json!({"action":"format","file":"source.fmtfixture","apply":apply}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFLICT"));
        assert_eq!(
            std::fs::read(temp.path().join("source.fmtfixture")).unwrap(),
            b"external edit\n"
        );
    }
}

#[test]
fn empty_and_identical_results_do_not_rewrite_the_file() {
    for mode in ["null", "empty", "same"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else {
            return;
        };
        let result = run(
            &tool,
            &runtime,
            json!({"action":"format","file":"source.fmtfixture","apply":true}),
        )
        .unwrap();
        assert_eq!(result["changed"], false);
        assert_eq!(result["applied"], false);
        assert_eq!(result["returnedNull"], mode == "null");
        assert!(
            !requests(temp.path())
                .iter()
                .any(|call| call["method"] == "textDocument/didClose")
        );
        assert_eq!(
            std::fs::read(temp.path().join("source.fmtfixture")).unwrap(),
            b"let x=1;\n"
        );
    }
}

#[test]
fn actual_formatting_keeps_same_position_insert_order() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "inserts") else {
        return;
    };
    run(
        &tool,
        &runtime,
        json!({"action":"format","file":"source.fmtfixture","apply":true}),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("source.fmtfixture")).unwrap(),
        b"ABClet x=1;\n"
    );
}

#[test]
fn preview_limits_never_return_a_partial_replacement_as_an_edit() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "large-preview") else {
        return;
    };
    let result = run(
        &tool,
        &runtime,
        json!({"action":"format","file":"source.fmtfixture"}),
    )
    .unwrap();
    assert_eq!(result["editCount"], 1);
    assert_eq!(result["edits"], json!([]));
    assert_eq!(result["previewTruncated"], true);
    assert_eq!(
        std::fs::read(temp.path().join("source.fmtfixture")).unwrap(),
        b"let x=1;\n"
    );
}

#[test]
fn formatting_does_not_grant_workspace_apply_edit_permission() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "callback") else {
        return;
    };
    run(
        &tool,
        &runtime,
        json!({"action":"format","file":"source.fmtfixture","apply":true}),
    )
    .unwrap();
    let reply: Value =
        serde_json::from_str(&std::fs::read_to_string(temp.path().join("callback.json")).unwrap())
            .unwrap();
    assert_eq!(reply["result"]["applied"], false);
    assert_eq!(
        std::fs::read(temp.path().join("other.fmtfixture")).unwrap(),
        b"keep\n"
    );
}

#[test]
fn a_formatter_timeout_cannot_apply_or_report_a_successful_format() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "hang") else {
        return;
    };
    let error = run(
        &tool,
        &runtime,
        json!({"action":"format","file":"source.fmtfixture","apply":true,"timeout":1}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("LSP_TIMEOUT"));
    assert_eq!(
        std::fs::read(temp.path().join("source.fmtfixture")).unwrap(),
        b"let x=1;\n"
    );
}

#[test]
fn formatter_edits_cannot_round_a_surrogate_interior_into_a_valid_edit() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "surrogate") else {
        return;
    };
    let path = temp.path().join("source.fmtfixture");
    std::fs::write(&path, "😀x").unwrap();
    let error = run(
        &tool,
        &runtime,
        json!({"action":"format","file":"source.fmtfixture","apply":true}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("surrogate"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "😀x");
}

#[test]
fn dropped_formatting_request_cancels_before_a_successor_can_apply() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "hang") else {
        return;
    };
    runtime.block_on(async {
        let mut pending = Box::pin(tool.execute(
            "cancel-format",
            json!({
                "action":"format","file":"source.fmtfixture","apply":true
            }),
            None,
        ));
        let owner = AgentCx::for_current_or_request();
        let mut posted = false;
        for _ in 0..500 {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            if let Ok(log) = std::fs::read_to_string(temp.path().join("requests.jsonl"))
                && log.lines().any(|line| {
                    serde_json::from_str::<Value>(line)
                        .is_ok_and(|frame| frame["method"] == "textDocument/formatting")
                })
            {
                posted = true;
                break;
            }
            owner
                .time()
                .sleep(std::time::Duration::from_millis(10))
                .await;
        }
        assert!(posted, "formatter request reached the protocol peer");
        drop(pending);
        // This response is also a protocol barrier: the peer has processed the
        // preceding cancellation notification before answering the next call.
        tool.execute(
            "after-format",
            json!({
                "action":"request","file":"source.fmtfixture","method":"test/barrier"
            }),
            None,
        )
        .await
        .unwrap();
    });
    let calls = requests(temp.path());
    let posted = calls
        .iter()
        .position(|frame| frame["method"] == "textDocument/formatting")
        .unwrap();
    let cancelled = calls
        .iter()
        .position(|frame| frame["method"] == "$/cancelRequest")
        .unwrap();
    let successor = calls
        .iter()
        .position(|frame| frame["method"] == "test/barrier")
        .unwrap();
    assert!(posted < cancelled && cancelled < successor);
    assert_eq!(calls[cancelled]["params"]["id"], calls[posted]["id"]);
    assert_eq!(
        std::fs::read(temp.path().join("source.fmtfixture")).unwrap(),
        b"let x=1;\n"
    );
}

#[test]
#[cfg(unix)]
fn formatter_application_preserves_executable_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else {
        return;
    };
    let path = temp.path().join("source.fmtfixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
    run(
        &tool,
        &runtime,
        json!({"action":"format","file":"source.fmtfixture","apply":true}),
    )
    .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o751
    );
    assert_eq!(std::fs::read(path).unwrap(), b"let x = 1;\n");
}
