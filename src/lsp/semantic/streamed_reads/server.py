"""Framed-stdio peer for production streamed semantic-read tests.

Only the wire peer is scripted. Tests exercise Pi's real tool, client, transport,
partial-result owner, semantic validators and output rendering.
"""
import copy
import json
import pathlib
import sys

mode, root_arg = sys.argv[1:3]
root = pathlib.Path(root_arg)
frames_path = root / "streamed-read-frames.jsonl"
calls = {}
old_tokens = {}


def send(value):
    body = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(body)).encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


def reply(message, result):
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})


def error(message, code, text):
    send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": code, "message": text}})


def progress(token, value):
    send({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": token, "value": value}})


def span(start, end):
    return {"start": {"line": 0, "character": start}, "end": {"line": 0, "character": end}}


def values(method):
    first_uri = (root.resolve() / "metadata-target.rs").as_uri()
    second_uri = "untitled:virtual-target"
    if method == "workspace/symbol":
        return [
            {"name": "alpha", "kind": 12, "location": {"uri": first_uri, "range": span(0, 1)}},
            {"name": "界", "kind": 12, "location": {"uri": second_uri},
             "data": {"ticket": 7, "nested": [True, None, "$(not executed)"]}},
        ]
    if method == "textDocument/references" or mode == "single":
        return [{"uri": first_uri, "range": span(0, 1)},
                {"uri": second_uri, "range": span(1, 2)}]
    return [
        {"targetUri": uri, "targetRange": span(0, 7),
         "targetSelectionRange": span(1, 3), "originSelectionRange": span(0, 2)}
        for uri in [first_uri, second_uri]
    ]


while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    with frames_path.open("a", encoding="utf-8") as log:
        log.write(json.dumps(message, ensure_ascii=False) + "\n")
    method = message.get("method")
    params = message.get("params") or {}
    if method == "exit":
        break
    if method == "initialized":
        if mode != "warmup":
            send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                  "params": {"quiescent": True}})
        continue
    if "id" not in message:
        continue
    if method == "initialize":
        reply(message, {"capabilities": {
            "textDocumentSync": 1, "positionEncoding": "utf-16",
            "workspaceSymbolProvider": {"resolveProvider": True},
            "referencesProvider": True, "definitionProvider": True,
            "typeDefinitionProvider": True, "implementationProvider": True,
            "hoverProvider": True,
        }})
        continue
    if method == "shutdown":
        reply(message, None)
        continue
    if method == "textDocument/hover":
        if "partialResultToken" in params:
            error(message, -32602, "hover must not negotiate array fragments")
        else:
            reply(message, {"contents": {"kind": "plaintext", "value": "still usable"}})
        continue
    if method == "workspaceSymbol/resolve":
        result = copy.deepcopy(params)
        result["location"]["range"] = span(2, 4)
        reply(message, result)
        continue
    if method not in ("workspace/symbol", "textDocument/references", "textDocument/definition",
                      "textDocument/typeDefinition", "textDocument/implementation"):
        error(message, -32601, "unexpected request")
        continue
    token = params.get("partialResultToken")
    if not isinstance(token, str) or not token:
        error(message, -32602, "expected a partial-result token")
        continue
    calls[method] = calls.get(method, 0) + 1
    attempt = calls[method]
    batch = values(method)
    if mode == "legacy":
        reply(message, batch)
        continue
    if mode == "single":
        reply(message, batch[0])
        continue
    if mode == "legacy-null":
        reply(message, None)
        continue
    if mode == "warmup" and attempt == 1:
        progress(token, [])
        reply(message, None)
        continue
    if mode == "retry" and attempt == 1:
        old_tokens[method] = token
        progress(token, [{"uri": "untitled:discarded-attempt", "range": span(0, 1)}])
        error(message, -32801, "index changed")
        continue
    if mode == "retry":
        progress(old_tokens[method], [{"uri": "untitled:late-old-attempt", "range": span(0, 1)}])
    # Foreign and work-done progress must not be interpreted as an array part.
    progress("foreign-work-done", {"kind": "report", "percentage": 70})
    if mode == "too-many":
        progress(token, [batch[0]] * 2048)
        progress(token, [batch[0]] * 2049)
        reply(message, None)
        continue
    if mode == "oversized":
        large = copy.deepcopy(batch[0])
        large["extra"] = "x" * (2 * 1024 * 1024)
        progress(token, [large])
        reply(message, None)
        continue
    progress(token, [batch[0]])
    if mode == "stall":
        continue
    if mode == "error":
        error(message, -32603, "search failed after progress")
        continue
    if mode == "malformed-fragment":
        progress(token, {"items": [batch[1]]})
        reply(message, None)
        continue
    if mode == "nonempty-terminal":
        reply(message, [batch[1]])
        continue
    if mode == "bad-tail":
        if method == "workspace/symbol":
            batch[1]["kind"] = 0
        elif method == "textDocument/references":
            batch[1]["range"] = span(7, 2)
        else:
            batch[1]["targetSelectionRange"] = span(1, 20)
    progress(token, [])
    progress(token, [batch[1]])
    if mode == "source-change":
        (root / "anchor.pistream").write_text("fn changed() {}\n", encoding="utf-8")
    reply(message, [] if mode == "terminal-empty" else None)
