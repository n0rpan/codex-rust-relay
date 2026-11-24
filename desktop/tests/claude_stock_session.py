"""Stock Claude Code stream-json sessions against disposable loopback endpoints."""

import argparse
import json
import os
import queue
import signal
import ssl
import subprocess
import tempfile
import threading
import time
import uuid
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_CONNECT(self):
        if self.path != "api.anthropic.com:443" or not hasattr(
            self.server, "native_tls"
        ):
            self.send_error(403)
            return
        self.send_response(200, "Connection established")
        self.end_headers()
        self.wfile.flush()
        try:
            connection = self.server.native_tls.wrap_socket(self.connection, server_side=True)
        except (ConnectionResetError, ssl.SSLEOFError):
            # Stock may abandon a speculative connection on session shutdown.
            self.close_connection = True
            return
        with connection:
            self.connection = self.request = connection
            self.setup()
            self.handle_one_request()
        self.close_connection = True

    def do_POST(self):
        size = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(size))
        if self.path.split("?")[0] == "/v1/messages/count_tokens":
            data = b'{"input_tokens":8}'
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        if self.path.split("?")[0] != "/v1/messages":
            self.send_error(404)
            return
        supplied = [
            self.headers[name]
            for name in ("x-api-key", "Authorization")
            if self.headers.get(name)
        ]
        if (
            not supplied
            or any(value != self.server.expected_auth for value in supplied)
            or (not hasattr(self.server, "native_tls") and body["model"] != "demo")
        ):
            self.send_error(403)
            self.server.errors.append("Wrong credential or model")
            return
        self.server.requests.append(body)
        history = body["messages"]
        user = next(
            (m["content"] for m in reversed(history) if m["role"] == "user"), []
        )
        prompt = (
            next(
                (
                    block["text"]
                    for block in reversed(user)
                    if block.get("type") == "text"
                ),
                "",
            )
            if isinstance(user, list)
            else user
        )
        output = [{"type": "text", "text": "Fixture completed."}]
        if "Name a conversation using its first user message" in str(body.get("system")):
            assert not body.get("tools"), "Claude's title request included tools"
            title = "Sapling Schedule" if "spring planting" in prompt else "Orchard Planning"
            output = [{"type": "text", "text": title}]
        elif getattr(self.server, "worker_case", False) and prompt.startswith("HOST-WORKER "):
            output = [{"type":"tool_use","id":"toolu_" + uuid.uuid4().hex[:16],
                       "name":"mcp__ox_workers__start_ox_worker",
                       "input":{"runtime":"claude","mode":"session","model":"fixture/demo",
                                "task":"Work on the host fixture", "workspace_write":prompt.endswith("write")}}]
        elif "LIVE-SPLIT" in prompt and hasattr(self.server, "release_stream"):
            output = [{"type":"text","text":"Copperstream"}]
        elif "TERMINAL-REVIEW" in prompt and hasattr(self.server, "review_command"):
            output = [{"type":"tool_use","id":"toolu_"+uuid.uuid4().hex[:16],"name":"Bash",
                       "input":{"command":self.server.review_command,"description":"Verify local terminal handoff"}}]
        elif "APPROVAL" in prompt:
            output = [
                {
                    "type": "tool_use",
                    "id": "toolu_" + uuid.uuid4().hex[:16],
                    "name": "Bash",
                    "input": {
                        "command": "echo accepted > approved.txt",
                        "description": "Write a fixture marker",
                    },
                }
            ]
        elif "QUESTION-FORM" in prompt:
            output = [{"type": "tool_use", "id": "toolu_" + uuid.uuid4().hex[:16],
                       "name": "AskUserQuestion", "input": {"questions": [
                {"question": "Which fruit?", "header": "Fruit", "multiSelect": True,
                 "options": [{"label": "Apple", "description": "An apple"},
                             {"label": "Pear", "description": "A pear"}]}]}}]
        elif "SUBAGENT" in prompt:
            output = [
                {
                    "type": "tool_use",
                    "id": "toolu_" + uuid.uuid4().hex[:16],
                    "name": "Agent",
                    "input": {
                        "description": "Inspect fixture",
                        "prompt": "CHILD_RESPONSE",
                        "subagent_type": "general-purpose",
                    },
                }
            ]
        msg = {
            "id": "msg_" + uuid.uuid4().hex[:16],
            "model": body["model"],
            "role": "assistant",
            "type": "message",
            "content": output,
            "stop_reason": "tool_use"
            if output[0]["type"] == "tool_use"
            else "end_turn",
            "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 8},
        }
        if not body.get("stream"):
            payload = json.dumps(msg).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        events = [
            {
                "type": "message_start",
                "message": dict(msg, content=[], stop_reason=None),
            }
        ]
        for i, block in enumerate(output):
            if block["type"] == "tool_use":
                content = dict(block, input={})
                delta = {
                    "type": "input_json_delta",
                    "partial_json": json.dumps(block["input"]),
                }
            else:
                content = {"type": "text", "text": ""}
                delta = {"type": "text_delta", "text": block["text"]}
            events.extend(
                [
                    {
                        "type": "content_block_start",
                        "index": i,
                        "content_block": content,
                    },
                    {"type": "content_block_delta", "index": i, "delta": delta},
                    {"type": "content_block_stop", "index": i},
                ]
            )
        events.extend(
            [
                {
                    "type": "message_delta",
                    "delta": {"stop_reason": msg["stop_reason"], "stop_sequence": None},
                    "usage": {"output_tokens": 8},
                },
                {"type": "message_stop"},
            ]
        )
        split = output == [{"type":"text","text":"Copperstream"}] and hasattr(self.server, "release_stream")
        if split:
            events.insert(3, {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" Sunbeam"}})
        frames = [("event: " + e["type"] + "\ndata: " + json.dumps(e) + "\n\n").encode() for e in events]
        payload = b"".join(frames)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        if split:
            self.wfile.write(b"".join(frames[:3]))
            self.wfile.flush()
            if not self.server.release_stream.wait(40):
                return
            self.wfile.write(b"".join(frames[3:]))
        else:
            self.wfile.write(payload)


