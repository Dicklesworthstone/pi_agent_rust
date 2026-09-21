"""Independent framed peer for reviewed formatter tests, not a language server."""
import json
import pathlib
import sys

root = pathlib.Path.cwd()
startup_mode = sys.argv[1]
formats = 0


def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise SystemExit(0)
        if line in (b'\r\n', b'\n'):
            break
        key, value = line.decode('ascii').split(':', 1)
        headers[key.lower()] = value.strip()
    return json.loads(sys.stdin.buffer.read(int(headers['content-length'])))


def send(message):
    body = json.dumps(message).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()


def edit(text, span=None):
    return {'range': span or {'start': {'line': 0, 'character': 0},
                            'end': {'line': 0, 'character': 8}}, 'newText': text}


while True:
    message = read()
    with (root / 'review-requests.jsonl').open('a', encoding='utf-8') as log:
        log.write(json.dumps(message) + '\n')
    method = message.get('method')
    if method == 'exit':
        raise SystemExit(0)
    if 'id' not in message:
        continue
    params = message.get('params') or {}
    result = None
    if method == 'initialize':
        if startup_mode == 'startup-hang':
            continue
        result = {'capabilities': {
            'textDocumentSync': 0 if startup_mode == 'nosync' else 1,
            'documentFormattingProvider': True, 'documentRangeFormattingProvider': True}}
    elif method in ('textDocument/formatting', 'textDocument/rangeFormatting'):
        formats += 1
        mode = json.loads((root / 'review-mode.json').read_text(encoding='utf-8'))
        if mode == 'hang':
            continue
        if mode == 'error':
            send({'jsonrpc': '2.0', 'id': message['id'],
                  'error': {'code': -32603, 'message': 'formatter deliberately refused'}})
            continue
        # Recomputing instead of approving the first response is observably wrong.
        result = [edit('let x = 1;' if formats == 1 else 'RECOMPUTED')]
        if method == 'textDocument/rangeFormatting':
            result = [edit('formatted', params['range'])]
        if mode == 'empty':
            result = []
        elif mode == 'null':
            result = None
        elif mode == 'same':
            result = [edit('let x=1;')]
        elif mode == 'invalid':
            result = {'changes': {params['textDocument']['uri']: result}}
        elif mode == 'overlap':
            result += result
        elif mode == 'summary':
            result = [edit('x' * 70000)]
        elif mode == 'preview-limit':
            result = [edit('x' * (210 * 1024))]
        elif mode == 'response-limit':
            result = [edit('x' * (2 * 1024 * 1024))]
        elif mode == 'drift':
            (root / 'source.reviewfmt').write_text('external edit\n', encoding='utf-8')
        elif mode == 'callback':
            send({'jsonrpc': '2.0', 'id': 'format-probe', 'method': 'workspace/applyEdit',
                  'params': {'edit': {'changes': {(root / 'other.reviewfmt').as_uri(): [edit('bad')]}}}})
            while True:
                reply = read()
                if reply.get('id') == 'format-probe' and 'method' not in reply:
                    break
            (root / 'format-probe.json').write_text(json.dumps(reply), encoding='utf-8')
    elif method == 'test/count':
        result = formats
    send({'jsonrpc': '2.0', 'id': message['id'], 'result': result})
