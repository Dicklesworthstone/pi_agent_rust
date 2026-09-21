"""Framed workspace-symbol peer; fixtures are not language-analysis evidence."""
import copy
import json
import pathlib
import sys
import time

root = pathlib.Path.cwd()
mode = sys.argv[1]
pending = None
anchor = (root / 'anchor.pisymbol').as_uri()


def send(message):
    body = json.dumps(message, ensure_ascii=False).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()


def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise EOFError()
        if line in (b'\r\n', b'\n'):
            break
        key, value = line.decode('ascii').split(':', 1)
        headers[key.lower()] = value.strip()
    value = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    with (root / 'symbol-requests.jsonl').open('a', encoding='utf-8') as log:
        log.write(json.dumps(value, ensure_ascii=False) + '\n')
    return value


def reply(message, result):
    send({'jsonrpc': '2.0', 'id': message['id'], 'result': result})


def error(message):
    send({'jsonrpc': '2.0', 'id': message['id'], 'error': {'code': -32603, 'message': 'symbol provider failed'}})


def span(index=0):
    return {'start': {'line': index, 'character': 2}, 'end': {'line': index, 'character': 8}}


def symbol(index, concrete=False):
    value = {'name': 'target😀', 'kind': 12, 'containerName': 'Module%d' % index,
             'location': {'uri': (root / 'unopened.pisymbol').as_uri()},
             'data': {'ticket': index, 'literal': '$(not executed)', 'unicode': '界'}}
    if index == 2:
        value['location']['uri'] = 'https://example.invalid/unopened.pisymbol'
    if concrete:
        value['location']['range'] = span(index)
    return value


def resolution(original):
    value = copy.deepcopy(original)
    value['location']['range'] = span(original['data']['ticket'])
    if mode == 'changed':
        value['name'] = 'different'
    elif mode == 'changed-uri':
        value['location']['uri'] = 'file:///other.pisymbol'
    elif mode == 'dropped-data':
        del value['data']
    elif mode == 'unresolved':
        del value['location']['range']
    return value


def probe():
    send({'jsonrpc': '2.0', 'id': 'unapproved-symbol-edit', 'method': 'workspace/applyEdit',
          'params': {'edit': {'changes': {anchor: [{'range': span(), 'newText': 'bad'}]}}}})
    while True:
        response = read()
        if response.get('id') == 'unapproved-symbol-edit':
            assert response['result']['applied'] is False
            return


while True:
    try:
        message = read()
    except EOFError:
        break
    method = message.get('method')
    params = message.get('params') or {}
    if method == 'exit':
        break
    if method == 'test/release' and pending:
        request, original = pending
        reply(request, resolution(original))
        pending = None
    if 'id' not in message:
        continue
    if method == 'initialize':
        assert params['capabilities']['workspace']['symbol']['resolveSupport']['properties'] == ['location.range']
        if mode == 'hang-initialize':
            continue
        options = True if mode == 'legacy' else {'resolveProvider': True}
        if mode == 'disabled':
            options = False
        caps = {'textDocumentSync': 1, 'workspaceSymbolProvider': options, 'documentSymbolProvider': True}
        if mode == 'encoding':
            caps['positionEncoding'] = 'utf-8'
        reply(message, {'capabilities': caps})
    elif method == 'workspace/symbol':
        (root / 'workspace-search-started').write_text('ready', encoding='utf-8')
        if mode == 'stall-search':
            continue
        if mode == 'search-error':
            error(message)
            continue
        if mode == 'shared-budget':
            time.sleep(0.65)
        if mode == 'probe':
            probe()
        if mode == 'drift-search':
            (root / 'anchor.pisymbol').write_text('external\n', encoding='utf-8')
        values = [symbol(0, True), symbol(1), symbol(2)]
        if mode == 'legacy':
            values = [symbol(i, True) for i in range(3)]
        elif mode == 'null':
            values = None
        elif mode == 'empty':
            values = []
        elif mode == 'invalid-last':
            values[-1]['location']['range'] = {'start': {'line': 9, 'character': 0}, 'end': {'line': 0, 'character': 0}}
        elif mode == 'oversized-item':
            values[-1]['data'] = 'x' * 65536
        elif mode == 'too-many':
            values = [symbol(0, True)] * 4097
        elif mode == 'byte-output':
            values = [symbol(i, True) for i in range(4)]
            for value in values:
                value['data']['large'] = 'x' * 60000
        reply(message, values)
    elif method == 'workspaceSymbol/resolve':
        (root / 'workspace-resolve-started').write_text('ready', encoding='utf-8')
        if mode == 'stall-resolve':
            pending = (message, params)
            continue
        if mode == 'resolve-error':
            error(message)
            continue
        if mode == 'shared-budget':
            time.sleep(0.65)
        if mode == 'drift-resolve':
            (root / 'anchor.pisymbol').write_text('external\n', encoding='utf-8')
        reply(message, resolution(params))
    elif method == 'textDocument/documentSymbol':
        reply(message, [{'name': 'document_only', 'kind': 12, 'range': span(), 'selectionRange': span()}])
    else:
        reply(message, None)
