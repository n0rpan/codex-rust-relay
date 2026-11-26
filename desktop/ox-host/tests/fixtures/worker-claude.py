#!/usr/bin/env python3
"""A stock stdio peer for the host worker launch and relaunch tests."""
import json
from pathlib import Path
import re
import sys


args = sys.argv[1:]
mode = args[args.index("--permission-mode") + 1] if "--permission-mode" in args else "default"
session = next(args[i + 1] for i, arg in enumerate(args) if arg in ("--session-id", "--resume"))

pinned = args[args.index("--model") + 1] if "--model" in args else None
picker = [
    {"value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)"},
    {"value": "fable", "resolvedModel": "claude-fable-5-2", "displayName": "Fable 5.2"},
] if pinned else []

with Path(f"argv-{session}").open("a") as log:
    log.write(json.dumps(sys.argv) + "\n")

if "--session-id" in args:
    project = str(Path.cwd().resolve())
    directory = Path("native/projects") / re.sub("[^A-Za-z0-9]", "-", project)
    directory.mkdir(parents=True, exist_ok=True)
    with (directory / f"{session}.jsonl").open("w") as journal:
        for kind, suffix, content in (("user", "01", "hi"), ("assistant", "02", [{"type": "text", "text": "ok"}])):
            uuid = "00000000-0000-4000-8000-0000000000" + suffix
            row = {"type": kind, "uuid": uuid, "cwd": project, "sessionId": session, "entrypoint": "sdk-cli",
                   "message": {"role": kind, "id": uuid, "content": content}}
            journal.write(json.dumps(row) + "\n")

def finish(text):
    answer = {"type": "assistant", "uuid": f"a-{session}", "session_id": session, "parent_tool_use_id": None,
              "message": {"id": f"m-{session}", "role": "assistant", "model": "claude-opus-5-5",
                          "content": [{"type": "text", "text": text}]}}
    print(json.dumps(answer), flush=True)
    result = {"type": "result", "subtype": "success", "is_error": False, "session_id": session,
              "stop_reason": "end_turn", "result": text}
    print(json.dumps(result), flush=True)


for line in sys.stdin:
    frame = json.loads(line)
    if frame["type"] == "control_response":
        # Plan mode's exit request was answered: log the answer and report.
        response = frame["response"]
        with Path(f"plan-exit-{session}").open("w") as log:
            log.write(json.dumps(response))
        finish("Findings: the plan is the report.")
    elif frame["type"] == "control_request":
        request = frame["request"]
        if request["subtype"] == "set_permission_mode":
            mode = request["mode"]
            data = {"mode": mode}
        else:
            data = {"claude_code_version": "2.2.0", "current_permission_mode": mode,
                    "commands": [], "models": picker, "account": {"apiProvider": "firstParty", "subscriptionType": "pro"}}
        reply = {"type": "control_response", "response": {
            "request_id": frame["request_id"], "subtype": "success", "response": data}}
        print(json.dumps(reply), flush=True)
    elif frame["type"] == "user":
        echo = {"type": "user", "message": frame["message"], "uuid": frame.get("uuid"),
                "session_id": session, "isReplay": True}
        print(json.dumps(echo), flush=True)
        text = json.dumps(frame["message"])
        if mode == "plan" and "PLAN-EXIT" in text:
            # Like stock Plan mode, finish the plan by asking to leave Plan mode.
            ask = {"type": "control_request", "request_id": "plan-exit", "request": {
                "subtype": "can_use_tool", "tool_name": "ExitPlanMode", "tool_use_id": "tool-plan",
                "input": {"plan": "1. Review the diff."}}}
            print(json.dumps(ask), flush=True)
            continue
        if pinned:
            model = next((row["resolvedModel"] for row in picker if pinned in (row["value"], row["resolvedModel"])), pinned)
            answer = {"type": "assistant", "uuid": f"a-{session}", "session_id": session, "parent_tool_use_id": None,
                      "message": {"id": f"m-{session}", "role": "assistant", "model": model,
                                  "content": [{"type": "text", "text": "reviewed"}]}}
            print(json.dumps(answer), flush=True)
        result = {"type": "result", "subtype": "success", "is_error": False, "session_id": session,
                  "stop_reason": "end_turn", "result": "reviewed" if pinned else "ok"}
        print(json.dumps(result), flush=True)
