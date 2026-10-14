"""Deterministic LSP peer for native directory-rename integration tests.

This is a protocol fixture, not a real language server. Logs are outside the
moving tree, so observing the workflow cannot invalidate its tree snapshot.
"""
import json
import pathlib
import sys
import time

ROOT = pathlib.Path.cwd()
MODE = sys.argv[1]
MAX_FRAME = 2 * 1024 * 1024


def send(message):
    body = json.dumps(message).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body)
    sys.stdout.buffer.flush()


def receive():
    length = None
    for _ in range(32):
        line = sys.stdin.buffer.readline(8193)
        if not line:
            return None
        if len(line) > 8192:
            raise ValueError("oversized header")
        if line in (b"\n", b"\r\n"):
            break
        key, value = line.decode("ascii").split(":", 1)
        if key.lower() == "content-length":
            length = int(value.strip())
    else:
        raise ValueError("too many headers")
    if length is None or not 0 < length <= MAX_FRAME:
        raise ValueError("invalid frame length")
    body = sys.stdin.buffer.read(length)
    if len(body) != length:
        raise ValueError("truncated frame")
    return json.loads(body)


def registration(kind, glob):
    return {"filters": [{"scheme": "file", "pattern": {"glob": glob, "matches": kind}}]}


def replacement():
    return [{"range": {"start": {"line": 0, "character": 0},
                       "end": {"line": 0, "character": 3}}, "newText": "new"}]


while (message := receive()) is not None:
    with (ROOT / "requests.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps(message) + "\n")
    method = message.get("method")
    if method == "workspace/didRenameFiles":
        new = ROOT / "nested/new"
        evidence = {
            "oldExists": (ROOT / "old").exists(),
            "newExists": new.is_dir(),
            "newText": (new / "lib.refactor").read_text(encoding="utf-8"),
            "imports": (ROOT / "imports.refactor").read_text(encoding="utf-8"),
            "binary": list((new / "asset.bin").read_bytes()),
            "emptyDirectory": (new / "empty").is_dir(),
            "pairs": message["params"]["files"],
        }
        (ROOT / "notification.json").write_text(json.dumps(evidence), encoding="utf-8")
    if "id" not in message:
        continue
    result = None
    if method == "initialize":
        capabilities = {"textDocumentSync": 1}
        if MODE != "unregistered":
            will = registration("folder", "**/old") if MODE == "folder" else registration("file", "**/*.refactor")
            capabilities["workspace"] = {"fileOperations": {
                "willRename": will, "didRename": registration("file", "**/*.refactor")}}
        result = {"capabilities": capabilities}
    elif method == "workspace/willRenameFiles":
        if MODE == "error":
            send({"jsonrpc": "2.0", "id": message["id"],
                  "error": {"code": -32603, "message": "import preparation failed"}})
            continue
        if MODE == "timeout":
            time.sleep(5)
        paths = [ROOT / "old/lib.refactor", ROOT / "old/sub/use.refactor", ROOT / "imports.refactor"]
        result = {"changes": {path.as_uri(): replacement() for path in paths}}
        if MODE == "invalid":
            result["changes"][paths[-1].as_uri()][0]["range"]["end"]["line"] = 999
        elif MODE == "escape":
            result["changes"][(ROOT.parent / "outside.refactor").as_uri()] = replacement()
        elif MODE == "drift":
            (ROOT / "old/asset.bin").write_bytes(b"external")
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
