#!/usr/bin/env python3
"""A fake ACP agent for the run-configuration generator tests (no network, no model).

It answers each prompt with a JSON block of run configurations: the first detected one from the
prompt (kept), one it adds, and one with an unknown kind (dropped by CodeCraft). A prompt without
the generator's wording gets a reply without JSON. Never prints anything but JSON-RPC on stdout.
"""
import json
import re
import sys


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def say(session, text):
    send({"jsonrpc": "2.0", "method": "session/update",
          "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk",
                                                       "content": {"type": "text", "text": text}}}})


for line in sys.stdin:
    msg = json.loads(line)
    method, mid, params = msg.get("method"), msg.get("id"), msg.get("params") or {}
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "gen"}})
    elif method == "session/prompt":
        text = "".join(b.get("text", "") for b in params.get("prompt", []))
        if "Propose run configurations" not in text:
            say("gen", "I only generate run configurations.")
        else:
            m = re.search(r"```json\n(.*?)\n```", text, re.S)
            detected = json.loads(m.group(1)) if m else []
            reply = detected[:1] + [
                {"name": "Worker", "kind": "python", "command": "-m worker", "workingDir": "",
                 "env": {"QUEUE": "default"}, "reason": "the background worker"},
                {"name": "Nonsense", "kind": "cobol", "command": "run"},
            ]
            say("gen", "Looked at the project.\n```json\n")
            say("gen", json.dumps(reply, indent=1))
            say("gen", "\n```\n")
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown method"}})
