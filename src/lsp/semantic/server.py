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


def hints(selection):
    result = [
        {"position": {"line": 1, "character": 5}, "kind": 2,
         "label": [{"value": "value:"}, {"value": " Text", "command": {
             "title": "never execute", "command": "untrusted.command", "arguments": ["not-for-output"]}}],
         "paddingRight": True, "data": {"token": ["opaque", 7]},
         "textEdits": [{"range": {"start": {"line": 1, "character": 0},
                                    "end": {"line": 1, "character": 0}}, "newText": "never insert"}]},
        {"position": {"line": 1, "character": 16}, "kind": 1, "label": ": bool", "data": {"token": ["type", 8]}},
        {"position": {"line": 1, "character": 16}, "label": "inferred", "data": {"token": ["same-position", 9]}}
    ]
    if mode in ("hints-many", "hints-resolve-output"):
        result = [{"position": {"line": 1, "character": 16}, "label": "hint %d" % index,
                   "data": {"index": index}} for index in range(160 if mode == "hints-many" else 8)]
    bounds = [(selection[key]["line"], selection[key]["character"]) for key in ("start", "end")]
    return [item for item in result if bounds[0] <= (item["position"]["line"], item["position"]["character"]) <= bounds[1]]


def request_edit(response):
    global pending
    pending = response
    send({"jsonrpc": "2.0", "id": "unexpected-edit", "method": "workspace/applyEdit", "params": {"edit": {
        "changes": {(root / "source.pisig").as_uri(): [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "unauthorized"
        }]}
    }}})


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
        caps = {"textDocumentSync": 1, "signatureHelpProvider": {}, "inlayHintProvider": {"resolveProvider": True}}
        if mode == "unsupported":
            caps.pop("signatureHelpProvider")
        if mode == "encoding":
            caps["positionEncoding"] = "utf-8"
        if mode == "hints-unsupported":
            caps.pop("inlayHintProvider")
        elif mode == "hints-no-resolve":
            caps["inlayHintProvider"] = {}
        elif mode == "hints-boolean":
            caps["inlayHintProvider"] = True
        response["result"] = {"capabilities": caps}
    elif method == "textDocument/inlayHint":
        if mode == "hints-hang":
            continue
        response["result"] = hints(message["params"]["range"])
        if mode == "hints-null":
            response["result"] = None
        elif mode == "hints-empty":
            response["result"] = []
        elif mode == "hints-malformed":
            response["result"][-1]["label"] = ""
        elif mode == "hints-outside":
            response["result"][0]["position"] = {"line": 0, "character": 0}
        elif mode == "hints-drift":
            (root / "source.pisig").write_text("external hint edit\n", encoding="utf-8")
        elif mode == "hints-oversized":
            response["result"][0]["data"] = "x" * (2 * 1024 * 1024)
        elif mode == "hints-error":
            response.pop("result")
            response["error"] = {"code": -32603, "message": "hint engine failed"}
        elif mode == "hints-unsolicited":
            request_edit(response)
            continue
    elif method == "inlayHint/resolve":
        if mode == "hints-resolve-hang":
            continue
        result = message["params"]
        result["tooltip"] = {"kind": "markdown", "value": "resolved detail"}
        if isinstance(result["label"], list):
            result["label"][0]["tooltip"] = "resolved part"
            result["label"][0]["location"] = {"uri": "https://invalid.example/type", "range": {
                "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}}}
        if mode == "hints-resolve-change":
            result["position"]["character"] += 1
        elif mode == "hints-resolve-data":
            result["data"] = {"token": "different identity"}
        elif mode == "hints-resolve-command":
            result["label"][1]["command"]["command"] = "new.command"
        elif mode == "hints-resolve-drift":
            (root / "source.pisig").write_text("external hint edit\n", encoding="utf-8")
        elif mode == "hints-resolve-oversized":
            result["tooltip"] = "x" * (64 * 1024)
        elif mode == "hints-resolve-output":
            result["tooltip"] = "x" * (50 * 1024)
        response["result"] = result
        if mode == "hints-resolve-error":
            response.pop("result")
            response["error"] = {"code": -32603, "message": "hint resolution failed"}
        elif mode == "hints-resolve-unsolicited":
            request_edit(response)
            continue
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
            request_edit(response)
            continue
    send(response)
