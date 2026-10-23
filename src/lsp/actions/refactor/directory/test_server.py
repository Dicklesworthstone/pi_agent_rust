"""Deterministic LSP peer for native directory-rename integration tests.

This is a protocol fixture, not a real language server. Logs are outside the
moving tree, so observing the workflow cannot invalidate its tree snapshot.
"""
import json
import pathlib
import sys
import time
import urllib.parse
import urllib.request

ROOT = pathlib.Path.cwd()
MODE = sys.argv[1]
SECONDARY = len(sys.argv) > 2 and sys.argv[2] == "secondary"
PREFIX = "secondary-" if SECONDARY else ""
SAME_SOURCE = len(sys.argv) > 3 and sys.argv[3] == "same-source"
EXTENSION = "other" if SECONDARY and not SAME_SOURCE else "refactor"
MAX_FRAME = 2 * 1024 * 1024
DOCUMENTS = {}


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


def replacement(uri=None):
    line, column = 0, 0
    if MODE == "fresh":
        text = DOCUMENTS[uri] if uri in DOCUMENTS else native(uri).read_text(encoding="utf-8")
        prefix = text[:text.index("old")].replace("\r\n", "\n").replace("\r", "\n")
        line = prefix.count("\n")
        column = len(prefix.split("\n")[-1].encode("utf-16-le")) // 2
    return [{"range": {"start": {"line": line, "character": column},
                       "end": {"line": line, "character": column + 3}}, "newText": "new"}]


def native(uri):
    return pathlib.Path(urllib.request.url2pathname(urllib.parse.urlsplit(uri).path))


while (message := receive()) is not None:
    with (ROOT / f"{PREFIX}requests.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps(message) + "\n")
    method = message.get("method")
    if method == "textDocument/didOpen":
        doc = message["params"]["textDocument"]
        DOCUMENTS[doc["uri"]] = doc["text"]
    elif method == "textDocument/didChange":
        params = message["params"]
        DOCUMENTS[params["textDocument"]["uri"]] = params["contentChanges"][0]["text"]
    elif method == "textDocument/didClose":
        DOCUMENTS.pop(message["params"]["textDocument"]["uri"], None)
    if method == "workspace/didRenameFiles":
        new = ROOT / "nested/new"
        single = not new.is_dir()
        pair = message["params"]["files"][0]
        target = native(pair["newUri"]) if single else new
        evidence = {
            "oldExists": (native(pair["oldUri"]) if single else ROOT / "old").exists(),
            "newExists": target.exists(),
            "newText": (target if single else new / "lib.refactor").read_text(encoding="utf-8"),
            "imports": (ROOT / "imports.refactor").read_text(encoding="utf-8"),
            "otherImports": (ROOT / "imports.other").read_text(encoding="utf-8") if (ROOT / "imports.other").exists() else None,
            "binary": None if single else list((new / "asset.bin").read_bytes()),
            "emptyDirectory": not single and (new / "empty").is_dir(),
            "pairs": message["params"]["files"],
        }
        (ROOT / f"{PREFIX}notification.json").write_text(json.dumps(evidence), encoding="utf-8")
    if "id" not in message:
        continue
    result = None
    if method == "initialize":
        capabilities = {"textDocumentSync": 1}
        if MODE != "unregistered":
            will = registration("folder", "**/old") if MODE == "folder" else registration("file", f"**/*.{EXTENSION}")
            capabilities["workspace"] = {"fileOperations": {
                "willRename": will, "didRename": registration("file", f"**/*.{EXTENSION}")}}
        result = {"capabilities": capabilities}
    elif method == "workspace/willRenameFiles":
        if MODE == "error":
            send({"jsonrpc": "2.0", "id": message["id"],
                  "error": {"code": -32603, "message": "import preparation failed"}})
            continue
        if MODE == "timeout":
            time.sleep(5)
        paths = ([ROOT / "old/secondary.other", ROOT / "imports.other"] if SECONDARY else
                 [ROOT / "old/lib.refactor", ROOT / "old/sub/use.refactor", ROOT / "imports.refactor"])
        if SAME_SOURCE:
            paths[0] = ROOT / "old/lib.refactor"
        result = {"changes": {path.as_uri(): replacement(path.as_uri()) for path in paths}}
        if MODE == "invalid":
            result["changes"][paths[-1].as_uri()][0]["range"]["end"]["line"] = 999
        elif MODE == "escape":
            result["changes"][(ROOT.parent / "outside.refactor").as_uri()] = replacement()
        elif MODE == "drift":
            (ROOT / "old/asset.bin").write_bytes(b"external")
        elif MODE in ("shared", "conflict"):
            result["changes"][(ROOT / "shared.refactor").as_uri()] = [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "newText": "Y" if MODE == "conflict" else "X"}]
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
