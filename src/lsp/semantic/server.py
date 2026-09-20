"""Independent framed-stdio peer for native semantic inspection tests."""
import json
import pathlib
import sys

root = pathlib.Path.cwd()
mode = sys.argv[1]
pending = None


def send(value):
    body = json.dumps(value, ensure_ascii=False).encode("utf-8")
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(body)).encode("ascii") + body)
    sys.stdout.buffer.flush()


def signature():
    return {"signatures": [
        {"label": "call(😀: Text, flag: bool)", "parameters": [
            {"label": [5, 13], "documentation": "Unicode argument"},
            {"label": "flag: bool", "documentation": {"kind": "markdown", "value": "Whether to enable"}}
        ], "activeParameter": 1},
        {"label": "call(value: Number)", "parameters": [{"label": "value: Number"}]}
    ], "activeSignature": 0, "activeParameter": 0}


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
    with (root / "semantic-requests.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps(message, ensure_ascii=False) + "\n")
    method = message.get("method")
    if method == "exit":
        sys.exit(0)
    if message.get("id") == "unexpected-edit" and method is None:
        send(pending)
        pending = None
        continue
    if "id" not in message:
        continue
    response = {"jsonrpc": "2.0", "id": message["id"], "result": None}
    if method == "initialize":
        if mode == "hang-initialize":
            continue
        caps = {"textDocumentSync": 1, "signatureHelpProvider": {}}
        if mode == "unsupported":
            caps.pop("signatureHelpProvider")
        if mode == "encoding":
            caps["positionEncoding"] = "utf-8"
        response["result"] = {"capabilities": caps}
    elif method == "textDocument/signatureHelp":
        if mode == "hang":
            continue
        response["result"] = signature()
        if mode == "null":
            response["result"] = None
        elif mode == "empty":
            response["result"] = {"signatures": []}
        elif mode == "malformed":
            response["result"]["signatures"][0]["parameters"][0]["label"] = [5, 6]
        elif mode == "error":
            response.pop("result")
            response["error"] = {"code": -32603, "message": "signature engine failed"}
        elif mode == "drift":
            (root / "source.pisig").write_text("external edit\n", encoding="utf-8")
        elif mode == "oversized":
            response["result"]["unknown"] = "x" * (2 * 1024 * 1024)
        elif mode == "unsolicited":
            pending = response
            send({"jsonrpc": "2.0", "id": "unexpected-edit", "method": "workspace/applyEdit", "params": {"edit": {
                "changes": {(root / "source.pisig").as_uri(): [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "unauthorized"
                }]}
            }}})
            continue
    send(response)
