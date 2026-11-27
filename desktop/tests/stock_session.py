"""Exercise the compiled desktop session backend with stock Codex and loopback providers."""

import base64
import json
import os
import re
import ssl
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ACCOUNT = "desktop-fixture-account"


def jwt(payload):
    def encode(value):
        return base64.urlsafe_b64encode(json.dumps(value).encode()).decode().rstrip("=")

    return f"{encode({'alg': 'none'})}.{encode(payload)}.sig"


APPS_MCP = "/backend-api/ps/mcp"


def apps_mcp_reply(body):
    """Answer stock Codex's built-in ChatGPT apps MCP server (`codex_apps`).

    On the subscription route Codex connects it at `chatgpt_base_url` + /ps/mcp.
    A 404 there makes rmcp log `UnexpectedServerResponse("HTTP 404")`, so the
    fixture serves an empty streamable-HTTP server instead.
    Returns (status, payload).
    """
    request = json.loads(body or b"{}")
    if "id" not in request:
        return 202, b""
    method = request.get("method")
    results = {
        "initialize": {
            "protocolVersion": request.get("params", {}).get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture_apps", "version": "1"},
        },
        "tools/list": {"tools": []},
        "resources/list": {"resources": []},
        "resources/templates/list": {"resourceTemplates": []},
        "prompts/list": {"prompts": []},
        "ping": {},
    }
    reply = {"jsonrpc": "2.0", "id": request["id"]}
    if method in results:
        reply["result"] = results[method]
    else:
        reply["error"] = {"code": -32601, "message": f"fixture has no {method}"}
    return 200, json.dumps(reply).encode()


