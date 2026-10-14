#!/usr/bin/env python3
"""A fake ACP agent for tests (no network, no model).

Each prompt: say something, read hello.txt through the client, propose an upper-cased version
(tool_call with a diff), ask permission, and on approval write it through the client. A prompt
containing "run" asks to execute a command instead; one containing "chat" only answers.
Never prints anything but JSON-RPC on stdout.

Options: `--load` advertises `loadSession` (session/load replays one message, then answers);
`--log PATH` appends every handshake call and prompt to PATH as JSON lines.
"""
import json
import os
import sys

next_id = 1000
cwd = os.getcwd()
session = "fake-session"
can_load = "--load" in sys.argv[1:]
log_path = None
if "--log" in sys.argv[1:]:
    i = sys.argv.index("--log")
    if i + 1 < len(sys.argv):
        log_path = sys.argv[i + 1]


def log(entry):
    if log_path:
        with open(log_path, "a") as f:
            f.write(json.dumps(entry) + "\n")


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line)


def call(method, params):
    """Send a request to the client and wait for its response (queueing other messages)."""
    global next_id
    next_id += 1
    rid = next_id
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    while True:
        msg = read()
        if msg.get("id") == rid and "method" not in msg:
            return msg
        if msg.get("method") == "session/cancel":
            continue
        backlog.append(msg)


def update(u):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": u}})


backlog = []


def handle(msg):
    method = msg.get("method")
    mid = msg.get("id")
    params = msg.get("params") or {}
    global cwd, session
    if method in ("initialize", "session/new", "session/load"):
        log({"method": method, "params": params})
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1,
              "agentCapabilities": {"loadSession": can_load}, "authMethods": []}})
    elif method == "session/new":
        cwd = params.get("cwd", cwd)
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/load" and can_load:
        cwd = params.get("cwd", cwd)
        session = params.get("sessionId", session)
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Replayed history."}})
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "session/prompt":
        text = "".join(b.get("text", "") for b in params.get("prompt", []))
        log({"method": method, "sessionId": params.get("sessionId"), "text": text})
        if "chat" in text:
            update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Chatting."}})
            send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
            return
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Working on it."}})
        update({"sessionUpdate": "plan", "entries": [{"content": "Shout", "priority": "high", "status": "in_progress"}]})
        if "run" in text:
            update({"sessionUpdate": "tool_call", "toolCallId": "x1", "title": "Run tests", "kind": "execute",
                    "status": "pending", "rawInput": {"command": "cargo test"}})
            r = call("session/request_permission", {"sessionId": session, "toolCall": {"toolCallId": "x1"},
                     "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                                 {"optionId": "no", "name": "Reject", "kind": "reject_once"}]})
            ok = r.get("result", {}).get("outcome", {}).get("optionId") == "yes"
            update({"sessionUpdate": "tool_call_update", "toolCallId": "x1", "status": "completed" if ok else "failed"})
        else:
            path = os.path.join(cwd, "hello.txt")
            r = call("fs/read_text_file", {"sessionId": session, "path": path})
            old = r.get("result", {}).get("content", "")
            new = old.upper()
            update({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Edit hello.txt", "kind": "edit",
                    "status": "pending", "locations": [{"path": path}],
                    "content": [{"type": "diff", "path": path, "oldText": old, "newText": new}]})
            r = call("session/request_permission", {"sessionId": session, "toolCall": {"toolCallId": "c1"},
                     "options": [{"optionId": "once", "name": "Allow", "kind": "allow_once"},
                                 {"optionId": "always", "name": "Always", "kind": "allow_always"},
                                 {"optionId": "no", "name": "Reject", "kind": "reject_once"}]})
            choice = r.get("result", {}).get("outcome", {}).get("optionId")
            if choice in ("once", "always"):
                w = call("fs/write_text_file", {"sessionId": session, "path": path, "content": new})
                status = "failed" if "error" in w else "completed"
            else:
                status = "failed"
            update({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": status,
                    "content": [{"type": "content", "content": {"type": "text", "text": status}}]})
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown method"}})


while True:
    msg = backlog.pop(0) if backlog else read()
    handle(msg)