class Session:
    def __init__(self, ox, project, env, route, *args):
        self.process = subprocess.Popen(
            [
                ox,
                "claude",
                *route,
                "-p",
                "--verbose",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
                *args,
            ],
            cwd=project,
            env=env,
            text=True,
            start_new_session=True,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.inbox = queue.Queue()
        self.frames = []
        self.stderr = []

        def read():
            for line in self.process.stdout:
                try:
                    self.inbox.put(json.loads(line))
                except ValueError:
                    self.inbox.put({"invalid": True})
            self.inbox.put(None)

        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()
        self.err_reader = threading.Thread(
            target=lambda: self.stderr.extend(self.process.stderr), daemon=True
        )
        self.err_reader.start()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGTERM)
            self.process.wait(timeout=5)
        self.reader.join(timeout=2)
        self.err_reader.join(timeout=2)

    def send(self, msg):
        print(json.dumps(msg), file=self.process.stdin, flush=True)

    def until(self, predicate, label, timeout=25):
        deadline = time.monotonic() + timeout
        while True:
            try:
                frame = self.inbox.get(timeout=max(0.01, deadline - time.monotonic()))
            except queue.Empty as e:
                raise AssertionError(
                    f"Timed out waiting for {label}: {[f.get('type') for f in self.frames]}"
                ) from e
            if frame is None:
                raise AssertionError(
                    f"Claude exited before {label}: {''.join(self.stderr)[-1200:]}"
                )
            self.frames.append(frame)
            if predicate(frame):
                return frame

    def prompt(self, text):
        self.send(
            {
                "type": "user",
                "message": {"role": "user", "content": text},
                "parent_tool_use_id": None,
                "session_id": "",
            }
        )

    def control(self, kind, **fields):
        request_id = str(uuid.uuid4())
        self.send(
            {
                "type": "control_request",
                "request_id": request_id,
                "request": {"subtype": kind, **fields},
            }
        )
        return request_id

    def permission(self):
        return self.until(
            lambda f: (
                f.get("type") == "control_request"
                and f.get("request", {}).get("subtype") == "can_use_tool"
            ),
            "native Claude permission request",
        )

    def decide(self, req, allow):
        decision = (
            {"behavior": "allow", "updatedInput": req["request"]["input"]}
            if allow
            else {"behavior": "deny", "message": "Declined in OX fixture"}
        )
        self.send(
            {
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": req["request_id"],
                    "response": decision,
                },
            }
        )

    def result(self):
        return self.until(lambda f: f.get("type") == "result", "result")


