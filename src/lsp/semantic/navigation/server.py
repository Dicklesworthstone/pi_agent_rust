"""Framed navigation peer; supplies protocol cases, not language analysis."""
import json
import pathlib
import sys

ROOT = pathlib.Path.cwd()
MODE = sys.argv[1]
PENDING = None


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
    return json.loads(sys.stdin.buffer.read(int(headers["content-length"])))


def send(message):
    body = json.dumps(message, ensure_ascii=False).encode("utf-8")
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(body)).encode("ascii") + body)
    sys.stdout.buffer.flush()


def span(start, end):
    return {"start": {"line": 0, "character": start}, "end": {"line": 0, "character": end}}


def reply(message):
    method = message["method"]
    target = (ROOT / "not-created.navfixture").as_uri()
    plain = {"uri": target, "range": span(3, 8)}
    link = {"targetUri": target, "targetRange": span(0, 20),
            "targetSelectionRange": span(3, 8), "originSelectionRange": span(2, 7)}
    result = [plain, plain] if method == "textDocument/references" else [link]
    if MODE == "plain":
        result = [plain] if method == "textDocument/references" else plain
    if method == "textDocument/hover":
        result = {"contents": {"kind": "markdown", "value": "**alpha**"}, "range": span(2, 7)}
        if MODE == "marked":
            result["contents"] = [{"language": "rust", "value": "fn alpha()"}, "docs"]
        elif MODE == "bad-hover":
            result["contents"] = ["valid", False]
    if MODE == "null":
        result = None
    elif MODE == "empty":
        result = {"contents": []} if method == "textDocument/hover" else []
    elif MODE == "bad-tail":
        result = [link, {"targetUri": target, "targetRange": span(0, 20)}]
    elif MODE == "bad-selection":
        link["targetSelectionRange"] = span(19, 21)
    elif MODE == "many":
        result = [plain] * 4097
    elif MODE == "output-limit":
        result = [{"uri": "generated:" + "x" * 8000, "range": span(0, 1)}] * 40
    elif MODE == "drift":
        (ROOT / "source.navfixture").write_text("external source\n", encoding="utf-8")
    if MODE == "error":
        send({"jsonrpc": "2.0", "id": message["id"],
              "error": {"code": -32603, "message": "navigation provider failed"}})
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})


while True:
    message = read()
    if message is None:
        break
    with (ROOT / "requests.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps(message) + "\n")
    method = message.get("method")
    if method == "exit":
        break
    if method == "test/release" and PENDING is not None:
        reply(PENDING)
        PENDING = None
    if "id" not in message:
        continue
    if method == "initialize":
        if MODE == "hang-init":
            continue
        caps = {"textDocumentSync": 1}
        for key in ("definitionProvider", "referencesProvider", "typeDefinitionProvider", "implementationProvider", "hoverProvider"):
            caps[key] = MODE != "disabled"
        if MODE == "encoding":
            caps["positionEncoding"] = "utf-8"
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": caps}})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"quiescent": True}})
    elif method in ("textDocument/definition", "textDocument/references", "textDocument/typeDefinition", "textDocument/implementation", "textDocument/hover"):
        if MODE in ("hold", "hang"):
            PENDING = message
            continue
        if MODE == "probe":
            uri = (ROOT / "other.navfixture").as_uri()
            send({"jsonrpc": "2.0", "id": "edit-probe", "method": "workspace/applyEdit",
                  "params": {"edit": {"changes": {uri: [{"range": span(0, 4), "newText": "wrong"}]}}}})
            (ROOT / "probe.json").write_text(json.dumps(read()), encoding="utf-8")
        reply(message)
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"barrier": True}})
