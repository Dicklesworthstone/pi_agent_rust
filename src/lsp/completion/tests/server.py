"""A framed completion peer; no production Rust behavior is simulated here."""
import json
import pathlib
import sys

root = pathlib.Path.cwd()
mode = sys.argv[1]
source_path = root / "source.picomp"
opaque = {"ticket": [1, {"nested": "opaque"}]}
pending = None


def send(value):
    body = json.dumps(value, ensure_ascii=False).encode("utf-8")
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(body)).encode("ascii") + body)
    sys.stdout.buffer.flush()


def log(value):
    with (root / "completion-requests.jsonl").open("a", encoding="utf-8") as output:
        output.write(json.dumps(value) + "\n")


def position(character):
    return {"line": 1, "character": character}


def span(start, end):
    return {"start": position(start), "end": position(end)}


def result():
    if mode == "empty":
        return None
    if mode == "malformed":
        return {"items": [], "isIncomplete": "false"}
    primary = {"range": span(12, 14), "newText": "Type"}
    item = {"label": "Type", "textEdit": primary, "data": opaque}
    if mode == "plain":
        item = {"label": "Type", "insertText": "Type"}
    elif mode == "replace":
        item["textEdit"] = {"insert": span(12, 14), "replace": span(12, 16), "newText": "Type"}
    elif mode.startswith("snippet"):
        item["insertTextFormat"] = 2
        snippets = {
            "snippet": "Type(${1:argument})$0",
            "snippet_lazy": "Type(${1:argument}, $1)$0",
            "snippet_mirror": "Type(${1:argument}, $1)$0",
            "snippet_required": "Type($1, $2)$0",
            "snippet_choice": "Type(${1|red,green|})$0",
            "snippet_nested": "Type(${1:outer(${2:inner})})$0",
            "snippet_variable": "Type($CLIPBOARD)$0",
            "snippet_transform": "Type(${1/(.*)/$1/})$0",
            "snippet_malformed": "Type(${1:unfinished",
            "snippet_conflict": "Type(${1:one}, ${1:two})",
            "snippet_defaults": "Type(${1:default})$0",
        }
        item["textEdit"]["newText"] = snippets[mode]
        if mode == "snippet_defaults":
            return {"isIncomplete": False,
                    "itemDefaults": {"editRange": span(12,14), "insertTextFormat": 2, "data": opaque},
                    "items": [{"label": "Type", "textEditText": snippets[mode]}]}
        return [item]
    elif mode == "command":
        item["command"] = {"title": "Run", "command": "unsafe.run", "arguments": ["opaque"]}
    elif mode == "indent":
        item["insertTextMode"] = 2
    if mode in ("array", "plain", "replace", "snippet", "command", "indent"):
        return [item]
    return {"isIncomplete": True, "itemDefaults": {"editRange": span(12, 14), "insertTextFormat": 1, "data": opaque},
            "items": [{"label": "Type", "filterText": "Ty", "sortText": "02", "textEditText": "Type"},
                      {"label": "Other", "filterText": "Other", "sortText": "01"}]}


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
    log(message)
    method = message.get("method")
    if method == "exit":
        break
    if method is None and message.get("id") == "unsolicited-edit":
        assert message.get("result", {}).get("applied") is False, "unsolicited edit was authorized"
        send({"jsonrpc": "2.0", "id": pending[0], "result": pending[1]})
        pending = None
        continue
    if "id" not in message:
        continue
    response = {"jsonrpc": "2.0", "id": message["id"], "result": None}
    if method == "initialize":
        if mode == "hang_initialize":
            continue
        capabilities = message["params"]["capabilities"]["textDocument"]["completion"]
        assert capabilities["completionItem"]["snippetSupport"] is True
        assert capabilities["completionItem"]["insertReplaceSupport"] is True
        assert "additionalTextEdits" in capabilities["completionItem"]["resolveSupport"]["properties"]
        assert "editRange" in capabilities["completionList"]["itemDefaults"]
        server = {"textDocumentSync": 1}
        if mode == "encoding":
            server["positionEncoding"] = "utf-8"
        if mode != "unsupported":
            resolve = mode not in ("array", "plain", "replace", "command", "indent")
            if mode.startswith("snippet"):
                resolve = mode in ("snippet_lazy", "snippet_defaults")
            server["completionProvider"] = {"resolveProvider": resolve}
        response["result"] = {"capabilities": server}
    elif method == "textDocument/completion":
        assert message["params"]["position"] == position(14)
        assert message["params"]["context"] == {"triggerKind": 1}
        if mode == "hang":
            continue
        response["result"] = result()
        if mode == "drift_list":
            source_path.write_text("external\n", encoding="utf-8")
        if mode == "unsolicited":
            pending = (message["id"], response["result"])
            send({"jsonrpc": "2.0", "id": "unsolicited-edit", "method": "workspace/applyEdit",
                  "params": {"edit": {"changes": {source_path.as_uri(): [{"range": span(12,14), "newText": "BAD"}]}}}})
            continue
    elif method == "completionItem/resolve":
        item = message["params"]
        assert item["data"] == opaque, "completion data was not preserved"
        assert item["textEdit"]["range"] == span(12,14), "default edit range was not materialized"
        item["detail"] = "struct Type"
        item["documentation"] = {"kind": "markdown", "value": "A semantic type."}
        item["additionalTextEdits"] = [{"range": {"start": {"line": 0, "character": 0},
                                                    "end": {"line": 0, "character": 0}},
                                         "newText": "use example::Type;\n"}]
        if mode == "snippet_lazy":
            item["additionalTextEdits"][0]["newText"] += "// literal $0 ${1:keep}\n"
        if mode == "overlap":
            item["additionalTextEdits"][0]["range"] = span(12,14)
        elif mode == "mutate":
            item["textEdit"]["newText"] = "Substitute"
        elif mode == "drift_resolve":
            source_path.write_text("external\n", encoding="utf-8")
        response["result"] = item
    elif method == "workspace/executeCommand":
        raise AssertionError("completion must never execute commands")
    send(response)
