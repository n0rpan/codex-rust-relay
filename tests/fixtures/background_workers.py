"""Exercise the worker MCP interface with the installed stock worker binaries."""

import base64
import http.server
import json
import os
import pathlib
import shutil
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

KEY = "fixture-background-worker-key"
GOAL = "Run the background goal fixture."
SUBSCRIPTION = "Return the ChatGPT worker fixture marker."
HOSTED_SUBSCRIPTION = "Return the hosted ChatGPT worker fixture marker."
ACCOUNT = "fixture-subscription-account"
def jwt(payload):
    def encode(value):
        return base64.urlsafe_b64encode(json.dumps(value).encode()).decode().rstrip("=")
    return f"{encode({'alg': 'none'})}.{encode(payload)}.sig"

WORKFLOW = "Run the background workflow fixture."
EDIT = "Run the background edit fixture."
HOLD_SECONDS = float(os.environ.get("OX_TEST_WORKER_HOLD_SECONDS", "0.5"))


def mcp_probe():
    config = next(arg.split("=", 1)[1] for arg in sys.argv if arg.startswith("--mcp-config="))
    endpoint = json.loads(config)["mcpServers"]["ox_workers"]["url"]
    bearer = "Bearer " + os.environ["OX_WORKER_SESSION"]

    def call(name, arguments):
        request = urllib.request.Request(
            endpoint,
            data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": name, "arguments": arguments}}).encode(),
            headers={"Authorization": bearer, "Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=25) as reply:
            rpc = json.load(reply)
        assert "error" not in rpc and not rpc["result"].get("isError"), rpc
        return rpc["result"]["structuredContent"]

    objective = {"runtime": "codex", "mode": "goal", "model": "fixture/worker-model", "task": GOAL}
    job = call("start_ox_worker", objective)
    assert job["status"] == "running" and not job["result"], job
    job_id = {"job_id": job["job_id"]}
    catalog = call("list_ox_models", {})
    assert {row["model"] for row in catalog["models"]} == {
        "fixture/worker-model", "messages/claude-sonnet-5",
    }
    assert all(row["auth"] == "api_key" for row in catalog["models"])
    assert {route["runtime"]: route["worker_available"] for route in catalog["subscriptions"]} == {
        "codex": True, "claude": True,
    }
    assert call("list_ox_models", {"query": "anthropic"})["subscriptions"][0]["provider"] == "anthropic"
    native = call("start_ox_worker", {"runtime":"codex", "mode":"session", "billing":"subscription", "task":SUBSCRIPTION})
    completed = wait_for_job(call, {"job_id":native["job_id"]})
    assert completed["status"] == "completed" and completed["result"] == "ChatGPT worker confirmed", completed
    assert completed["billing_label"] == "ChatGPT subscription" and completed["model"] == "ChatGPT subscription", completed
    # The OS keyring can be empty or contain a different auth method even when
    # the configured credential store advertises subscription support.
    cfg = pathlib.Path(os.environ["CODEX_HOME"])
    config, saved = cfg / "config.toml", cfg / "auth.json"
    old_config, old_auth = config.read_bytes(), saved.read_bytes()
    try:
        config.write_bytes(b'cli_auth_credentials_store="auto"\n' + old_config)
        saved.write_text(json.dumps({"OPENAI_API_KEY":"fixture-wrong-account"}))
        denied = call("start_ox_worker", {"runtime":"codex","mode":"session","billing":"subscription","task":"Wrong account fixture"})
        stopped = wait_for_job(call, {"job_id":denied["job_id"]})
        assert stopped["status"] == "failed" and "ChatGPT sign-in" in stopped["error"] and not stopped["native_session"], stopped
    finally:
        config.write_bytes(old_config)
        saved.write_bytes(old_auth)
    status = wait_for_job(call, job_id)
    assert status["status"] == "completed" and status["native_session"], status
    assert status["result"] == "background goal complete", status
    print("Claude-side MCP started an actual Codex goal and collected its result.", flush=True)

    workflow = call("start_ox_worker", {
        "runtime": "claude", "mode": "workflow", "model": "messages/claude-sonnet-5", "task": WORKFLOW,
    })
    assert workflow["status"] == "running", workflow
    status = wait_for_job(call, {"job_id": workflow["job_id"]})
    assert status["status"] == "completed" and status["native_session"], status
    assert status["result"] == "background workflow complete", status
    print("MCP started an actual Claude Code workflow and collected its result.", flush=True)

    running = call("start_ox_worker", {"runtime":"claude", "mode":"session",
                                        "model":"messages/claude-sonnet-5", "task":"Check the worker inbox."})
    ref = {"job_id": running["job_id"]}
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if any(j["job_id"] == ref["job_id"] and j["can_steer"] for j in call("list_ox_workers", {})["jobs"]):
            break
        time.sleep(0.1)
    assert call("steer_ox_worker", {**ref, "text":"Return the steering marker."})["accepted"]
    status = wait_for_job(call, ref)
    assert status["result"] == "worker steering received", status
    assert any(row["role"] == "user" and "steering marker" in row["text"] for row in status["messages"]), status
    print("A live Claude worker accepted a follow-up and returned its answer.", flush=True)

    edit = call("start_ox_worker", {
        "runtime": "claude", "mode": "session", "model": "messages/claude-sonnet-5",
        "task": EDIT, "workspace_write": True,
    })
    status = wait_for_job(call, {"job_id": edit["job_id"]})
    assert status["status"] == "completed", status
    assert pathlib.Path("worker-output.txt").read_text() == "fixed\n"
    print("A workspace-write worker edited a disposable project.", flush=True)

    cancelled = call("start_ox_worker", {**objective, "task": "Wait to be cancelled."})
    job_id = {"job_id": cancelled["job_id"]}
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        running = call("ox_worker_status", job_id)
        if running["native_session"]:
            break
        time.sleep(0.15)
    assert running["native_session"] and running["status"] == "running", running
    assert call("cancel_ox_worker", job_id)["status"] == "cancelled"
    print("A running worker was cancelled by job ID.", flush=True)


def wait_for_job(call, job_id):
    deadline = time.monotonic() + HOLD_SECONDS + 60
    while time.monotonic() < deadline:
        status = call("ox_worker_status", job_id)
        if status["status"] != "running":
            return status
        time.sleep(0.15)
    raise AssertionError(f"Worker did not finish: {status}")


class Vendor(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        if self.path == "/backend-api/wham/accounts/check":
            payload = json.dumps({"accounts":[{"id":ACCOUNT,"workspace_backend_origin":self.server.tls_origin,
                                               "account_routing_override":"NO_CONSTRAINT"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type","application/json")
            self.send_header("Content-Length",str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        else:
            self.send_error(404)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path.split("?")[0] == "/v1/messages":
            assert self.headers["x-api-key"] == KEY
            self.messages(request)
        elif self.path == "/backend-api/codex/responses":
            assert self.headers["Authorization"] == "Bearer " + self.server.native_token
            assert self.headers["ChatGPT-Account-Id"] == ACCOUNT
            inputs = json.dumps(request["input"])
            assert SUBSCRIPTION in inputs or HOSTED_SUBSCRIPTION in inputs
            if HOSTED_SUBSCRIPTION in inputs:
                self.server.hosted_worker_seen.set()
            self.responses_reply(request)
        elif self.path == "/v1/responses":
            assert self.headers["Authorization"] == "Bearer " + KEY
            self.responses_reply(request)
        else:
            self.send_error(404)

    def stream(self, events):
        payload = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def responses_reply(self, request):
        inputs = json.dumps(request["input"])
        assert GOAL in inputs or SUBSCRIPTION in inputs or HOSTED_SUBSCRIPTION in inputs or "Wait to be cancelled." in inputs, inputs[:200]
        if "Wait to be cancelled." in inputs:
            time.sleep(10)
            return
        if SUBSCRIPTION in inputs or HOSTED_SUBSCRIPTION in inputs:
            item = {"id":"msg_native", "type":"message", "role":"assistant", "status":"completed",
                    "content":[{"type":"output_text","text":"ChatGPT worker confirmed","annotations":[]}]}
        elif "call_finish_goal" not in inputs:
            assert any(tool.get("name") == "update_goal" for tool in request["tools"])
            time.sleep(HOLD_SECONDS)
            item = {"id": "fc_goal", "type": "function_call", "status": "completed",
                    "call_id": "call_finish_goal", "name": "update_goal",
                    "arguments": '{"status":"complete"}'}
        else:
            item = {"id": "msg_goal", "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "background goal complete", "annotations": []}]}
        self.stream([
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": {
                "id": "resp_goal", "object": "response", "created_at": 1700000000,
                "model": request["model"], "status": "completed", "output": [item],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5,
                          "input_tokens_details": {"cached_tokens": 0},
                          "output_tokens_details": {"reasoning_tokens": 0}},
            }},
        ])

    def messages(self, request):
        inputs = json.dumps(request["messages"])
        if "Check the worker inbox." in inputs and "Return the steering marker." not in inputs:
            time.sleep(1)
        if EDIT in inputs and "toolu_fixture_edit" not in inputs:
            assert any(tool.get("name") == "Write" for tool in request.get("tools", []))
            block = {"type": "tool_use", "id": "toolu_fixture_edit", "name": "Write", "input": {}}
            delta = {"type": "input_json_delta", "partial_json": json.dumps({
                "file_path": str(self.server.project / "worker-output.txt"), "content": "fixed\n",
            })}
            stop = "tool_use"
        elif WORKFLOW in inputs and "toolu_fixture_workflow" not in inputs:
            assert any(tool.get("name") == "Workflow" for tool in request.get("tools", []))
            block = {"type": "tool_use", "id": "toolu_fixture_workflow", "name": "Workflow", "input": {}}
            delta = {"type": "input_json_delta", "partial_json": json.dumps({"script":
                'export const meta = { name: "test-worker", description: "Disposable workflow" };\n'
                'return await agent("Reply with the workflow agent marker");'})}
            stop = "tool_use"
        else:
            if WORKFLOW in inputs:
                assert "workflow agent marker" in inputs, inputs[-500:]
            block = {"type": "text", "text": ""}
            delta = {"type": "text_delta",
                     "text": "background workflow complete" if WORKFLOW in inputs else "workflow agent marker"}
            if "Return the steering marker." in inputs:
                delta["text"] = "worker steering received"
            stop = "end_turn"
        self.stream([
            {"type": "message_start", "message": {"id": "msg_bg", "type": "message", "role": "assistant",
                "model": request["model"], "content": [], "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 3, "output_tokens": 0}}},
            {"type": "content_block_start", "index": 0, "content_block": block},
            {"type": "content_block_delta", "index": 0, "delta": delta},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
             "usage": {"output_tokens": 2}},
            {"type": "message_stop"},
        ])


def test():
    codex = pathlib.Path(os.environ["CODEOX_TEST_CODEX_BIN"]).resolve()
    claude = pathlib.Path(os.environ["OX_TEST_CLAUDE_BIN"]).resolve()
    ox = pathlib.Path(os.environ.get("OX_TEST_BINARY", "target/release/ox")).resolve()
    with tempfile.TemporaryDirectory(prefix="ox-background-worker-") as directory:
        cfg = pathlib.Path(directory)
        path = cfg / "bin"
        path.mkdir()
        (path / "codex").symlink_to(codex)
        shim = path / "claude"
        shim.write_text(
            '#!/bin/sh\nfor arg in "$@"; do\n'
            '  if [ "$arg" = worker-mcp-probe ]; then exec "$PYTHON" "$FIXTURE" "$@"; fi\n'
            'done\nexec "$STOCK_CLAUDE" "$@"\n'
        )
        shim.chmod(0o700)
        def openssl(*args):
            subprocess.run(["openssl",*args], cwd=cfg, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
        openssl("req","-x509","-newkey","rsa:2048","-nodes","-subj","/CN=Worker Fixture CA","-days","1",
                "-keyout","ca.key","-out","ca.pem","-addext","basicConstraints=critical,CA:TRUE")
        openssl("req","-new","-newkey","rsa:2048","-nodes","-subj","/CN=127.0.0.1","-keyout","server.key","-out","server.csr")
        (cfg / "server.ext").write_text("subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n")
        openssl("x509","-req","-in","server.csr","-CA","ca.pem","-CAkey","ca.key","-CAcreateserial",
                "-out","server.pem","-days","1","-extfile","server.ext")
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(cfg / "server.pem", cfg / "server.key")
        with (http.server.ThreadingHTTPServer(("127.0.0.1", 0), Vendor) as server,
              http.server.ThreadingHTTPServer(("127.0.0.1", 0), Vendor) as workspace):
            workspace.hosted_worker_seen = threading.Event()
            workspace.socket = tls.wrap_socket(workspace.socket,server_side=True)
            server.tls_origin = workspace.tls_origin = f"https://127.0.0.1:{workspace.server_port}"
            threading.Thread(target=workspace.serve_forever, daemon=True).start()
            threading.Thread(target=server.serve_forever, daemon=True).start()
            url = f"http://127.0.0.1:{server.server_port}/v1"
            (cfg / "codeox.toml").write_text(f"""
[providers.fixture]
base_url = "{url}"
env_key = "WORKER_API_KEY"
wire_api = "responses"
discovery = false
[[providers.fixture.models]]
id = "worker-model"
[providers.messages]
base_url = "{url}"
env_key = "WORKER_API_KEY"
wire_api = "anthropic_messages"
discovery = false
[[providers.messages.models]]
id = "claude-sonnet-5"
""")
            (cfg / "config.toml").write_text(
                f'chatgpt_base_url = "http://127.0.0.1:{server.server_port}/backend-api"\n'
                '[features]\napps=false\nenable_request_compression=false\n')
            (cfg / ".claude.json").write_text('{"oauthAccount":{"accountUuid":"fixture-account"}}')
            auth = {"chatgpt_account_id":ACCOUNT, "chatgpt_plan_type":"plus"}
            expires = int(time.time()) + 3600
            server.native_token = workspace.native_token = jwt({"exp":expires, "https://api.openai.com/auth":auth})
            identity = jwt({"email":"worker@example.invalid", "exp":expires, "https://api.openai.com/auth":auth})
            (cfg / "auth.json").write_text(json.dumps({"auth_mode":"chatgpt", "tokens":{
                "access_token":server.native_token, "id_token":identity, "refresh_token":"fixture", "account_id":ACCOUNT
            }, "last_refresh":time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}))
            server.project = cfg
            env = {
                "HOME": directory, "CODEX_HOME": directory, "TMPDIR": directory,
                "PATH": f"{path}:/usr/bin:/bin", "WORKER_API_KEY": KEY,
                "CODEX_CA_CERTIFICATE": str(cfg / "ca.pem"), "SSL_CERT_FILE": str(cfg / "ca.pem"),
                "PYTHON": sys.executable, "FIXTURE": str(pathlib.Path(__file__).resolve()),
                "STOCK_CLAUDE": str(claude), "NO_PROXY": "127.0.0.1,localhost",
                "HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1",
                "ALL_PROXY": "http://127.0.0.1:1", "DISABLE_AUTOUPDATER": "1",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "ENABLE_TOOL_SEARCH": "false",
                "OX_TEST_WORKER_HOLD_SECONDS": str(HOLD_SECONDS),
            }
            subprocess.run([ox, "claude", "--native", "worker-mcp-probe"], env=env, cwd=cfg,
                           stdin=subprocess.DEVNULL, timeout=HOLD_SECONDS + 180, check=True)
            if host_bin := os.environ.get("OX_TEST_HOST_BIN"):
                from contextlib import closing
                sys.path.insert(0,str(pathlib.Path(__file__).resolve().parents[2] / "desktop" / "tests"))
                from host_fixture import session_host
                from host_codex_interrupt import ready
                from host_stock_session import Client
                project = cfg / "project"
                project.mkdir()
                shutil.copy2(host_bin, path / "ox-host")
                (path / "ox").symlink_to(ox)
                env["XDG_STATE_HOME"] = str(cfg / "state")
                scope = {"project":str(project),"route":{"runtime":"codex","billing":"subscription"}}
                with session_host(path / "ox-host", cfg, env), closing(Client(env["OX_HOST_SOCKET"])) as client:
                    ready(client, scope)
                    client.act(scope, "seed", {"send_text":[None,SUBSCRIPTION]})
                    parent = client.until(lambda f:f["type"]=="opened")["thread"]
                    def finished(frame, child):
                        return next((d["thread"] for d in frame.get("changes",[]) if d["kind"]=="notification"
                                     and d["event"]=="finished" and (d["thread"] != parent if child else d["thread"] == parent)),None)
                    client.until(lambda f:finished(f,False))
                    client.act(scope,"native-worker",{"start_worker":{"parent":parent,"runtime":"codex",
                                "mode":"session","task":HOSTED_SUBSCRIPTION,"permission":"read","host":"parent"}})
                    client.until(lambda f:finished(f,True))
                    assert workspace.hosted_worker_seen.is_set()
                    print("Host-owned Codex worker finished on the selected ChatGPT account.", flush=True)


if "worker-mcp-probe" in sys.argv:
    mcp_probe()
else:
    test()
