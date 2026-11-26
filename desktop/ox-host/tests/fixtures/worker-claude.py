#!/usr/bin/env python3
"""A stock stdio peer for the host worker launch and relaunch tests."""
import json
from pathlib import Path
import re
import sys


args = sys.argv[1:]
mode = args[args.index("--permission-mode") + 1] if "--permission-mode" in args else "default"
session = next(args[i + 1] for i, arg in enumerate(args) if arg in ("--session-id", "--resume"))

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

for line in sys.stdin:
    frame = json.loads(line)
    if frame["type"] == "control_request":
        request = frame["request"]
        if request["subtype"] == "set_permission_mode":
            mode = request["mode"]
            data = {"mode": mode}
        else:
            data = {"claude_code_version": "2.2.0", "current_permission_mode": mode,
                    "commands": [], "models": [], "account": {"apiProvider": "firstParty", "subscriptionType": "pro"}}
        reply = {"type": "control_response", "response": {
            "request_id": frame["request_id"], "subtype": "success", "response": data}}
        print(json.dumps(reply), flush=True)
    elif frame["type"] == "user":
        echo = {"type": "user", "message": frame["message"], "uuid": frame.get("uuid"),
                "session_id": session, "isReplay": True}
        print(json.dumps(echo), flush=True)
        result = {"type": "result", "subtype": "success", "is_error": False, "session_id": session,
                  "stop_reason": "end_turn", "result": "ok"}
        print(json.dumps(result), flush=True)
