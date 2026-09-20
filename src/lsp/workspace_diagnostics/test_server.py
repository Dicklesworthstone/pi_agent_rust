"""Framed subprocess peer for active workspace diagnostics."""
import json
import pathlib
import sys

root = pathlib.Path.cwd()
mode = sys.argv[1]
documents = {}


def send(message):
    body = json.dumps(message).encode("utf-8")
    sys.stdout.buffer.write(
        ("Content-Length: %d\r\n\r\n" % len(body)).encode("ascii") + body
    )
    sys.stdout.buffer.flush()


def items(uri):
    text = documents[uri]["text"]
    if "broken" not in text:
        return []
    message = "broken declaration"
    if mode == "oversize":
        message = "x" * (220 * 1024)
    return [{
        "range": {"start": {"line": 0, "character": 0},
                  "end": {"line": 0, "character": 1}},
        "severity": 1, "message": message,
    }]


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
    method = message.get("method")
    params = message.get("params") or {}
    with (root / "requests.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps({"method": method, "params": params}) + "\n")
    if method is None:
        if message.get("id") == "unsolicited-edit":
            (root / "edit-response.json").write_text(json.dumps(message["result"]), encoding="utf-8")
        continue
    if method == "exit":
        sys.exit(0)
    if method == "initialized":
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"quiescent": True}})
    if method == "textDocument/didOpen":
        doc = params["textDocument"]
        documents[doc["uri"]] = doc.copy()
    elif method == "textDocument/didChange":
        doc = params["textDocument"]
        documents[doc["uri"]]["version"] = doc["version"]
        documents[doc["uri"]]["text"] = params["contentChanges"][-1]["text"]
    if method in ("textDocument/didOpen", "textDocument/didChange") and mode == "push":
        doc = params["textDocument"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
              "params": {"uri": doc["uri"], "version": doc["version"],
                         "diagnostics": items(doc["uri"])}})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"quiescent": True}})
    if "id" not in message:
        continue
    result = None
    if method == "initialize":
        capabilities = {"textDocumentSync": {"openClose": True, "change": 1, "save": True}}
        if mode not in ("push", "pending"):
            capabilities["diagnosticProvider"] = {
                "identifier": "scan", "interFileDependencies": True,
                "workspaceDiagnostics": False,
            }
        result = {"capabilities": capabilities}
    elif method == "textDocument/diagnostic":
        uri = params["textDocument"]["uri"]
        if mode == "unsolicited" and uri.endswith("/a.scan"):
            send({"jsonrpc": "2.0", "id": "unsolicited-edit", "method": "workspace/applyEdit",
                  "params": {"edit": {"changes": {uri: [{
                      "range": {"start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 6}},
                      "newText": "changed",
                  }]}}}})
        if mode == "hang":
            continue
        if mode == "error" and uri.endswith("/a.scan"):
            send({"jsonrpc": "2.0", "id": message["id"],
                  "error": {"code": -32603, "message": "fixture diagnostic failure"}})
            continue
        if mode == "drift" and uri.endswith("/b.scan"):
            (root / "a.scan").write_text("external change\n", encoding="utf-8")
        result = {"kind": "full", "resultId": uri + ":" + str(documents[uri]["version"]),
                  "items": items(uri)}
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    if method == "textDocument/diagnostic":
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"quiescent": True}})