class Fixture(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, status, content_type, payload):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        try:
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            # Interrupting an in-flight turn closes its inference request.
            pass

    def do_GET(self):
        if self.path == APPS_MCP:
            # No server-initiated SSE stream, as the MCP spec allows.
            self.reply(405, "application/json", b"")
        elif self.path == "/backend-api/wham/accounts/check":
            info = {
                "id": ACCOUNT,
                "workspace_backend_origin": self.server.tls_origin,
                "account_routing_override": "NO_CONSTRAINT",
            }
            self.reply(
                200, "application/json", json.dumps({"accounts": [info]}).encode()
            )
        elif self.path.split("?")[0].endswith("/wham/usage"):
            now = int(time.time())
            self.reply(200, "application/json", json.dumps({
                "plan_type": "plus",
                "rate_limit": {"allowed": True, "limit_reached": False,
                    "primary_window": {"used_percent": 92, "limit_window_seconds": 18000,
                                       "reset_after_seconds": 600, "reset_at": now + 600}},
            }).encode())
        elif self.path.startswith("/v1/models"):
            self.reply(
                200, "application/json", b'{"object":"list","data":[{"id":"demo"}]}'
            )
        else:
            self.reply(404, "application/json", b"{}")

    def do_DELETE(self):
        self.reply(405 if self.path == APPS_MCP else 404, "application/json", b"")

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        if self.path == APPS_MCP:
            status, payload = apps_mcp_reply(body)
            self.reply(status, "application/json", payload)
            return
        if self.path in ("/backend-api/codex/responses", "/v1/responses"):
            if self.path.startswith("/backend-api"):
                assert self.headers["Authorization"] == f"Bearer {self.server.access}"
                assert self.headers["chatgpt-account-id"] == ACCOUNT
                assert self.server.https
            else:
                assert self.headers["Authorization"] == "Bearer fixture-only"
            if self.server.mode == "native" and b"verify-picked-settings" in body and not (self.server.project / "model-settings-received").exists():
                params = json.loads(body)
                expected = json.loads((self.server.project / "expected-model-settings.json").read_text())
                assert params["model"] == expected["model"], params["model"]
                assert params["reasoning"]["effort"] == expected["effort"], params["reasoning"]
                if expected["tier"]:
                    assert params.get("service_tier") == expected["tier"], (params.get("service_tier"), expected["tier"])
                (self.server.project / "model-settings-received").touch()
            item = {
                "id": "msg_fixture",
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": "fixture complete",
                        "annotations": [],
                    }
                ],
            }
            naming = b"Name a conversation using its first user message" in body
            if naming:
                assert not json.loads(body).get("tools")
                item["content"][0]["text"] = "Fixture conversation"
            elif self.server.mode == "host-cut":
                self.server.on_turn(json.loads(body))
            elif self.server.mode in ("ox-workers", "worker-cwd", "worker-inherit"):
                payload = json.loads(body)
                result = next(
                    (
                        i.get("output")
                        for i in payload.get("input", [])
                        if i.get("type") == "function_call_output"
                        and i.get("call_id") == "start-fixture-job"
                    ),
                    None,
                )
                if result is not None and "job_id" not in str(result):
                    item["content"][0]["text"] = f"OX worker launch failed: {result}"
                if result is None:
                    tools = payload["tools"]
                    choices = [(t.get("name", ""), {}) for t in tools]
                    choices.extend(
                        (child["name"], {"namespace": group["name"]})
                        for group in tools
                        for child in group.get("tools", [])
                    )
                    match = next(
                        (
                            (name, ns)
                            for name, ns in choices
                            if name.endswith("start_ox_worker")
                        ),
                        None,
                    )
                    assert match is not None, (
                        f"Fixture expected OX worker in tools {choices}"
                    )
                    name, namespace = match
                    args = {
                        "runtime": "codex",
                        "mode": "session",
                        "model": "fixture/demo",
                        "task": "OX worker fixture task",
                    }
                    if self.server.mode == "worker-inherit":
                        args["checkout"] = str(self.server.project.with_name("worker-checkout"))
                    if self.server.mode == "worker-cwd":
                        args["workspace_write"] = True
                    item = {
                        "id": "fc_start",
                        "type": "function_call",
                        "status": "completed",
                        "call_id": "start-fixture-job",
                        "name": name,
                        "arguments": json.dumps(args),
                        **namespace,
                    }
            events = [
                {"type": "response.output_item.done", "output_index": 0, "item": item},
                {
                    "type": "response.completed",
                    "response": {
                        "id": "resp_fixture",
                        "object": "response",
                        "created_at": 1700000000,
                        "model": "demo",
                        "status": "completed",
                        "output": [item],
                        "usage": {
                            "input_tokens": 1,
                            "output_tokens": 1,
                            "total_tokens": 2,
                        },
                    },
                },
            ]
            stream = "".join(
                "event: " + e["type"] + "\ndata: " + json.dumps(e) + "\n\n"
                for e in events
            )
        elif self.path in ("/v1/chat/completions", "/other/v1/chat/completions"):
            req = json.loads(body)
            if self.path.startswith("/other/"):
                assert self.headers["Authorization"] == "Bearer other-only"
                assert "model-switch-foreign" in str(req["messages"])
                (self.server.project / "other-provider-received").touch()
            else:
                assert self.headers["Authorization"] == "Bearer fixture-only"
            if any(m.get("role") == "user" and "model-switch-second" in str(m.get("content")) for m in req["messages"]):
                assert req["model"] == "alternate", req["model"]
            else:
                assert req["model"] == "demo", req["model"]
            has_output = any(m["role"] == "tool" for m in req["messages"])
            chunks = [{"role": "assistant"}]
            calls = None
            n_calls = 1
            naming = b"Name a conversation using its first user message" in body
            if naming:
                assert not req.get("tools")
                chunks.append({"content":"Fixture conversation"})
            elif any(m.get("role") == "user" and "model-switch-" in str(m.get("content")) for m in req["messages"]):
                chunks.append({"content":"fixture complete"})
            elif self.server.mode == "fleet-advance":
                chunks.append({"content": "Finished the private source turn."})
            elif self.server.mode == "fleet-offline" and b"OX-READ-WHILE" in body:
                last_user = max(i for i,m in enumerate(req["messages"]) if m["role"]=="user")
                if any(m["role"]=="tool" for m in req["messages"][last_user+1:]):
                    chunks.append({"content":"Recorded source availability."})
                else:
                    name = next(t["function"]["name"] for t in req["tools"] if t["function"]["name"].endswith("read_ox_thread"))
                    args = {"limit": 1}
                    calls = (name,args)
            elif self.server.mode == "fleet-return" and b"Continue on Studio and read the original source" in body:
                last_user = max(i for i,m in enumerate(req["messages"]) if m["role"]=="user")
                if any(m["role"]=="tool" for m in req["messages"][last_user+1:]):
                    chunks.append({"content":"Retrieved the original Studio conversation."})
                else:
                    name = next(t["function"]["name"] for t in req["tools"] if t["function"]["name"].endswith("read_ox_thread"))
                    n_calls = 5
                    args = {"host":self.server.original_host,"session":self.server.original_session,"limit":100}
                    calls = (name,args)
            elif self.server.mode == "fleet-handoff" and b"Continue locally from Studio" in body:
                last_user = max(n for n, m in enumerate(req["messages"]) if m["role"] == "user")
                if any(m["role"] == "tool" for m in req["messages"][last_user+1:]):
                    chunks.append({"content":"Read the source conversation."})
                else:
                    name = next(t["function"]["name"] for t in req["tools"] if t["function"]["name"].endswith("read_ox_thread"))
                    cut = re.search(rb"read_ox_thread item (\S+) returns the full item",body)
                    assert cut and b"OX_SOURCE_ONLY_END" not in body, "expected a cut Studio message"
                    args = {"item": cut.group(1).decode()}
                    calls = (name, args)
            elif self.server.mode == "fleet-handoff" and has_output and b"Inspect the remote navigation fixture" in body:
                chunks.append({"content":"Source fixture " + "abcdef"*2300 + " OX_SOURCE_ONLY_END"})
            elif self.server.mode == "linked-parent":
                if b"OX worker #" in body and b"uncommitted on laptop" in body:
                    (self.server.project / "parent-received-result").touch()
                    chunks.append({"content":"I received the remote worker report."})
                elif has_output:
                    output = str([m["content"] for m in req["messages"] if m["role"]=="tool"])
                    (self.server.project / "parent-tool-result").write_text(output)
                    chunks.append({"content":"Waiting for the remote worker."})
                else:
                    names=[t["function"]["name"] for t in req["tools"]]
                    name = next((n for n in names if n.endswith("start_ox_worker")),None)
                    assert name is not None, names
                    args = {"needs":{"memory_gb":1,"cpus":1,"os":"linux"},"runtime":"codex","mode":"session","permission_mode":"read",
                            "model":"fixture/demo","task":"Read tracked and send its contents to the parent."}
                    calls = (name,args)
            elif self.server.mode == "linked-worker":
                if b"linked-parent-offline-fixture" in body and not has_output:
                    (self.server.project / "request-received").touch()
                    deadline = time.monotonic()+60
                    while not (self.server.project / "release-request").exists() and time.monotonic()<deadline:
                        time.sleep(.05)
                if has_output:
                    output = str([m["content"] for m in req["messages"] if m["role"] == "tool"])
                    chunks.append({"content": "Linked read result: " + output})
                else:
                    name = next(t["function"]["name"] for t in req["tools"] if t["function"]["name"].endswith("exec_command"))
                    cmd = "cat tracked"
                    if b"linked-write-fixture" in body:
                        cmd = "cat tracked && echo 'from worker' > tracked && echo private > worker-output"
                    args = {"cmd": cmd}
                    calls = (name, args)
            elif self.server.mode in ("worker-cwd", "worker-inherit"):
                if has_output:
                    chunks.append({"content": "worker directory checked"})
                else:
                    name = next(t["function"]["name"] for t in req["tools"]
                                if t["function"]["name"].endswith("exec_command"))
                    args = {"cmd": "cat fixture-input > worker-marker"}
                    calls = (name, args)
            elif self.server.mode == "ox-workers":
                # Keep inference active until the UI steers and cancels this job.
                time.sleep(8)
                chunks.append({"content": "OX worker fixture answer"})
            elif self.server.mode == "workers" or (
                self.server.mode == "parallel" and any(
                    "desktop fixture worker" in str(m.get("content", "")) or
                    "desk-child-task" in str(m.get("content", ""))
                    for m in req["messages"] if m["role"] == "user"
                )
            ):
                functions = {t["function"]["name"]: t["function"] for t in req["tools"]}
                messages = req["messages"]
                child = any(
                    "desk-child-task" in str(m.get("content", ""))
                    for m in messages
                    if m["role"] == "user"
                )
                followup = any(
                    "Confirm the worker follow-up" in str(m.get("content", ""))
                    for m in messages
                    if m["role"] == "user"
                )
                if not child and not has_output:
                    calls = (
                        "spawn_agent",
                        {"message": "desk-child-task", "agent_type": "default"},
                    )
                elif child and (not has_output or followup):
                    if followup and self.server.mode == "parallel":
                        self.server.followup_requested.set()
                    calls = (
                        "exec_command",
                        {
                            "cmd": "echo child-followup"
                            if followup
                            else "echo child-probe",
                            "sandbox_permissions": "require_escalated",
                            "justification": "Approve child task",
                        },
                    )
                elif not child and not any(
                    m["role"] == "tool" and "parent-probe" in str(m.get("content", ""))
                    for m in messages
                ):
                    calls = (
                        "exec_command",
                        {
                            "cmd": "echo parent-probe",
                            "sandbox_permissions": "require_escalated",
                            "justification": "Approve parent task",
                        },
                    )
                if calls:
                    name, args = calls
                    name = next((n for n in functions if n.endswith(name)), None)
                    assert name, list(functions)
                else:
                    if child and self.server.mode == "parallel" and any(
                        "window clarification" in str(m.get("content", "")) for m in messages
                    ):
                        self.server.steered.set()
                    chunks.append({"content": "desk-worker-answer"})
            elif has_output:
                chunks.append({"content": "fixture complete"})
            else:
                name = next(
                    t["function"]["name"]
                    for t in req["tools"]
                    if t["function"]["name"].endswith("exec_command")
                )
                args = {
                    "cmd": "echo accepted > approved.txt",
                    "sandbox_permissions": "require_escalated",
                    "justification": "Approve this fixture?",
                }
                calls = (name, args)
            if calls:
                chunks.append(
                    {
                        "tool_calls": [
                            {
                                "index": index,
                                "id": "call-"
                                + str(
                                    index + sum(m["role"] == "tool" for m in req["messages"])
                                ),
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": json.dumps(args),
                                },
                            } for index in range(n_calls)
                        ]
                    }
                )
            stream = ""
            for delta in chunks + [{}]:
                finish = None if delta else ("tool_calls" if calls else "stop")
                frame = {
                    "id": "chat-demo",
                    "object": "chat.completion.chunk",
                    "created": 1700000000,
                    "model": "demo",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                }
                stream += "data: " + json.dumps(frame) + "\n\n"
            stream += "data: [DONE]\n\n"
        else:
            self.reply(404, "application/json", b"{}")
            return
        self.reply(200, "text/event-stream", stream.encode())


