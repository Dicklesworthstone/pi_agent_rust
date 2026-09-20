"""Independent framed-stdio peer for review/approval tool tests, not a real LSP."""
import json
import pathlib
import sys
from urllib.parse import unquote, urlparse

root = pathlib.Path.cwd()
mode = sys.argv[1]
versions = {}
counts = {"renames": 0, "will": 0, "did": 0, "commands": 0}
notifications = []
unsolicited_applied = None
pending = None


def send(value):
    body = json.dumps(value).encode("utf-8")
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(body)).encode("ascii") + body)
    sys.stdout.buffer.flush()


def file_path(uri):
    path = unquote(urlparse(uri).path)
    if sys.platform == "win32" and path.startswith("/") and len(path) > 2 and path[2] == ":":
        path = path[1:]
    return pathlib.Path(path)


def edit(uri, new_text, version=None):
    return {"textDocument": {"uri": uri, "version": version}, "edits": [{
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
        "newText": new_text
    }]}


def answer(request, replacement):
    if mode == "error":
        send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32603, "message": "refactor engine failed"}})
        return
    if mode == "malformed":
        result = {"documentChanges": False}
    else:
        params = request["params"]
        source = params["textDocument"]["uri"] if "textDocument" in params else params["files"][0]["oldUri"]
        version = versions[source]
        if mode == "stale_version":
            version += 1
        if mode == "oversized":
            replacement = "x" * (230 * 1024)
        steps = [edit(source, replacement, version), edit((root / "sibling.preview").as_uri(), replacement)]
        if mode == "guard_only":
            steps = steps[1:]
        if mode == "escape":
            steps.append(edit((root.parent / "outside.preview").as_uri(), "wrong"))
        result = {"documentChanges": steps}
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})


while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\n", b"\r\n"):
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    method = message.get("method")
    with (root / "preview-requests.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps({"case": mode, "method": method, "id": message.get("id")}) + "\n")
    if message.get("id") == "unsolicited-1" and method is None:
        unsolicited_applied = message["result"]["applied"]
        request, replacement = pending
        pending = None
        answer(request, replacement)
        continue
    if method == "exit":
        break
    params = message.get("params") or {}
    if method == "textDocument/didOpen":
        doc = params["textDocument"]
        versions[doc["uri"]] = doc["version"]
    if method == "workspace/didRenameFiles":
        counts["did"] += 1
        old = file_path(params["files"][0]["oldUri"])
        new = file_path(params["files"][0]["newUri"])
        notifications.append({"oldExists": old.exists(), "newExists": new.exists(),
            "newText": new.read_text(encoding="utf-8") if new.exists() else None,
            "siblingText": (root / "sibling.preview").read_text(encoding="utf-8")})
    if "id" not in message:
        continue
    if method == "initialize":
        caps = {"textDocumentSync": 1, "renameProvider": True}
        if mode != "unregistered":
            registration = {"filters": [{"scheme": "file", "pattern": {"glob": "**/*.preview", "matches": "file"}}]}
            caps["workspace"] = {"fileOperations": {"willRename": registration, "didRename": registration}}
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": caps}})
    elif method in ("textDocument/rename", "workspace/willRenameFiles"):
        counts["renames" if method == "textDocument/rename" else "will"] += 1
        replacement = params.get("newName", "moved")
        if mode == "unsolicited":
            pending = (message, replacement)
            send({"jsonrpc": "2.0", "id": "unsolicited-1", "method": "workspace/applyEdit", "params": {
                "edit": {"documentChanges": [edit((root / "sibling.preview").as_uri(), "uninvited")]}
            }})
        else:
            answer(message, replacement)
    elif method == "test/barrier":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            **counts, "notifications": notifications, "unsolicitedApplied": unsolicited_applied
        }})
    else:
        if method == "workspace/executeCommand":
            counts["commands"] += 1
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
