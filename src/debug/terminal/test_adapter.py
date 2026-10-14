"""Bounded DAP protocol peer for terminal-host tests, not a live debugger."""
import json
from pathlib import Path
import sys

MODE = sys.argv[1] if len(sys.argv) > 1 else "normal"
SEQ = 0
LAUNCH = None
REQUESTS = []


def read():
    total = 0
    length = None
    while True:
        line = sys.stdin.buffer.readline(4097)
        if not line:
            return None
        total += len(line)
        if total > 4096 or len(line) > 4096:
            raise ValueError("oversized header")
        if line == b"\r\n":
            break
        key, value = line.split(b":", 1)
        if key.lower() == b"content-length":
            if length is not None:
                raise ValueError("duplicate content length")
            length = int(value)
    if length is None or not 0 < length <= 2 * 1024 * 1024:
        raise ValueError("invalid body length")
    data = sys.stdin.buffer.read(length)
    if len(data) != length:
        raise ValueError("truncated body")
    return json.loads(data)


def send(message):
    global SEQ
    SEQ += 1
    message["seq"] = SEQ
    data = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    sys.stdout.buffer.flush()


def reply(request, body=None, error=None):
    send({"type": "response", "request_seq": request["seq"], "command": request["command"],
          "success": error is None, "body": body or {}, "message": error or ""})


def event(name, body=None):
    send({"type": "event", "event": name, "body": body or {}})


while True:
    message = read()
    if message is None:
        break
    if message["type"] == "response":
        assert message["command"] == "runInTerminal"
        if not message["success"]:
            reply(LAUNCH, error=message.get("message", "terminal refused"))
            continue
        Path("terminal-pid.txt").write_text(str(message["body"]["processId"]), encoding="ascii")
        if MODE == "reject_after_start":
            reply(LAUNCH, error="deliberate launch rejection after terminal start")
            continue
        if MODE != "stall":
            event("initialized")
        continue
    command = message["command"]
    arguments = message.get("arguments", {})
    REQUESTS.append({"command": command, "arguments": arguments})
    if command == "initialize":
        assert arguments["supportsStartDebuggingRequest"] is False
        assert arguments.get("supportsArgsCanBeInterpretedByShell", False) is False
        reply(message, {"supportsConfigurationDoneRequest": True, "supportTerminateDebuggee": True})
    elif command == "launch":
        LAUNCH = message
        if MODE == "no_reverse":
            event("initialized")
            continue
        assert arguments["console"] == "integratedTerminal"
        terminal = {"kind": "integrated", "title": "test terminal", "cwd": arguments["cwd"],
                    "args": [sys.executable, "-I", "-u", arguments["program"], *arguments.get("args", [])],
                    "env": {"PI_TERMINAL_TEST_MARKER": "literal-$value;", "CARGO_TARGET_DIR": None}}
        if MODE == "shell_request":
            terminal["argsCanBeInterpretedByShell"] = True
        send({"type": "request", "command": "runInTerminal", "arguments": terminal})
    elif command == "setBreakpoints":
        reply(message, {"breakpoints": [dict(item, verified=True) for item in arguments["breakpoints"]]})
    elif command == "configurationDone":
        reply(message)
        event("stopped", {"threadId": 7, "reason": "entry"})
        reply(LAUNCH)
    elif command == "capture":
        reply(message, {"requests": REQUESTS})
    elif command == "disconnect":
        reply(message)
        event("terminated")
        break
    else:
        reply(message, error="unsupported test command: " + command)