def run():
    rust_test = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else None
    ox = Path(os.environ["OX_TEST_OX_BIN"]).resolve(strict=True)
    codex = Path(os.environ["CODEOX_TEST_CODEX_BIN"]).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="ox-desktop-tests-") as temp:
        root = Path(temp)
        binaries = root / "bin"
        binaries.mkdir()
        (binaries / "ox").symlink_to(ox)
        (binaries / "codex").symlink_to(codex)
        if node := shutil.which("node"):
            (binaries / "node").symlink_to(node)  # Installed npm wrappers use /usr/bin/env node.

        def openssl(*args):
            subprocess.run(
                ["openssl", *args], cwd=root, check=True, stderr=subprocess.DEVNULL
            )

        openssl(
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=Desktop test CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
        )
        openssl(
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=127.0.0.1",
            "-keyout",
            "server.key",
            "-out",
            "server.csr",
        )
        (root / "server.ext").write_text(
            "subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n"
        )
        openssl(
            "x509",
            "-req",
            "-in",
            "server.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-out",
            "server.pem",
            "-days",
            "1",
            "-extfile",
            "server.ext",
        )
        cert = root / "ca.pem"
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(root / "server.pem", root / "server.key")
        with (
            ThreadingHTTPServer(("127.0.0.1", 0), Fixture) as http,
            ThreadingHTTPServer(("127.0.0.1", 0), Fixture) as https,
        ):
            https.socket = tls.wrap_socket(https.socket, server_side=True)
            expiry = int(time.time()) + 86400
            access = jwt({"exp": expiry})
            for server in [http, https]:
                server.tls_origin = f"https://127.0.0.1:{https.server_port}"
                server.https = server is https
                server.access = access
                threading.Thread(target=server.serve_forever, daemon=True).start()
            modes = [
                "workers",
                "ox-workers",
                "worker-cwd",
                "worker-inherit",
                "allow",
                "deny",
                "stop",
                "native",
                "wrong-auth",
                "legacy",
            ]
            selected = os.environ.get("OX_DESKTOP_TEST_CASE")
            if selected:
                assert selected in modes
                modes = [selected]
            for mode in modes:
                http.mode = https.mode = mode
                base = root / mode
                home = base / "codex"
                home.mkdir(parents=True)
                project = base / "project"
                project.mkdir()
                http.project = https.project = project
                native_base = f"http://127.0.0.1:{http.server_port}/backend-api"
                vendor_base = f"http://127.0.0.1:{http.server_port}/v1"
                (home / "config.toml").write_text(
                    f'model = "gpt-5.1-codex"\nchatgpt_base_url = "{native_base}"\n'
                )
                if mode == "native":
                    with (home / "config.toml").open("a") as config:
                        config.write("[features]\ngoals=true\nenable_request_compression=false\n")
                if mode in ("native", "ox-workers", "worker-cwd", "worker-inherit"):
                    identity = jwt(
                        {
                            "exp": expiry,
                            "email": "fixture@example.invalid",
                            "https://api.openai.com/auth": {
                                "chatgpt_account_id": ACCOUNT,
                                "chatgpt_plan_type": "plus",
                            },
                        }
                    )
                    tokens = {
                        "id_token": identity,
                        "access_token": access,
                        "refresh_token": "fixture",
                        "account_id": ACCOUNT,
                    }
                    (home / "auth.json").write_text(
                        json.dumps(
                            {
                                "OPENAI_API_KEY": None,
                                "tokens": tokens,
                                "last_refresh": time.strftime(
                                    "%Y-%m-%dT%H:%M:%SZ", time.gmtime()
                                ),
                            }
                        )
                    )
                elif mode == "wrong-auth":
                    (home / "auth.json").write_text(
                        json.dumps({"auth_mode": "apikey", "OPENAI_API_KEY": "fake"})
                    )
                elif mode == "legacy":
                    seeded_profile = home / "ox-clients" / "codex"
                    seeded_profile.mkdir(parents=True)
                    (seeded_profile / "config.toml").write_text(
                        'model_provider = "openai"\n'
                    )
                    (home / "config.toml").write_text(
                        f'model = "demo"\nmodel_provider = "legacy"\n[model_providers.legacy]\n'
                        f'name = "Fixture"\nbase_url = "{vendor_base}"\nenv_key = "OX_FIXTURE_KEY"\nwire_api = "responses"\n'
                    )
                if mode in ("ox-workers", "worker-cwd", "worker-inherit"):
                    with (home / "config.toml").open("a") as config:
                        config.write(
                            "[features]\ntool_search=false\nenable_request_compression=false\n"
                        )
                if mode not in ("legacy", "native", "wrong-auth"):
                    (home / "codeox.toml").write_text(
                        f'[providers.fixture]\nname="Fixture"\nwire_api="chat_completions"\nbase_url="{vendor_base}"\n'
                        'env_key="OX_FIXTURE_KEY"\ndiscovery=false\n[[providers.fixture.models]]\nid="demo"\ncontext_window=131072\n'
                    )
                if mode == "allow":
                    with (home / "codeox.toml").open("a") as config:
                        config.write('[[providers.fixture.models]]\nid="alternate"\ncontext_window=131072\n')
                        config.write(
                            f'[providers.other]\nname="Second"\nwire_api="chat_completions"\nbase_url="http://127.0.0.1:{http.server_port}/other/v1"\n'
                            'env_key="OX_SECOND_FIXTURE_KEY"\ndiscovery=false\n[[providers.other.models]]\nid="demo"\ncontext_window=131072\n'
                        )
                env = {
                    "HOME": str(base),
                    "CODEX_HOME": str(home),
                    "PATH": f"{binaries}:/usr/bin:/bin",
                    "OX_DESKTOP_FIXTURE_MODE": mode,
                    "OX_FIXTURE_KEY": "fixture-only",
                    "OX_SECOND_FIXTURE_KEY": "other-only",
                    "CODEX_CA_CERTIFICATE": str(cert),
                    "SSL_CERT_FILE": str(cert),
                    "NO_PROXY": "127.0.0.1,localhost",
                }
                for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"]:
                    env[k] = "http://127.0.0.1:1"
                if mode == "worker-cwd":
                    from stock_boundaries import verify_worker_directories
                    verify_worker_directories(ox, project, env)
                    print(f"PASS {mode}", flush=True)
                    continue
                if mode == "worker-inherit":
                    from host_fixture import session_host
                    from stock_boundaries import verify_hosted_inheritance
                    env["XDG_STATE_HOME"] = str(base / "state")
                    with session_host(ox.with_name("ox-host"), base, env):
                        verify_hosted_inheritance(project, env)
                    print(f"PASS {mode}", flush=True)
                    continue
                assert rust_test, "This scenario requires the compiled desktop test binary."
                case = {"workers": "stock_subagent", "ox-workers": "stock_ox_worker"}.get(mode, "stock_session")
                cases = [case]
                if mode in ("allow", "native"):
                    cases.append("stock_reopen")
                if mode == "allow":
                    cases.extend(["stock_model_switch", "stock_model_cold_routes"])
                for case in cases:
                    name = f"views::workbench::tests::{case}"
                    from host_fixture import session_host
                    env["XDG_STATE_HOME"] = str(base / "state")
                    with session_host(ox.with_name("ox-host"), base, env):
                        result = subprocess.run(
                            [rust_test, "--exact", name, "--ignored", "--nocapture"],
                            env=env, cwd=project, timeout=60, check=True,
                            stdout=subprocess.PIPE, text=True,
                        )
                    assert f"test {name} ... ok" in result.stdout, result.stdout
                if mode == "native":
                    from stock_boundaries import verify_catalog
                    verify_catalog(ox, project, env)
                    from stock_boundaries import verify_route_catalogs
                    (home / "codeox.toml").write_text(
                        f'[providers.fixture]\nname="Fixture"\nwire_api="chat_completions"\nbase_url="{vendor_base}"\n'
                        'env_key="OX_FIXTURE_KEY"\ndiscovery=false\n[[providers.fixture.models]]\nid="demo"\ncontext_window=131072\n'
                    )
                    verify_route_catalogs(ox, project, env)
                print(f"PASS {mode}", flush=True)


if __name__ == "__main__":
    run()
