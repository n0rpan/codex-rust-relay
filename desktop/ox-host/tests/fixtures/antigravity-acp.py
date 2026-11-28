#!/usr/bin/env python3
"""Offline ACP v1 peer for the Antigravity session host test."""
import json
import os
from pathlib import Path
import sys


SESSION = "fixture-antigravity-session"
PLAN = "Review the workspace before editing"
FIRST_REPLY = "The plan is ready for review."
REOPEN_REPLY = "Continuing the same Antigravity session."
mode = "default"
model = "fixture-gemini-a"
prompt_id = None
permission_id = None
cancelled = False


def send(**frame):
    print(json.dumps(dict(jsonrpc="2.0", **frame)), flush=True)


def reply(key, **result):
    send(id=key, result=result)


def update(update_type, **fields):
    send(method="session/update", params={"sessionId": SESSION,
         "update": dict(sessionUpdate=update_type, **fields)})


def chunk(role, text, key):
    update(f"{role}_message_chunk", messageId=key, content={"type": "text", "text": text})


def config_options():
    return [
        {"type": "select", "id": "mode", "name": "Mode", "category": "mode", "currentValue": mode,
         "options": [{"value": value, "name": name} for value, name in
                     (("default", "Default"), ("plan", "Plan"), ("accept-edits", "Accept edits"))]},
        {"type": "select", "id": "model", "name": "Model", "category": "model", "currentValue": model,
         "options": [{"value": "fixture-gemini-a", "name": "Fixture Gemini A"},
                     {"value": "fixture-gemini-b", "name": "Fixture Gemini B"}]},
    ]


def ask(key):
    tool = {"toolCallId": key, "title": "Review fixture command", "kind": "execute",
            "status": "pending"}
    update("tool_call", **tool)
    options = [{"optionId": "allow-once", "kind": "allow_once", "name": "Allow once"},
               {"optionId": "reject-once", "kind": "reject_once", "name": "Reject once"}]
    send(id=key, method="session/request_permission",
         params={"sessionId": SESSION, "toolCall": tool, "options": options})


expected_args = ["acp"]
if os.environ.get("OX_TEST_AGY_YOLO"):
    expected_args.append("--dangerously-skip-permissions")
assert sys.argv[1:] == expected_args, sys.argv
log_path = Path(os.environ["OX_TEST_AGY_LOG"])
saved_prompt = Path(os.environ["OX_TEST_AGY_PROMPT"])
for line in sys.stdin:
    frame = json.loads(line)
    with log_path.open("a") as log:
        log.write(json.dumps(frame) + "\n")
    key = frame.get("id")
    method = frame.get("method")
    params = frame.get("params", {})
    if method == "initialize":
        assert params["protocolVersion"] == 1, params
        assert params["clientInfo"]["name"] == "ox", params
        reply(key, protocolVersion=1, agentInfo={"name": "Antigravity ACP Server", "version": "1.5.19"},
              agentCapabilities={"loadSession": True, "sessionCapabilities": {"resume": {}, "load": {}},
                                 "promptCapabilities": {"embeddedContext": True}},
              authMethods=[{"id": "oauth-personal", "name": "Log in with Google"}])
    elif method == "session/new":
        assert params == {"cwd": os.getcwd(), "mcpServers": []}, params
        if os.environ.get("OX_TEST_AGY_SIGNED_OUT"):
            send(id=key, error={"code": -32000, "message": "Authentication required"})
        else:
            reply(key, sessionId=SESSION, configOptions=config_options())
    elif method == "session/load":
        assert params == {"cwd": os.getcwd(), "mcpServers": [], "sessionId": SESSION}, params
        mode = "plan"
        model = "fixture-gemini-b"
        chunk("user", saved_prompt.read_text(), "user-original")
        chunk("agent", FIRST_REPLY, "agent-original")
        reply(key, configOptions=config_options())
    elif method == "session/set_config_option":
        assert params["sessionId"] == SESSION, params
        if params["configId"] == "mode":
            assert params["value"] == "plan", params
            mode = "plan"
        else:
            assert params["configId"] == "model" and params["value"] == "fixture-gemini-b", params
            model = "fixture-gemini-b"
        reply(key, configOptions=config_options())
    elif method == "session/prompt":
        assert params["sessionId"] == SESSION, params
        prompt = params["prompt"]
        assert prompt[-1]["type"] == "text" and all(p["type"] == "resource" for p in prompt[:-1]), params
        text = prompt[-1]["text"]
        if text.startswith("<ox-handoff>"):
            assert mode == "plan", "the plan must be applied before the first prompt"
            saved_prompt.write_text(text)
            update("plan", entries=[{"content": PLAN, "priority": "high", "status": "in_progress"}])
            chunk("agent", "The plan is ", "agent-original")
            chunk("agent", "ready for review.", "agent-original")
            prompt_id, permission_id = key, "approve-first"
            ask(permission_id)
        elif text == "Cancel this turn":
            assert model == "fixture-gemini-b", model
            prompt_id, permission_id = key, "approve-second"
            ask(permission_id)
        elif text == "Continue after reopening":
            chunk("agent", REOPEN_REPLY, "agent-continuation")
            reply(key, stopReason="end_turn")
        elif text in ("Start in explicit YOLO", "Follow up in explicit YOLO"):
            assert os.environ.get("OX_TEST_AGY_YOLO"), expected_args
            chunk("agent", "YOLO fixture response", str(key))
            reply(key, stopReason="end_turn")
        else:
            raise AssertionError(f"Unexpected prompt {text!r}")
    elif method == "session/cancel":
        assert "id" not in frame and params == {"sessionId": SESSION}, frame
        assert permission_id == "approve-second", permission_id
        cancelled = True
    elif method is None and key == permission_id and permission_id is not None:
        if permission_id == "approve-first":
            assert frame["result"] == {"outcome": {"outcome": "selected", "optionId": "reject-once"}}, frame
            reply(prompt_id, stopReason="end_turn")
        else:
            assert cancelled and frame["result"] == {"outcome": {"outcome": "cancelled"}}, frame
            reply(prompt_id, stopReason="cancelled")
        prompt_id = permission_id = None
    else:
        raise AssertionError(f"Unexpected ACP frame {frame!r}")
