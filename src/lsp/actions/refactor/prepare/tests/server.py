"""Bounded framed peer for rename-target tests, not a semantic language server."""
import json
import pathlib
import sys

root = pathlib.Path.cwd()
mode = sys.argv[1]
documents = {}


def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()
    size = int(headers["content-length"])
    assert 0 <= size <= 4 * 1024 * 1024
    raw = sys.stdin.buffer.read(size)
    assert len(raw) == size
    message = json.loads(raw)
    with (root / "requests.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps(message) + "\n")
    return message


def send(message):
    body = json.dumps(message).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body)
    sys.stdout.buffer.flush()


def span(start, end, line=1):
    return {"start": {"line": line, "character": start},
            "end": {"line": line, "character": end}}


def edit(uri, selected, replacement):
    return {"textDocument": {"uri": uri, "version": documents.get(uri)},
            "edits": [{"range": selected, "newText": replacement}]}


while True:
    message = read()
    if message is None:
        break
    method = message.get("method")
    params = message.get("params") or {}
    if method == "exit":
        break
    if method == "textDocument/didOpen":
        doc = params["textDocument"]
        documents[doc["uri"]] = doc["version"]
    if method == "initialized":
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"quiescent": True}})
    if "id" not in message:
        continue
    result = None
    if method == "initialize":
        if mode == "init-stall":
            (root / "started").write_text("initialize", encoding="utf-8")
            continue
        caps = {"textDocumentSync": 1, "renameProvider": {"prepareProvider": True}}
        if mode == "plain":
            caps["renameProvider"] = True
        elif mode == "absent":
            del caps["renameProvider"]
        elif mode == "disabled":
            caps["renameProvider"] = False
        elif mode == "bad-capability":
            caps["renameProvider"]["prepareProvider"] = "true"
        elif mode == "utf8":
            caps["positionEncoding"] = "utf-8"
        result = {"capabilities": caps}
    elif method == "textDocument/prepareRename":
        cursor = params["position"]
        selected = span(12, 17) if cursor["character"] >= 12 else span(4, 9)
        result = {"range": selected, "placeholder": "binding"}
        if mode == "bare-range":
            result = selected
        elif mode == "refuse":
            result = None
        elif mode == "error":
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32602, "message": "cannot rename this binding"}})
            continue
        elif mode == "default":
            result = {"defaultBehavior": True}
        elif mode == "split":
            result = {"range": span(4, 5, 0), "placeholder": "bad"}
        elif mode == "unrelated":
            result = {"range": span(4, 9), "placeholder": "wrong"}
        elif mode == "empty":
            result = {"range": span(14, 14), "placeholder": "empty"}
        elif mode == "large-target":
            result["placeholder"] = "x" * (16 * 1024 + 1)
        elif mode == "oversized":
            result["extra"] = "x" * (2 * 1024 * 1024 + 1)
        elif mode == "prepare-drift":
            (root / "source.target").write_text("external source\n", encoding="utf-8")
        elif mode == "prepare-stall":
            (root / "started").write_text("prepareRename", encoding="utf-8")
            continue
        elif mode == "probe":
            sibling = (root / "sibling.target").as_uri()
            send({"jsonrpc": "2.0", "id": "edit-probe", "method": "workspace/applyEdit",
                  "params": {"edit": {"documentChanges": [edit(sibling, span(0, 5, 0), "unauthorized")]}}})
            response = read()
            assert response["id"] == "edit-probe"
            (root / "probe.json").write_text(json.dumps(response), encoding="utf-8")
    elif method == "textDocument/rename":
        selected = span(12, 17) if params["position"]["character"] >= 12 else span(4, 9)
        result = {"documentChanges": [
            edit(params["textDocument"]["uri"], selected, params["newName"]),
            edit((root / "sibling.target").as_uri(), span(0, 5, 0), params["newName"]),
        ]}
        if mode == "rename-drift":
            (root / "source.target").write_text("external source\n", encoding="utf-8")
    elif method == "test/barrier":
        result = "ready"
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
