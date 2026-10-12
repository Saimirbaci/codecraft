#!/usr/bin/env python3
"""A fake ACP agent for tests (no network, no model).

Each prompt: say something, read hello.txt through the client, propose an upper-cased version
(tool_call with a diff), ask permission, and on approval write it through the client. A prompt
containing "run" asks to execute a command instead. Never prints anything but JSON-RPC on stdout.
"""
import json
import os
import sys

next_id = 1000
cwd = os.getcwd()
session = "fake-session"


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
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}})
    elif method == "session/new":
        global cwd
        cwd = params.get("cwd", cwd)
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
    elif method == "session/prompt":
        text = "".join(b.get("text", "") for b in params.get("prompt", []))
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
