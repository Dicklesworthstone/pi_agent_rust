"""Framed LSP peer for code-action integration tests; not a language server."""
import json
from pathlib import Path
import sys
from urllib.parse import unquote, urlparse

MODE = sys.argv[1]
SOURCE = None
VERSIONS = []
SEQUENCE = 0


def read():
    length = None
    total = 0
    while True:
        line = sys.stdin.buffer.readline(4097)
        if not line:
            return None
        total += len(line)
        if total > 16384 or len(line) > 4096:
            raise ValueError("oversized test frame headers")
        if line == b"\r\n":
            break
        key, value = line.split(b":", 1)
        if key.lower() == b"content-length":
            length = int(value)
    if length is None or not 0 < length <= 4 * 1024 * 1024:
        raise ValueError("invalid test frame size")
    body = sys.stdin.buffer.read(length)
    if len(body) != length:
        raise ValueError("truncated test frame")
    return json.loads(body)


def send(message):
    payload = json.dumps(dict(jsonrpc="2.0", **message)).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(payload) + payload)
    sys.stdout.buffer.flush()


def reply(request, result):
    send({"id": request["id"], "result": result})


def edit(uri, text="fixed", end=3):
    return {"changes": {uri: [{"range": {"start": {"line": 0, "character": 0},
                                         "end": {"line": 0, "character": end}},
                              "newText": text}]}}


def server_edit(uri):
    global SEQUENCE
    SEQUENCE += 1
    request_id = "apply-%d" % SEQUENCE
    send({"id": request_id, "method": "workspace/applyEdit", "params": {"edit": edit(uri)}})
    while True:
        response = read()
        if response is None:
            raise EOFError("client left during server edit")
        if response.get("id") == request_id:
            return response["result"]
        if "method" in response and "id" not in response:
            continue
        raise ValueError("unexpected request while awaiting server edit acknowledgement")


def main():
    global SOURCE
    while True:
        request = read()
        if request is None:
            return
        method = request.get("method")
        params = request.get("params", {})
        if method:
            with Path("requests.jsonl").open("a", encoding="utf-8") as log:
                log.write(json.dumps({"method": method, "params": params}) + "\n")
        if method == "initialize":
            assert params["capabilities"]["textDocument"]["codeAction"]["dataSupport"]
            reply(request, {"capabilities": {"textDocumentSync": 1,
                  "codeActionProvider": {"resolveProvider": True},
                  "executeCommandProvider": {"commands": ["test.finish"]}}})
        elif method in ("textDocument/didOpen", "textDocument/didChange"):
            VERSIONS.append(params["textDocument"]["version"])
        elif method == "textDocument/codeAction":
            SOURCE = params["textDocument"]["uri"]
            action = {"title": "Finish refactoring", "kind": "refactor", "data": {"uri": SOURCE}}
            if MODE == "disabled":
                action["disabled"] = {"reason": "not applicable"}
            reply(request, [action])
        elif method == "codeAction/resolve":
            resolved = dict(params, edit=edit(params["data"]["uri"]),
                            command={"title": "Finish", "command": "test.finish", "arguments": ["literal"]})
            if MODE == "changed":
                resolved["title"] = "Another action"
            if MODE == "malformed":
                resolved["command"]["arguments"] = {"not": "an array"}
            reply(request, resolved)
        elif method == "workspace/executeCommand":
            assert Path(unquote(urlparse(SOURCE).path)).read_text() == "fixed\n", "command preceded edit"
            Path("command-started").write_text("started", encoding="ascii")
            if MODE == "stall":
                continue
            if MODE == "error":
                send({"id": request["id"], "error": {"code": -32801, "message": "command failed after the edit"}})
                continue
            result = None
            if MODE == "callback":
                result = server_edit(Path("sibling.lspfixture").resolve().as_uri())
            elif MODE == "outside":
                result = server_edit((Path.cwd().parent / "outside.lspfixture").as_uri())
            reply(request, result)
        elif method in ("test/unsolicited", "test/lateEdit"):
            result = server_edit(Path("sibling.lspfixture").resolve().as_uri())
            reply(request, result)
        elif method == "test/versions":
            reply(request, VERSIONS)
        elif method == "shutdown":
            reply(request, None)
        elif method == "exit":
            return
        elif "id" in request:
            send({"id": request["id"], "error": {"code": -32601, "message": "unknown fixture method"}})


if __name__ == "__main__":
    main()
