"""Framed annotation peer. Models protocol messages, not language semantics."""
import json
import pathlib
import sys

ROOT = pathlib.Path.cwd()
MODE = sys.argv[1]
SOURCE = (ROOT / "source.anno").as_uri()
OTHER = (ROOT / "other.anno").as_uri()


def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise EOFError
        if line == b"\r\n":
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()
    body = sys.stdin.buffer.read(int(headers["content-length"]))
    message = json.loads(body)
    with (ROOT / "requests.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps(message) + "\n")
    return message


def send(message):
    body = json.dumps(message).encode("utf-8")
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(body)).encode("ascii") + body)
    sys.stdout.buffer.flush()


def workspace(uri=SOURCE, required=True, new_text="new"):
    annotation = {"label": "Change public API", "needsConfirmation": required,
                  "description": "Update callers of the exported name."}
    edit = {"range": {"start": {"line": 0, "character": 0},
                      "end": {"line": 0, "character": 3}},
            "newText": new_text, "annotationId": "public-api"}
    return {"changeAnnotations": {"public-api": annotation},
            "documentChanges": [{"textDocument": {"uri": uri, "version": None}, "edits": [edit]}]}


def result_edit(uri=SOURCE):
    result = workspace(uri, required=MODE != "info")
    if MODE == "dangling":
        result["changeAnnotations"] = {}
    elif MODE == "malformed":
        result["changeAnnotations"]["public-api"]["needsConfirmation"] = "true"
    elif MODE == "large":
        result["documentChanges"][0]["edits"][0]["newText"] = "x" * 220000
    return result


def callback(number, edit):
    request_id = "callback-%d" % number
    send({"jsonrpc": "2.0", "id": request_id, "method": "workspace/applyEdit",
          "params": {"label": "Annotated command", "edit": edit}})
    while True:
        reply = read()
        if reply.get("id") == request_id and "method" not in reply:
            with (ROOT / "callbacks.jsonl").open("a", encoding="utf-8") as log:
                log.write(json.dumps(reply) + "\n")
            return


def main():
    while True:
        message = read()
        method = message.get("method")
        params = message.get("params") or {}
        if method == "exit":
            return
        if method == "initialized":
            send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                  "params": {"quiescent": True}})
        if method == "workspace/didRenameFiles":
            (ROOT / "notified.json").write_text(json.dumps({
                "afterMove": not (ROOT / "source.anno").exists() and (ROOT / "moved.anno").exists(),
                "other": (ROOT / "other.anno").read_text(encoding="utf-8")}), encoding="utf-8")
        if "id" not in message:
            continue
        result = None
        if method == "initialize":
            result = {"capabilities": {"textDocumentSync": 1, "renameProvider": True,
                "codeActionProvider": {"resolveProvider": True},
                "executeCommandProvider": {"commands": ["test.annotated"]},
                "workspace": {"fileOperations": {
                    name: {"filters": [{"scheme": "file", "pattern": {"glob": "**/*.anno", "matches": "file"}}]}
                    for name in ("willRename", "didRename")}}}}
        elif method == "textDocument/rename":
            result = result_edit(params["textDocument"]["uri"])
        elif method == "workspace/willRenameFiles":
            result = result_edit(OTHER)
        elif method == "textDocument/codeAction":
            action = {"title": "Annotated refactor", "kind": "refactor.rewrite"}
            if MODE in ("command", "partial"):
                action["command"] = {"title": "Run", "command": "test.annotated", "arguments": []}
            elif MODE == "lazy":
                action["data"] = {"token": "exact-original"}
            else:
                action["edit"] = result_edit()
            result = [action]
        elif method == "codeAction/resolve":
            result = dict(params)
            if MODE == "lazy":
                assert params["data"] == {"token": "exact-original"}
                result["edit"] = result_edit()
                result["edit"]["documentChanges"].extend(workspace(OTHER)["documentChanges"])
        elif method == "workspace/executeCommand":
            if MODE == "partial":
                callback(1, workspace(required=False))
                callback(2, workspace(new_text="bad"))
                callback(3, workspace(required=False, new_text="bad"))
            else:
                callback(1, workspace())
            result = {"done": True}
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})


if __name__ == "__main__":
    try:
        main()
    except EOFError:
        pass
