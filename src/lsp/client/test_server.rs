//! Real framed-stdio peer for native client protocol tests. No provider keys,
//! ports or external language-server installations are required.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use asupersync::runtime::{Runtime, RuntimeBuilder};
use serde_json::{Value, json};

use super::LspClient;

pub(in crate::lsp) const SERVER: &str = r"
import json, os, sys, time
capabilities = json.loads(sys.argv[1])
hold_initialize = capabilities.pop('_fixtureHoldInitialize', False)
frames, replies, held = [], {}, []
startup = json.loads(sys.argv[2]) if len(sys.argv) > 2 else []
reverse, initializing = {}, None
def send(value):
    body = json.dumps(value, separators=(',', ':')).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
def next_startup():
    global initializing
    if startup:
        request = startup.pop(0)
        reverse[request['id']] = ('initialize', initializing)
        send(dict(request, jsonrpc='2.0'))
    else:
        send({'jsonrpc':'2.0', 'id':initializing, 'result':{'capabilities':capabilities}})
        initializing = None
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
    # Keep pressure traffic observable without echoing megabytes back through
    # test/frames. All actual protocol payloads remain recorded verbatim.
    if message.get('method') == 'test/outboundPressure':
        frames.append({'method':'test/outboundPressure','payloadBytes':len(body)})
    else:
        frames.append(message)
    method, params = message.get('method'), message.get('params') or {}
    if method is None:
        route = reverse.pop(message.get('id'), None)
        if route is not None:
            kind, owner = route
            if kind == 'initialize':
                next_startup()
            else:
                send({'jsonrpc':'2.0', 'id':owner, 'result':message})
        continue
    if method == 'test/release':
        request_id = held.pop(0)
        send({'jsonrpc':'2.0', 'id':request_id, 'result':params['result']})
        continue
    if 'id' not in message:
        continue
    response = {'jsonrpc':'2.0', 'id':message['id']}
    if method == 'initialize':
        if hold_initialize:
            continue
        if startup:
            initializing = message['id']
            next_startup()
            continue
        response['result'] = {'capabilities':capabilities}
    elif method == 'test/clientRequest':
        reverse[params['id']] = ('request', message['id'])
        send(dict(params, jsonrpc='2.0'))
        continue
    elif method == 'test/frames':
        response['result'] = frames
    elif method == 'test/configure':
        replies.update(params['replies'])
        response['result'] = None
    elif method == 'test/notifications':
        for notification in params['notifications']:
            send(notification)
        response['result'] = None
    elif method == 'test/hang':
        held.append(message['id'])
        continue
    elif method == 'test/stallReads':
        send(dict(response, result=None))
        deadline = time.monotonic() + 30
        while not os.path.exists(params['release']):
            if time.monotonic() >= deadline:
                sys.exit(3)
            time.sleep(0.005)
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
";

pub(super) struct Fixture {
    pub(super) client: LspClient,
    pub(super) runtime: Runtime,
}

/// Release the real peer even when an assertion unwinds while stdin is held.
pub(super) struct StalledReads {
    release: std::path::PathBuf,
}

impl StalledReads {
    pub(super) fn start(client: &LspClient, runtime: &Runtime, root: &Path) -> Self {
        let guard = Self {
            release: root.join(format!("release-{}", uuid::Uuid::new_v4())),
        };
        runtime
            .block_on(client.call(
                "test/stallReads",
                json!({"release":guard.release}),
                Duration::from_secs(5),
            ))
            .expect("peer stopped reading stdin");
        guard
    }
}

impl Drop for StalledReads {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"release");
    }
}

#[allow(clippy::needless_pass_by_value)]
impl Fixture {
    pub(super) fn connect(root: &Path, capabilities: Value) -> Option<Self> {
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            eprintln!("SKIP LSP stdio protocol fixture: Python is not installed");
            return None;
        };
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let args = vec![
            "-u".to_string(),
            "-c".to_string(),
            SERVER.to_string(),
            capabilities.to_string(),
        ];
        let client = runtime
            .block_on(LspClient::connect(
                python,
                &args,
                &[],
                root,
                None,
                None,
                Duration::from_secs(5),
            ))
            .expect("initialize real stdio peer");
        Some(Self { client, runtime })
    }

    pub(super) fn frames(&self) -> Vec<Value> {
        self.runtime
            .block_on(
                self.client
                    .call("test/frames", json!({}), Duration::from_secs(5)),
            )
            .expect("protocol frames")
            .as_array()
            .expect("frame array")
            .clone()
    }

    /// The acknowledgement precedes a bounded pause in reading stdin. Queue
    /// saturation tests therefore do not depend on sleeps or pipe capacity.
    pub(super) fn stall_reads(&self, root: &Path) -> StalledReads {
        StalledReads::start(&self.client, &self.runtime, root)
    }

    /// Wait for the reader to receive an entire notification batch without
    /// draining it through `LspClient::call`'s normal notification polling.
    pub(super) fn notifications(&self, notifications: &[Value]) {
        let (id, response) = self
            .client
            .rpc
            .request(
                "test/notifications",
                json!({"notifications": notifications}),
            )
            .expect("notification batch request");
        self.runtime
            .block_on(crate::lsp::jsonrpc::await_completion(
                response,
                Duration::from_secs(5),
                || self.client.rpc.cancel_request(id),
            ))
            .expect("notification batch acknowledgement")
            .expect("notification batch succeeded");
    }

    pub(super) fn configure(&self, replies: Value) {
        self.runtime
            .block_on(self.client.call(
                "test/configure",
                json!({"replies":replies}),
                Duration::from_secs(5),
            ))
            .expect("configure protocol replies");
    }
}
