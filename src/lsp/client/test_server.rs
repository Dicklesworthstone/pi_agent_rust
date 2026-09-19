//! Real framed-stdio peer for native client protocol tests. No provider keys,
//! ports or external language-server installations are required.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use asupersync::runtime::{Runtime, RuntimeBuilder};
use serde_json::{Value, json};

use super::LspClient;

const SERVER: &str = r#"
import json, sys
capabilities = json.loads(sys.argv[1])
frames, replies, held = [], {}, []
def send(value):
    body = json.dumps(value, separators=(',', ':')).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b'\r\n', b'\n'):
            break
        key, value = line.decode('ascii').split(':', 1)
        headers[key.lower()] = value.strip()
    body = sys.stdin.buffer.read(int(headers['content-length']))
    message = json.loads(body)
    frames.append(message)
    method, params = message.get('method'), message.get('params') or {}
    if method == 'test/release':
        request_id = held.pop(0)
        send({'jsonrpc':'2.0', 'id':request_id, 'result':params['result']})
        continue
    if 'id' not in message:
        continue
    response = {'jsonrpc':'2.0', 'id':message['id']}
    if method == 'initialize':
        response['result'] = {'capabilities':capabilities}
    elif method == 'test/frames':
        response['result'] = frames
    elif method == 'test/configure':
        replies.update(params['replies'])
        response['result'] = None
    elif method == 'test/hang':
        held.append(message['id'])
        continue
    elif method == 'test/success':
        response['result'] = {'ok':True}
    elif replies.get(method):
        reply = replies[method].pop(0)
        if reply.get('hold'):
            held.append(message['id'])
            continue
        response.update(reply)
    else:
        response['error'] = {'code':-32601, 'message':'test peer has no response configured'}
    send(response)
"#;

pub(super) struct Fixture {
    pub(super) client: LspClient,
    pub(super) runtime: Runtime,
}

impl Fixture {
    pub(super) fn connect(root: &Path, capabilities: Value) -> Option<Self> {
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program).arg("--version")
                .stdout(Stdio::null()).stderr(Stdio::null())
                .status().is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            eprintln!("SKIP LSP stdio protocol fixture: Python is not installed");
            return None;
        };
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let args = vec!["-u".to_string(), "-c".to_string(), SERVER.to_string(), capabilities.to_string()];
        let client = runtime.block_on(LspClient::connect(
            python, &args, &[], root, None, Duration::from_secs(5),
        )).expect("initialize real stdio peer");
        Some(Self { client, runtime })
    }

    pub(super) fn frames(&self) -> Vec<Value> {
        self.runtime.block_on(self.client.call(
            "test/frames", json!({}), Duration::from_secs(5),
        )).expect("protocol frames").as_array().expect("frame array").clone()
    }

    pub(super) fn configure(&self, replies: Value) {
        self.runtime.block_on(self.client.call(
            "test/configure", json!({"replies":replies}), Duration::from_secs(5),
        )).expect("configure protocol replies");
    }
}