def setup(root, claude, stack):
    server = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), Provider))
    server.requests = []
    server.errors = []
    server.expected_auth = "fixture-vendor-only"
    threading.Thread(target=server.serve_forever, daemon=True).start()
    stack.callback(server.shutdown)
    for sub in ["native", "home", "bin", "codex", "Alpha", "Beta", "tmp"]:
        (root / sub).mkdir(mode=0o700)
    (root / "bin/claude").symlink_to(claude)
    (root / "codex/codeox.toml").write_text(
        '[providers.fixture]\nname="Fixture"\nwire_api="anthropic_messages"\n'
        f'base_url="http://127.0.0.1:{server.server_port}/v1"\n'
        'discovery=false\nenv_key="OX_CLAUDE_FIXTURE_KEY"\n'
        '[[providers.fixture.models]]\nid="demo"\ncontext_window=131072\n'
    )
    env = {
        "PATH": str(root / "bin") + ":/usr/bin:/bin",
        "HOME": str(root / "home"),
        "CLAUDE_CONFIG_DIR": str(root / "native"),
        "CODEX_HOME": str(root / "codex"),
        "TMPDIR": str(root / "tmp"),
        "LANG": "C",
        "TERM": "dumb",
        "SHELL": "/bin/bash",
        "OX_CLAUDE_FIXTURE_KEY": server.expected_auth,
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DISABLE_AUTOUPDATER": "1",
    }
    for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        env[key] = env[key.lower()] = "http://127.0.0.1:1"
    env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost"
    return server, env


def native_endpoint(root, stack):
    config = root / "cert.conf"
    config.write_text(
        "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=api.anthropic.com\n"
        "[ext]\nsubjectAltName=DNS:api.anthropic.com\nbasicConstraints=critical,CA:TRUE\n"
    )
    cert = root / "cert.pem"
    key = root / "key.pem"
    subprocess.run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-config",
            config,
            "-keyout",
            key,
            "-out",
            cert,
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    server = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), Provider))
    server.native_tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    server.native_tls.load_cert_chain(cert, key)
    server.requests = []
    server.errors = []
    server.expected_auth = "Bearer sk-ant-oat01-fixture-OX"
    threading.Thread(target=server.serve_forever, daemon=True).start()
    stack.callback(server.shutdown)
    return server, cert


def workbench(root, ox, env, mode):
    harness = os.environ.get("OX_CLAUDE_WORKBENCH_TEST_BIN")
    if not harness:
        return
    if not (root / "bin" / "ox").exists():
        (root / "bin" / "ox").symlink_to(ox)
    project = root / ("Workbench" + mode.capitalize())
    project.mkdir(exist_ok=True)
    child_env = dict(env, OX_CLAUDE_WORKBENCH_CASE=mode, XDG_STATE_HOME=str(root / "state"))
    from host_fixture import session_host
    with session_host(Path(ox).with_name("ox-host"), root, child_env):
        subprocess.run([harness, "--ignored", "--exact", "views::workbench::tests::claude_workbench_session", "--nocapture"], env=child_env, cwd=project, check=True, timeout=210)


def run(root, ox, claude, api_only=False):
    with ExitStack() as stack:
        vendor, env = setup(root, claude, stack)
        api = ["--model", "fixture/demo"]
        a, b = root / "Alpha", root / "Beta"
        with Session(ox, a, env, api) as first, Session(ox, b, env, api) as second:
            first.prompt("APPROVAL Alpha")
            second.prompt("APPROVAL Beta")
            qa = first.permission()
            qb = second.permission()
            assert qa["request_id"] != qb["request_id"]
            assert qa["request"]["tool_name"] == qb["request"]["tool_name"] == "Bash"
            assert (
                not (a / "approved.txt").exists() and not (b / "approved.txt").exists()
            )
            # The new user message reaches the active process while Bash is waiting.
            first.prompt("STEER Alpha while Beta is still waiting")
            first.decide(qa, True)
            ra = first.result()
            assert (
                not ra["is_error"]
                and (a / "approved.txt").read_text().strip() == "accepted"
            )
            assert not (b / "approved.txt").exists(), (
                "one project's approval leaked into another"
            )
            deadline = time.monotonic() + 8
            while not any(
                "STEER Alpha" in json.dumps(r["messages"]) for r in vendor.requests
            ):
                if time.monotonic() >= deadline:
                    raise AssertionError(
                        "Steering never reached the active Claude process"
                    )
                time.sleep(0.05)
            second.decide(qb, False)
            rb = second.result()
            assert not rb["is_error"] and not (b / "approved.txt").exists()
            first_id, second_id = ra["session_id"], rb["session_id"]
            second.prompt("APPROVAL interrupted")
            second.permission()
            iid = second.control("interrupt")
            second.until(
                lambda f: (
                    f.get("type") == "control_response"
                    and f["response"]["request_id"] == iid
                ),
                "interrupt acknowledgement",
            )
            assert not (b / "approved.txt").exists()
        print("Concurrent Claude approvals, denial and interrupt passed.", flush=True)
        before = len(vendor.requests)
        with Session(ox, a, env, api, "--resume", first_id) as resumed:
            resumed.prompt("Relaunch with history")
            result = resumed.result()
            assert not result["is_error"] and result["session_id"] == first_id
        assert before < len(vendor.requests) and "APPROVAL Alpha" in json.dumps(
            vendor.requests[before]["messages"]
        )
        with Session(ox, a, env, api, "--resume", first_id, "--fork-session") as fork:
            fork.prompt("SUBAGENT")
            result = fork.result()
            assert not result["is_error"] and result["session_id"] not in (
                first_id,
                second_id,
            )
            agents = [
                f
                for f in fork.frames
                if f.get("subtype") == "task_started"
                and f.get("task_type") == "local_agent"
            ]
            assert agents and any(
                f.get("parent_tool_use_id") == agents[0]["tool_use_id"]
                and f.get("type") == "assistant"
                for f in fork.frames
            )
            assert any(
                f.get("subtype") == "task_notification"
                and f.get("task_id") == agents[0]["task_id"]
                for f in fork.frames
            )
        profile = root / "codex/ox-clients/claude/projects"
        assert all(
            len(list(profile.rglob(i + ".jsonl"))) == 1
            for i in [first_id, second_id, result["session_id"]]
        )
        assert not list((root / "native").rglob("*.jsonl"))
        print(
            "Stock relaunch, fork, API identity and nested native subagent passed.",
            flush=True,
        )
        if os.environ.get("OX_CLAUDE_WORKBENCH_TEST_BIN"):
            alternate = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), Provider))
            alternate.requests = []
            alternate.errors = []
            alternate.expected_auth = "fixture-other-only"
            threading.Thread(target=alternate.serve_forever, daemon=True).start()
            stack.callback(alternate.shutdown)
            with (root / "codex" / "codeox.toml").open("a") as config:
                config.write(
                    '\n[providers.other]\nname="Other"\nwire_api="anthropic_messages"\n'
                    f'base_url="http://127.0.0.1:{alternate.server_port}/v1"\n'
                    'discovery=false\nenv_key="OX_CLAUDE_OTHER_FIXTURE_KEY"\n'
                    '[[providers.other.models]]\nid="demo"\ncontext_window=131072\n'
                )
            env["OX_CLAUDE_OTHER_FIXTURE_KEY"] = alternate.expected_auth
            workbench(root, ox, env, "api")
            assert alternate.requests and not alternate.errors and not vendor.errors
            assert all("ONLY THE SECOND ALIAS" not in json.dumps(r["messages"]) for r in vendor.requests)
            assert any("ONLY THE SECOND ALIAS" in json.dumps(r["messages"]) for r in alternate.requests)

        workbench(root, ox, env, "signin")
        if api_only:
            return
        native, cert = native_endpoint(root, stack)
        # A self-signed loopback endpoint impersonates only the synthetic test
        # client's Anthropic endpoint. Other external proxy targets are rejected.
        native_env = dict(env, NODE_EXTRA_CA_CERTS=str(cert))
        for k in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            native_env[k] = native_env[k.lower()] = (
                f"http://127.0.0.1:{native.server_port}"
            )
        token = native.expected_auth.removeprefix("Bearer ")
        native_dir = root / "native"
        wrong = subprocess.run(
            [
                ox,
                "claude",
                "--subscription",
                "-p",
                "--verbose",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
            ],
            check=False,
            input="",
            cwd=a,
            env=dict(native_env, ANTHROPIC_API_KEY="fixture-console-key"),
            text=True,
            capture_output=True,
            timeout=10,
        )
        assert wrong.returncode and "ox login claude" in wrong.stderr, (
            "A Console key passed the subscription preflight"
        )
        assert (
            not native.requests
            and "fixture-console-key" not in wrong.stdout + wrong.stderr
        )

        (native_dir / ".credentials.json").write_text(
            json.dumps(
                {
                    "claudeAiOauth": {
                        "accessToken": token,
                        "refreshToken": "fixture-refresh",
                        "expiresAt": int(time.time() * 1000) + 3600000,
                        "scopes": ["user:inference", "user:profile"],
                        "subscriptionType": "max",
                        "rateLimitTier": "default_claude_max_5x",
                    }
                }
            )
        )
        (native_dir / ".credentials.json").chmod(0o600)
        (native_dir / ".claude.json").write_text(
            json.dumps(
                {
                    "oauthAccount": {
                        "accountUuid": str(uuid.uuid4()),
                        "organizationUuid": str(uuid.uuid4()),
                        "emailAddress": "desktop@example.invalid",
                    }
                }
            )
        )
        subprocess.run([ox, "login", "claude", "status"], env=native_env, cwd=a,
                       stdin=subprocess.DEVNULL, check=True, timeout=15)
        before = len(vendor.requests)
        with Session(ox, a, native_env, ["--subscription"]) as client:
            rid = client.control("initialize")
            init = client.until(
                lambda f: (
                    f.get("type") == "control_response"
                    and f["response"]["request_id"] == rid
                ),
                "initialize",
            )
            account = init["response"]["response"]["account"]
            assert (
                account["apiProvider"] == "firstParty" and account["subscriptionType"]
            )
            assert not account.get("apiKeySource")
            client.prompt("Subscription-only fixture")
            result = client.result()
            assert not result["is_error"], result
        assert native.requests and not native.errors
        assert len(vendor.requests) == before and not vendor.errors
        assert (
            len(list((native_dir / "projects").rglob(result["session_id"] + ".jsonl")))
            == 1
        )
        assert not list(profile.rglob(result["session_id"] + ".jsonl"))
        workbench(root, ox, native_env, "subscription")
        native_count = len(native.requests)
        with Session(
            ox,
            b,
            dict(
                native_env,
                ANTHROPIC_API_KEY="other-console-key",
                CLAUDE_CODE_OAUTH_TOKEN=token,
            ),
            api,
        ) as again:
            again.prompt(
                "Use only the vendor alias while a real native fixture login is available"
            )
            api_result = again.result()
            assert not api_result["is_error"]
        assert len(native.requests) == native_count and len(vendor.requests) > before
        assert not native.errors and not vendor.errors
        assert not list(
            (native_dir / "projects").rglob(api_result["session_id"] + ".jsonl")
        )
        print(
            "Isolated stock Claude subscription billed the private native endpoint.",
            flush=True,
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ox", type=Path)
    parser.add_argument("claude", type=Path)
    parser.add_argument("--api-only", action="store_true")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ox-claude-suite-") as tmp:
        run(
            Path(tmp).resolve(),
            args.ox.resolve(strict=True),
            args.claude.resolve(strict=True),
            args.api_only,
        )
