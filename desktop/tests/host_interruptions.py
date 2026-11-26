"""Interrupt the dogfood scenario at every awkward moment and check what the model sees next.

Scenario, with real stock Claude Code (lead and worker, both through `ox claude` on
an API route) against a scripted Chat Completions fixture that records every request:

  lead starts an API worker (unstarred catalog alias)
    -> worker runs `cd` (a counted side effect)
    -> worker reads two screenshots (two tool results that carry images)
    -> worker finishes with a final answer
    -> lead asks ox_worker_status and answers; the parent is ended (reaped)
    -> user sends again

Each case in CASES cuts the run at one point, restarts or reconnects the way a user
would, sends "continue" and asserts the next provider request(s) exactly:
well-formed tool-call/result pairing, images attributed to their call, exactly one
delivery of each user message, one route identity, no tool executed twice, the
worker's final answer still available through ox_worker_status, and that bytes already
in the stock client's journal were not rewritten (prefix check).

To add a case: write `def case_x(ctx)` and add a row to CASES.

Usage: python3 desktop/tests/host_interruptions.py /path/to/ox-host /path/to/ox /path/to/claude [case ...]
"""
from contextlib import ExitStack
import base64
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import struct
import sys
import tempfile
import threading
import time
import uuid
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from claude_stock_session import Provider, native_endpoint
from host_fixture import session_host
from host_stock_session import Client, ask, change

ROUTE = {"runtime": "claude", "billing": "subscription"}
WORKER_ROUTE = {"runtime": "claude", "billing": {"api": "fixture/demo"}}
LEAD_PROMPT = "DOGFOOD go"
WORKER_TASK = "WORKER-TASK: cd, then look at both screenshots, then report."
PART1 = "WORKER-PARTIAL: the first screenshot is red and"
PART2 = " the second is blue."
FINAL = "WORKER-FINAL: the two screenshots show a red and a blue square."


def png(rgb):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))
    raw = b"".join(b"\0" + bytes(rgb) * 4 for _ in range(4))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 4, 4, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def data_uri(rgb):
    return "data:image/png;base64," + base64.b64encode(png(rgb)).decode()


def blocks_of(m):
    return m["content"] if isinstance(m["content"], list) else [{"type": "text", "text": m["content"]}]


class Fixture:
    """A Chat Completions provider that answers from the request history alone."""

    def __init__(self, root):
        self.root = root
        self.requests = []          # [(agent, body)] for every non-title request
        self.lock = threading.Lock()
        self.gates = {}             # name -> Event the test sets to release a held reply
        self.reached = {}           # name -> Event set when the provider holds at that point
        self.worker_done = threading.Event()
        self.parent_waits_for_notice = False
        self.hold = set()           # gate names currently armed
        self.drop_at = {}           # name -> callable run when the gate is reached
        self.worker_final_sent = 0
        self.authz = []
        self.calls = 0
        self.partial = False
        self.release_rest = False
        self.bash_sleep = 0
        self.settle_delay = 2.0
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.server.daemon_threads = True
        self.port = self.server.server_port
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        for event in self.gates.values():
            event.set()

    def arm(self, name):
        self.hold.add(name)
        self.gates[name] = threading.Event()
        self.reached[name] = threading.Event()

    def pause(self, name):
        if name in self.hold:
            self.reached[name].set()
            self.gates[name].wait(60)

    def of(self, agent):
        with self.lock:
            return [b for a, b in self.requests if a == agent]

    # -- scripting -----------------------------------------------------------
    @staticmethod
    def names(messages):
        out = {}
        for m in messages:
            for call in m.get("tool_calls") or []:
                out[call["id"]] = call["function"]["name"]
        return out

    @staticmethod
    def text_of(content):
        if isinstance(content, list):
            return "".join(p.get("text", "") for p in content if isinstance(p, dict))
        return content or ""

    def script(self, body):
        messages = body["messages"]
        first = next((self.text_of(m["content"]) for m in messages if m["role"] == "user"), "")
        agent = "worker" if "WORKER-TASK" in first else "lead"
        last = messages[-1]
        names = self.names(messages)
        results = [(names.get(m["tool_call_id"], "?"), self.text_of(m["content"]))
                   for m in messages if m["role"] == "tool"]
        if agent == "worker":
            done = [n for n, _ in results]
            if not done:
                return agent, [("tool", "Bash", {
                    "command": f"echo ran >> {self.root}/Alpha/cd.log && sleep {self.bash_sleep} && cd {self.root}/Alpha",
                    "description": "Enter the project"})]
            reads = done.count("Read")
            if reads == 0:
                return agent, [("tool", "Read", {"file_path": str(self.root / "shots/one.png")})]
            if reads == 1:
                self.pause("after_shot1")
                return agent, [("tool", "Read", {"file_path": str(self.root / "shots/two.png")})]
            self.pause("before_final")
            if self.partial:
                return agent, [("partial", PART1, PART2)]
            self.worker_final_sent += 1
            self.worker_done.set()
            return agent, [("text", FINAL)]
        raise AssertionError("the chat provider only serves the worker")

    # -- the lead speaks Anthropic Messages through the subscription fixture ----
    def lead_script(self, body, prompt):
        messages = conversation(body)
        names, results = {}, []
        for m in messages:
            for block in m["content"] if isinstance(m["content"], list) else []:
                if block.get("type") == "tool_use":
                    names[block["id"]] = block["name"]
                if block.get("type") == "tool_result":
                    results.append((names.get(block["tool_use_id"], "?"), block))
        last = messages[-1]
        blocks = last["content"] if isinstance(last["content"], list) else []
        tool = lambda name, args, tid: [{"type": "tool_use", "id": tid, "name": name, "input": args}]
        job = None
        for n, r in results:
            if n.endswith("start_ox_worker"):
                job = json.loads(r["content"] if isinstance(r["content"], str) else r["content"][0]["text"])["job_id"]
        assert job is not None or LEAD_PROMPT in prompt or "prep" in prompt or not results, "no worker job id to ask about"
        if LEAD_PROMPT in prompt and last["role"] == "user" and not any(b.get("type") == "tool_result" for b in blocks):
            return tool("mcp__ox_workers__start_ox_worker", {
                "runtime": "claude", "mode": "session", "model": "fixture/demo",
                "task": WORKER_TASK, "permission_mode": "write"}, "toolu_lead_start")
        if "continue" in prompt and last["role"] == "user" and not any(b.get("type") == "tool_result" for b in blocks):
            nth = sum(1 for m in messages if m["role"] == "user" for b in blocks_of(m)
                      if b.get("type") == "text" and b["text"].strip().endswith("continue"))
            return tool("mcp__ox_workers__ox_worker_status", {"job_id": job}, f"toolu_lead_status_c{nth}")
        tr = [b for b in blocks if b.get("type") == "tool_result"]
        if tr:
            kind = names.get(tr[-1]["tool_use_id"], "")
            if kind.endswith("start_ox_worker"):
                if self.parent_waits_for_notice:
                    return [{"type": "text", "text": "I'll wait for the worker."}]
                self.pause("lead_after_start")
                self.worker_done.wait(90)
                time.sleep(self.settle_delay)  # the host has seen the worker finish
                self.pause("worker_done")
                return tool("mcp__ox_workers__ox_worker_status", {"job_id": job}, "toolu_lead_status")
            if kind.endswith("ox_worker_status"):
                self.pause("lead_status")
                return [{"type": "text", "text": "LEAD-SAW"}]
        return [{"type": "text", "text": "LEAD-OK"}]

    # -- HTTP ------------------------------------------------------------------
    def handler(self):
        fixture = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def do_POST(self):
                size = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(size))
                if not self.path.endswith("/chat/completions"):
                    self.send_error(404)
                    return
                if "Name a conversation" in json.dumps(body["messages"][:1]):
                    output = [("text", "Dogfood")]
                else:
                    agent, output = fixture.script(body) if self.record(body) else ("t", None)
                self.stream(body, output)

            def record(self, body):
                first = next((fixture.text_of(m["content"]) for m in body["messages"]
                              if m["role"] == "user"), "")
                agent = "worker" if "WORKER-TASK" in first else "lead"
                with fixture.lock:
                    fixture.requests.append((agent, body))
                    fixture.authz.append((self.headers.get("Authorization"), self.path))
                return True

            def stream_partial(self, body, output):
                """Send half an answer, hold the stream open, then (if released) the rest."""
                base = {"id": "chatcmpl-partial", "object": "chat.completion.chunk", "created": 1, "model": body["model"]}
                send = lambda c: (self.wfile.write(b"data: " + json.dumps(dict(base, **c)).encode() + b"\n\n"), self.wfile.flush())
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                try:
                    send({"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]})
                    send({"choices": [{"index": 0, "delta": {"content": output[0][1]}, "finish_reason": None}]})
                    fixture.reached["partial"].set()
                    fixture.gates["partial"].wait(60)
                    if fixture.gates["partial"].is_set() and fixture.release_rest:
                        send({"choices": [{"index": 0, "delta": {"content": output[0][2]}, "finish_reason": None}]})
                        send({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
                        send({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18}})
                        self.wfile.write(b"data: [DONE]\n\n")
                        self.wfile.flush()
                        fixture.worker_final_sent += 1
                        fixture.worker_done.set()
                except OSError:
                    pass

            def stream(self, body, output):
                if output and output[0][0] == "partial":
                    return self.stream_partial(body, output)
                chunks = []
                base = {"id": "chatcmpl-" + uuid.uuid4().hex[:12], "object": "chat.completion.chunk",
                        "created": 1, "model": body["model"]}
                calls = 0
                chunks.append({"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]})
                finish = "stop"
                for item in output:
                    if item[0] == "text":
                        chunks.append({"choices": [{"index": 0, "delta": {"content": item[1]}, "finish_reason": None}]})
                    else:
                        finish = "tool_calls"
                        fixture.calls += 1
                        cid = f"call_{item[1].split('__')[-1]}_{fixture.calls}"
                        chunks.append({"choices": [{"index": 0, "delta": {"tool_calls": [{
                            "index": calls, "id": cid, "type": "function",
                            "function": {"name": item[1], "arguments": json.dumps(item[2])}}]},
                            "finish_reason": None}]})
                        calls += 1
                chunks.append({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})
                chunks.append({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18}})
                payload = b"".join(b"data: " + json.dumps(dict(base, **c)).encode() + b"\n\n" for c in chunks)
                payload += b"data: [DONE]\n\n"
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                try:
                    self.wfile.write(payload)
                except OSError:
                    pass

        return H


class Ctx:
    """One isolated host with its fixture, home and state; restartable like a user's session."""

    def __init__(self, root, host, ox, claude, stack):
        self.root, self.stack = root, stack
        self.fixture = Fixture(root)
        stack.callback(self.fixture.close)
        for sub in ["native", "home", "bin", "codex", "Alpha", "tmp", "shots", "state"]:
            (root / sub).mkdir(mode=0o700)
        (root / "shots/one.png").write_bytes(png((200, 20, 20)))
        (root / "shots/two.png").write_bytes(png((20, 20, 200)))
        (root / "bin/claude").symlink_to(claude)
        for source, target in ((host, root / "bin/ox-host"), (ox, root / "bin/ox")):
            try:
                os.link(source, target)
            except OSError:
                shutil.copy2(source, target)
        (root / "codex/codeox.toml").write_text(
            '[providers.fixture]\nname="Fixture"\nwire_api="chat_completions"\n'
            f'base_url="http://127.0.0.1:{self.fixture.port}/v1"\n'
            'discovery=false\nenv_key="OX_FIXTURE_KEY"\n'
            '[[providers.fixture.models]]\nid="demo"\ncontext_window=131072\n')
        self.env = {
            "PATH": str(root / "bin") + ":/usr/bin:/bin", "HOME": str(root / "home"),
            "CLAUDE_CONFIG_DIR": str(root / "native"), "CODEX_HOME": str(root / "codex"),
            "TMPDIR": str(root / "tmp"), "LANG": "C", "TERM": "dumb", "SHELL": "/bin/bash",
            "OX_FIXTURE_KEY": "fixture-key", "XDG_STATE_HOME": str(root / "state"),
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_TELEMETRY": "1",
            "DISABLE_AUTOUPDATER": "1", "ENABLE_TOOL_SEARCH": "false",
        }
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            self.env[key] = self.env[key.lower()] = "http://127.0.0.1:1"
        self.env["NO_PROXY"] = self.env["no_proxy"] = "127.0.0.1,localhost"
        self.native, cert = native_endpoint(root, stack)
        self.native.script = self.fixture.lead_script
        self.native.handle_error = lambda *a: None  # killed hosts drop held replies
        self.native.auth = []
        self.env["NODE_EXTRA_CA_CERTS"] = str(cert)
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            self.env[key] = self.env[key.lower()] = f"http://127.0.0.1:{self.native.server_port}"
        cfg = root / "native"
        (cfg / ".credentials.json").write_text(json.dumps({"claudeAiOauth": {
            "accessToken": self.native.expected_auth.removeprefix("Bearer "), "refreshToken": "fixture-refresh",
            "expiresAt": int(time.time() * 1000) + 3600000, "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max", "rateLimitTier": "default_claude_max_5x"}}))
        (cfg / ".credentials.json").chmod(0o600)
        (cfg / ".claude.json").write_text(json.dumps({"oauthAccount": {
            "accountUuid": str(uuid.uuid4()), "organizationUuid": str(uuid.uuid4()),
            "emailAddress": "desktop@example.invalid"}}))
        self.scope = {"project": str(root / "Alpha"), "route": ROUTE}
        self.log = stack.enter_context((root / "host.log").open("w+"))
        self.host_proc = None
        self.clients = []

    # -- host lifecycle --------------------------------------------------------
    def start_host(self):
        manager = session_host(self.root / "bin/ox-host", self.root, self.env, self.log)
        self.host_proc = manager.__enter__()
        self._manager = manager
        return self.host_proc

    def stop_host(self, kill=False):
        for c in self.clients:
            try:
                c.conn.shutdown(socket.SHUT_RDWR)  # unblocks the reader thread before close
                c.close()
            except OSError:
                pass
        self.clients = []
        if kill:
            from host_fixture import descendants
            self.orphans = descendants(self.host_proc.pid)
            os.kill(self.host_proc.pid, signal.SIGKILL)
            for pid in self.orphans:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            self.host_proc.wait(timeout=5)
            time.sleep(0.5)
            # The host's stock children die with it; make sure nothing keeps the journals open.
        try:
            self._manager.__exit__(None, None, None)
        except Exception:
            if not kill:
                raise

    def client(self):
        c = Client(self.env["OX_HOST_SOCKET"])
        self.clients.append(c)
        return c

    def desktop(self, scope=None):
        c = self.client()
        c.open(scope or self.scope)
        state = c.until(lambda f: f["type"] == "snapshot")["scopes"][0]
        if "start" not in state["capabilities"] and "send" not in state["capabilities"]:
            c.until(lambda f: (x := change(f, "state")) and ("start" in x["capabilities"] or "send" in x["capabilities"]))
        return c

    def dump(self):
        extra = [f for p in getattr(self, "pipes", []) for f in p.frames if f["type"] in ("error", "eof")]
        print("ERRORS", extra, file=sys.stderr)
        return json.dumps({"lead": [[summ(m) for m in b["messages"]][-3:] for b in lead_requests(self)],
                           "worker": [[summ(m) for m in b["messages"] if m["role"] != "system"][-3:] for b in self.fixture.of("worker")]}, indent=1)[:6000]

    # -- journals ----------------------------------------------------------------
    def journals(self):
        return {p: p.read_bytes() for p in sorted((self.root / "native").glob("projects/*/*.jsonl"))}

    def ledger(self):
        p = self.root / "state/ox/host/ownership-v1.json"
        return p.read_bytes() if p.exists() else b""

    def side_effects(self):
        p = self.root / "Alpha/cd.log"
        return p.read_text().count("ran") if p.exists() else 0


def subprocess_kill_group(pid):
    try:
        os.killpg(pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass


class Pipe:
    """A subscribed client read by a thread: keeps every frame and approves stock asks."""

    def __init__(self, ctx, scope=None, approve=True):
        self.client = ctx.client()
        ctx.__dict__.setdefault("pipes", []).append(self)
        self.frames = []
        self.cond = threading.Condition()
        self.approve = approve
        self.asked = []
        self.scopes = {}
        self.client.conn.settimeout(None)
        self.client.open(scope)
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        while True:
            try:
                line = self.client.input.readline()
            except (OSError, ValueError):
                line = b""
            with self.cond:
                if not line:
                    self.frames.append({"type": "eof"})
                    self.cond.notify_all()
                    return
                frame = json.loads(line)
                self.frames.append(frame)
                self.cond.notify_all()
            self.handle(frame)

    def handle(self, frame):
        for state in frame.get("scopes", []):
            self.scopes[json.dumps(state["scope"], sort_keys=True)] = state["scope"]
            for a in state.get("asks", []):
                self.answer(state["scope"], a)
        if (pending := ask(frame)) and self.approve:
            scope = frame.get("scope") or self.only_scope()
            self.answer(scope, pending)

    def only_scope(self):
        return next(iter(self.scopes.values()))

    def answer(self, scope, pending):
        if not self.approve or pending["token"] in self.asked:
            return
        self.asked.append(pending["token"])
        try:
            self.client.act(scope, uuid.uuid4().hex, {"answer_ask": [pending["id"], 0]}, pending["token"])
        except OSError:
            pass

    def ready(self):
        def ok(f):
            for st in f.get("scopes", []):
                if st["connection"] == "ready":
                    return True
            c = change(f, "state")
            return bool(c and c["connection"] == "ready")
        return self.wait(ok, "ready scope")

    def wait(self, predicate, label, timeout=60):
        until = time.monotonic() + timeout
        seen = 0
        with self.cond:
            while True:
                while seen < len(self.frames):
                    frame = self.frames[seen]
                    seen += 1
                    if result := predicate(frame):
                        return frame if result is True else result
                    if frame["type"] == "eof":
                        raise RuntimeError(f"socket closed waiting for {label}")
                left = until - time.monotonic()
                if left <= 0:
                    raise AssertionError(f"timed out waiting for {label}: {[f['type'] for f in self.frames][-12:]}")
                self.cond.wait(left)


def summ(m):
    c = m.get("content")
    calls = [(t["id"], t["function"]["name"]) for t in m.get("tool_calls", [])]
    if isinstance(c, list):
        out = []
        for p in c:
            kind = p.get("type")
            if kind == "text":
                out.append(p["text"][-70:])
            elif kind == "tool_use":
                out.append(("use", p["id"], p["name"]))
            elif kind == "tool_result":
                out.append(("result", p["tool_use_id"], str(p["content"])[:120]))
            else:
                out.append(kind)
        return (m["role"], out, calls, m.get("tool_call_id"))
    return (m["role"], (c or "")[-110:], calls, m.get("tool_call_id"))


# --------------------------------------------------------------------------- scenario
class Run:
    """The dogfood scenario on one Ctx, driven step by step so a case can cut anywhere."""

    def __init__(self, ctx):
        self.ctx = ctx
        self.watcher = None
        self.desk = None
        self.thread = None

    def boot(self):
        ctx = self.ctx
        ctx.start_host()
        self.watcher = Pipe(ctx, None)
        self.watcher.wait(lambda f: f["type"] == "snapshot", "catalog snapshot")
        self.attach()
        return self

    def attach(self):
        """(Re)connect the desktop the way a window does: subscribe to its scope."""
        self.desk = Pipe(self.ctx, self.ctx.scope, approve=False)
        self.desk.ready()
        return self.desk

    def prep(self):
        d, scope = self.desk, self.ctx.scope
        d.client.act(scope, "create", {"send_text": [None, "DOGFOOD prep"]})
        self.thread = d.wait(lambda f: f["type"] == "opened", "opened")["thread"]
        d.wait(lambda f: (n := change(f, "notification")) and n["event"] == "finished", "prep finished")
        d.client.act(scope, "mode", {"set_access": [self.thread, "accept_edits"]})
        d.wait(lambda f: f["type"] == "ack" and f["id"] == "mode", "mode ack")
        time.sleep(1.5)  # a mode change reopens the session process

    def lead_saw(self, text, last_only=True):
        for body in self.ctx.native.requests:
            if "Name a conversation" in str(body.get("system")):
                continue
            last = conversation(body)[-1]
            blocks = last["content"] if isinstance(last["content"], list) else [{"type": "text", "text": last["content"]}]
            if last["role"] == "user" and any(text in b.get("text", "") for b in blocks):
                return True
        return False

    def wait_until(self, condition, label, timeout=60):
        until = time.monotonic() + timeout
        while not condition():
            if time.monotonic() > until:
                raise AssertionError(f"timed out waiting for {label}")
            time.sleep(0.05)

    def send(self, text, key):
        """Send as a window does; wait for the session to be ready first (a mode change reopens it)."""
        for attempt in range(40):
            self.desk.client.act(self.ctx.scope, key, {"send_text": [self.thread, text]})
            reply = self.desk.wait(lambda f: f["type"] in ("ack", "error") and f.get("id") == key, f"{key} reply")
            if reply["type"] == "ack":
                return
            assert "ready parent" in reply["message"], reply
            key = f"{key}-retry{attempt}"
            time.sleep(0.25)
        raise AssertionError("the session never became ready to send")

    def settle(self, timeout=90):
        """The whole scripted session is over: worker answered, lead read its status and replied."""
        self.wait_until(lambda: self.ctx.fixture.worker_done.is_set()
                        and any(status_result(b) for b in lead_requests(self.ctx)),
                        "worker final answer and the lead's status read", timeout)
        time.sleep(1.5)

    def finished(self, n=1, desk=None):
        """Wait for the lead's turn to finish (idle notification) on the current desktop."""
        (desk or self.desk).wait(lambda f: (c := change(f, "notification")) and c["event"] == "finished", "turn finished")

    def worker_ids(self):
        out = {}
        for p in (self.ctx.root / "native").glob("projects/*/*.jsonl"):
            out[p.stem] = p
        return out


def lead_requests(ctx):
    return [b for b in ctx.native.requests if "Name a conversation" not in str(b.get("system"))]


def user_texts(body):
    out = []
    for m in body["messages"]:
        if m["role"] != "user":
            continue
        for b in m["content"] if isinstance(m["content"], list) else [{"type": "text", "text": m["content"]}]:
            if b.get("type") == "text":
                out.append(b["text"])
    return out


def count_prompt(body, prompt):
    return sum(1 for t in user_texts(body) if t.strip().endswith(prompt))


def status_result(body):
    """The last ox_worker_status tool result in a lead request, as JSON."""
    found = None
    names = {}
    for m in body["messages"]:
        for b in m["content"] if isinstance(m["content"], list) else []:
            if b.get("type") == "tool_use":
                names[b["id"]] = b["name"]
            if b.get("type") == "tool_result" and names.get(b["tool_use_id"], "").endswith("ox_worker_status"):
                c = b["content"]
                found = json.loads(c if isinstance(c, str) else c[0]["text"])
    return found


def check_worker_wellformed(ctx, expect_images=True):
    """Every worker request: each tool call is answered, images follow their own call, exactly once."""
    shots = {"one.png": data_uri((200, 20, 20)), "two.png": data_uri((20, 20, 200))}
    for body in ctx.fixture.of("worker"):
        msgs = [m for m in body["messages"] if m["role"] != "system"]
        calls = {}
        for i, m in enumerate(msgs):
            for t in m.get("tool_calls") or []:
                calls[t["id"]] = (i, t["function"]["name"], t["function"]["arguments"])
        answered = {m["tool_call_id"] for m in msgs if m["role"] == "tool"}
        assert set(calls) <= answered, ("unanswered tool calls", set(calls) - answered)
        assert len(answered) == len([m for m in msgs if m["role"] == "tool"]), "a call answered twice"
        for i, m in enumerate(msgs):
            if m["role"] == "tool":
                assert m["tool_call_id"] in calls and calls[m["tool_call_id"]][0] < i, "result without its call"
        for i, m in enumerate(msgs):
            if m["role"] == "user" and isinstance(m["content"], list) and any(p.get("type") == "image_url" for p in m["content"]):
                label = None
                for p in m["content"]:
                    if p.get("type") == "text" and p["text"].startswith("Images from tool call "):
                        label = p["text"].split()[4]
                    if p.get("type") == "image_url":
                        assert label in calls, ("image attributed to an unknown call", label)
                        path = json.loads(calls[label][2])["file_path"]
                        want = shots[Path(path).name]
                        assert p["image_url"]["url"] == want, ("image attributed to the wrong call", label, path)
                assert i > max(calls[l][0] for l in calls if l == label), "image before its call"


def main(argv):
    import faulthandler
    faulthandler.dump_traceback_later(900, exit=True)
    host, ox, claude = (Path(p).resolve(strict=True) for p in argv[:3])
    only = argv[3:]
    names = [n for n, _ in CASES if not only or n in only]
    results = []
    for name, case in CASES:
        if name not in names:
            continue
        with tempfile.TemporaryDirectory(prefix="ox-host-int-", dir="/tmp") as directory:
            with ExitStack() as stack:
                ctx = Ctx(Path(directory).resolve(), host, ox, claude, stack)
                try:
                    case(ctx)
                    print(f"PASS {name}")
                    results.append((name, None))
                except BaseException as error:
                    results.append((name, error))
                    print(f"FAIL {name}")
                    import traceback
                    traceback.print_exc()
                    print("HOST EXIT", ctx.host_proc.poll(), ctx.dump()[:2500], file=sys.stderr)
                    print((ctx.root / "host.log").read_text()[-1500:], file=sys.stderr)
                finally:
                    try:
                        ctx.stop_host()
                    except Exception as error:
                        print("teardown:", error, file=sys.stderr)
    print("SUMMARY " + ", ".join(f"{n}={'ok' if e is None else 'FAIL'}" for n, e in results))
    if any(e for _, e in results):
        raise SystemExit(1)



def drop(pipe):
    """Kill the desktop's socket the way a crash or suspend does: no goodbye, nothing read."""
    try:
        pipe.client.conn.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass
    pipe.client.conn.close()


def first_snapshot(pipe):
    return next(f for f in pipe.frames if f["type"] == "snapshot")


def transcript_items(frame, thread):
    items = []
    for st in frame["scopes"]:
        items += st.get("transcripts", {}).get(thread, {}).get("items", [])
    return items


def transcript_users(frame, thread):
    return [i["text"] for i in transcript_items(frame, thread) if i["role"] == "user"]


# --------------------------------------------------------------------------- normal run
def blocks(m):
    return m["content"] if isinstance(m["content"], list) else [{"type": "text", "text": m["content"]}]


def conversation(body):
    # Stock Claude can append per-submit system context after the user's message.
    # These checks follow the conversation and its paired tool results.
    return [m for m in body["messages"] if m["role"] != "system"]


def lead_view(body):
    """The lead's request messages as (role, [what it carries]) with tool results parsed."""
    out = []
    for m in conversation(body):
        row = []
        for b in blocks(m):
            if b["type"] == "text":
                text = b["text"]
                if "</system-reminder>" in text:
                    text = text.rsplit("</system-reminder>", 1)[1].strip()
                if text:
                    row.append(("text", text))
            elif b["type"] == "tool_use":
                row.append(("tool_use", b["id"], b["name"], json.dumps(b["input"], sort_keys=True)))
            elif b["type"] == "tool_result":
                c = b["content"]
                c = c if isinstance(c, str) else "".join(x.get("text", "") for x in c)
                row.append(("tool_result", b["tool_use_id"], bool(b.get("is_error")), c))
        if row:
            out.append((m["role"], row))
    return out


def worker_view(body):
    """The worker's request messages as exact (role, ...) rows; image payloads kept byte for byte."""
    out = []
    first = True
    for m in body["messages"]:
        if m["role"] == "system":
            continue
        if m["role"] == "user" and isinstance(m["content"], str):
            text = m["content"].rsplit("</system-reminder>", 1)[-1].strip() if first else m["content"]
            first = False
            out.append(("user", text))
        elif m["role"] == "user":
            out.append(("user", [p["text"] if p["type"] == "text" else p["image_url"]["url"] for p in m["content"]]))
        elif m["role"] == "assistant":
            out.append(("assistant", m.get("content") or "", [(t["id"], t["function"]["name"], t["function"]["arguments"]) for t in m.get("tool_calls") or []]))
        else:
            out.append(("tool", m["tool_call_id"], m["content"]))
    return out


def lead_tool_ids(ctx):
    names = {}
    for b in lead_requests(ctx):
        for m in b["messages"]:
            for x in blocks(m):
                if x["type"] == "tool_use":
                    names[x["id"]] = x["name"]
    return names


def start_result(ctx):
    """The parsed start_ox_worker result, from the lead's own history. No fallbacks."""
    for body in reversed(lead_requests(ctx)):
        names = {}
        for m in body["messages"]:
            for x in blocks(m):
                if x["type"] == "tool_use":
                    names[x["id"]] = x["name"]
                if x["type"] == "tool_result" and names.get(x["tool_use_id"], "").endswith("start_ox_worker"):
                    assert not x.get("is_error"), ("start_ox_worker failed", x["content"])
                    c = x["content"]
                    c = c if isinstance(c, str) else c[0]["text"]
                    result = json.loads(c)
                    assert isinstance(result.get("job_id"), int) and result["job_id"] > 0, result
                    return result
    raise AssertionError("the lead never received a start_ox_worker result")


def no_tool_errors(ctx):
    for body in lead_requests(ctx):
        for m in body["messages"]:
            for x in blocks(m):
                assert not (x["type"] == "tool_result" and x.get("is_error")), ("lead tool error", x)
    for body in ctx.fixture.of("worker"):
        for m in body["messages"]:
            if m["role"] == "tool":
                assert not m["content"].startswith(("Error", "<tool_use_error>")), ("worker tool error", m)
                assert "Read-only file system" not in m["content"] and "Exit code" not in m["content"], m


def check_end_to_end(ctx, run, interrupted_ok=False):
    """The normal run, the baseline every cut is compared with."""
    first = lead_requests(ctx)[0]
    assert any(t["name"] == "mcp__ox_workers__start_ox_worker" for t in first["tools"]), "start_ox_worker was not advertised"
    job = start_result(ctx)["job_id"]
    leads = lead_requests(ctx)
    # the second upstream request after the go prompt carries the start call and its result, paired
    after = next(i for i, b in enumerate(leads) if lead_view(b)[-1][1][-1][:1] == ("tool_result",))
    view = lead_view(leads[after])
    assert view[-2][1][0][:3] == ("tool_use", "toolu_lead_start", "mcp__ox_workers__start_ox_worker"), view
    assert view[-1][1][0][:2] == ("tool_result", "toolu_lead_start"), view
    no_tool_errors(ctx)
    status = status_result(leads[-1])
    assert status and status["job_id"] == job, status
    assert status["status"] == "completed" and status["result"] == FINAL and status["partial"] is False, status
    assert status["session"].startswith("claude:api:") and status["model"] == "fixture/demo", status
    assert status["started_mode"] == "accept_edits", status
    check_worker_wellformed(ctx)
    assert ctx.side_effects() == 1, ("the cd ran", ctx.side_effects())
    return status


def one_route(ctx):
    """Every request, before and after the cut, used the same billing route and credential."""
    assert set(ctx.fixture.authz) == {("Bearer fixture-key", "/v1/chat/completions")}, set(ctx.fixture.authz)
    assert {b["model"] for b in ctx.fixture.of("worker")} == {"demo"}
    assert not ctx.native.errors, ctx.native.errors
    assert {b["model"] for b in lead_requests(ctx)} == {lead_requests(ctx)[0]["model"]}


def case_worker_notice(ctx):
    """An idle hosted parent receives its API worker's result without polling, including with no window open."""
    ctx.fixture.parent_waits_for_notice = True
    ctx.fixture.arm("before_final")
    run = Run(ctx).boot()
    run.prep()
    before = len(run.desk.frames)
    run.send(LEAD_PROMPT, "go")
    run.wait_until(lambda: ctx.fixture.reached["before_final"].is_set(), "the worker's last reply")
    run.wait_until(lambda: any((n := change(frame, "notification")) and n["event"] == "finished"
                               and n["thread"] == run.thread for frame in run.desk.frames[before:]),
                   "the parent to finish its own turn")
    job = start_result(ctx)["job_id"]
    for connection in list(ctx.pipes):
        drop(connection)
    ctx.fixture.gates["before_final"].set()
    heading = f"OX worker #{job}: completed"
    run.wait_until(lambda: run.lead_saw(heading), "the provider to receive the worker update")
    latest = next(b for b in lead_requests(ctx)
                  if heading in str(conversation(b)[-1]["content"]))
    assert FINAL in str(conversation(latest)[-1]["content"])
    assert not any(status_result(b) for b in lead_requests(ctx))
    run.wait_until(lambda: not json.loads(ctx.ledger())["worker_notices"]["pending"], "native receipt")
    assert sum(heading in text for text in user_texts(latest)) == 1
    assert sum(heading in str(conversation(body)[-1]["content"]) for body in lead_requests(ctx)) == 1
    assert ctx.side_effects() == 1


def case_0(ctx):
    """No interruption: the baseline the cuts are compared with."""
    run = Run(ctx).boot()
    run.prep()
    run.send(LEAD_PROMPT, "go")
    run.settle()
    check_end_to_end(ctx, run)
    one_route(ctx)


def delivered_once(ctx, prompt):
    """The prompt reached the model as a fresh user message exactly once, ever."""
    last_messages = (conversation(b)[-1] for b in lead_requests(ctx))
    fresh = [m for m in last_messages if m["role"] == "user"
             and prompt in "".join(x.get("text", "") for x in blocks(m))]
    assert len(fresh) == 1, (prompt, "delivered", len(fresh))
    assert count_prompt(lead_requests(ctx)[-1], prompt) == 1, "the prompt is in the history more than once"


def case_1b(ctx):
    """The host took the send; the desktop's socket died before it saw the ACK."""
    run = Run(ctx).boot()
    run.prep()
    # Written, never read back: the desktop does not see the ACK.
    run.desk.client.act(ctx.scope, "go", {"send_text": [run.thread, LEAD_PROMPT]})
    run.wait_until(lambda: run.lead_saw(LEAD_PROMPT), "the host to forward the send to the runtime")
    drop(run.desk)
    desk = run.attach()
    snap = first_snapshot(desk)
    # What the replica reconciles by: the host's transcript shows the user message once, verbatim.
    assert transcript_users(snap, run.thread).count(LEAD_PROMPT) == 1, transcript_users(snap, run.thread)
    # A client that cannot tell whether the host took it may retry the exact action: a receipt, no delivery.
    desk.client.act(ctx.scope, "go", {"send_text": [run.thread, LEAD_PROMPT]})
    reply = desk.wait(lambda f: f["type"] in ("ack", "error") and f.get("id") == "go", "the retry's reply")
    assert reply["type"] == "ack", reply
    run.settle()
    status = check_end_to_end(ctx, run)
    delivered_once(ctx, LEAD_PROMPT)                                   # (1) one delivered user message
    assert ctx.fixture.worker_final_sent == 1 and len(ctx.fixture.of("worker")) == 4   # (2) one worker answer
    final_items = [i for i in transcript_items(desk_snapshot(ctx, run), run.thread) if i["role"] == "assistant"]
    assert [i["text"] for i in final_items].count("LEAD-SAW") == 1
    one_route(ctx)                                                      # (3) same billing route throughout
    assert ctx.side_effects() == 1                                      # (4) no tool action twice


def desk_snapshot(ctx, run):
    again = Pipe(ctx, ctx.scope, approve=False)
    again.ready()
    return first_snapshot(again)



# --------------------------------------------------------------------------- cuts
def restart(run, kill=True):
    """Stop the host (SIGKILL by default) and come back the way a user does: new host, new window."""
    ctx = run.ctx
    before = ctx.journals()
    ledger = ctx.ledger()
    ctx.stop_host(kill=kill)
    ctx.start_host()
    run.watcher = Pipe(ctx, None)
    run.watcher.wait(lambda f: f["type"] == "snapshot", "catalog snapshot")
    run.attach()
    return before, ledger


def prefix_kept(ctx, before, ledger):
    """Bytes the stock client's journal already held were not rewritten; the ledger lost nothing."""
    after = ctx.journals()
    for path, data in before.items():
        assert path in after and after[path].startswith(data), ("journal rewritten", path.name)
    if ledger:
        old, new = json.loads(ledger), json.loads(ctx.ledger())
        for scope in old["scopes"]:
            now = next((x for x in new["scopes"] if x["scope"] == scope["scope"]), None)
            assert now is not None, ("ledger lost a scope", scope["scope"])
            if any(r["id"] not in {x["id"] for x in now["sessions"]} for r in scope["sessions"]):
                print("LEDGER BEFORE", json.dumps(old)[:1500]); print("LEDGER AFTER", json.dumps(new)[:1500])
            by_id = {x["id"]: x for x in now["sessions"]}
            for row in scope["sessions"]:
                assert row["id"] in by_id, ("ledger lost a session", row["id"])
                assert row.get("origin") == by_id[row["id"]].get("origin"), "a worker stamp was rewritten"
        assert new["reserved"] >= old["reserved"], "the sequence watermark went backwards"


def send_continue(run, key="continue"):
    run.send("continue", key)


def after_continue(run, n=1, timeout=90):
    """Wait for the lead's two requests after "continue": the user turn and the status result."""
    ctx = run.ctx
    def done():
        last_messages = (conversation(b)[-1] for b in lead_requests(ctx))
        return any(m["role"] == "user" and any(
            x["type"] == "tool_result" and x["tool_use_id"] == f"toolu_lead_status_c{n}"
            for x in blocks(m)) for m in last_messages)
    run.wait_until(done, "the lead to read the worker's status after continue", timeout)
    time.sleep(1.5)
    leads = lead_requests(ctx)
    i = next(k for k, b in enumerate(leads) if lead_view(b)[-1][0] == "user" and
             any(x == ("text", "continue") for x in lead_view(b)[-1][1]) and
             sum(1 for r in lead_view(b) for x in r[1] if x == ("text", "continue")) == n)
    return leads[i], next(b for b in leads[i:] if lead_view(b)[-1][1][-1][:2] == ("tool_result", f"toolu_lead_status_c{n}"))


def check_lead_resume(ctx, before_view, resumed, final):
    """The first request after "continue" is the history the model saw, plus exactly the new message."""
    view = lead_view(resumed)
    # Messages already delivered are unchanged, in order, once: the history the model saw before the
    # cut is a prefix of the resumed request, and exactly the new message follows. Claude Code keeps
    # roles alternating, so after a dangling user turn it inserts its own empty assistant reply.
    n = len(before_view)
    assert view[:n] == before_view, ("history changed", view[:n], before_view)
    tail = view[n:]
    new = ("user", [("text", "continue")])
    gap = ("assistant", [("text", "No response requested.")])
    want = [new] if before_view[-1][0] == "assistant" else [gap, new]
    assert tail == want, ("unexpected tail", tail, want)
    assert sum(1 for r in view for x in r[1] if x == ("text", "continue")) == 1, "continue delivered more than once"
    # every tool call in the resumed request is answered, in order, exactly once
    open_calls = []
    for role, row in view:
        for x in row:
            if x[0] == "tool_use":
                open_calls.append(x[1])
            if x[0] == "tool_result":
                assert x[1] in open_calls, ("result without its call", x[1])
                open_calls.remove(x[1])
    assert not open_calls, ("unanswered tool calls", open_calls)
    status = status_result(final)
    return status


def row_of(frames, thread):
    """The newest session row for `thread` in a pipe's frames."""
    found = None
    for f in frames:
        for st in f.get("scopes", []):
            for r in st["sessions"]:
                if r["id"] == thread:
                    found = r
        for c in f.get("changes", []):
            for r in c.get("sessions", []) + ([c["session"]] if "session" in c else []):
                if r["id"] == thread:
                    found = r
    return found


def worker_scope(ctx):
    return {"project": ctx.scope["project"], "route": WORKER_ROUTE}


def resume_checks(run, ctx, pre, expect, n=1):
    send_continue(run, f"continue{n}")
    resumed, final = after_continue(run, n)
    status = check_lead_resume(ctx, pre, resumed, final)
    print(f"  lead after continue: {lead_view(resumed)[len(pre):]}")
    print(f"  parent-visible worker status: {status['status']} partial={status['partial']} result={status['result']!r}")
    assert status["job_id"] == start_result(ctx)["job_id"], status
    assert status["status"] == expect, status
    if expect == "completed":
        assert status["result"] == FINAL and status["partial"] is False
    assert ctx.side_effects() == 1, ("a tool action ran twice", ctx.side_effects())
    one_route(ctx)
    row = row_of(run.desk.frames, run.thread)
    assert row and (row["detail"].get("access") or row["detail"].get("resume_access")) == "accept_edits", ("access not preserved", row and row["detail"])
    return status


def continue_worker(run, ctx, status, pre_worker):
    """The user opens the interrupted worker and says continue; it must pick up where it was cut."""
    pipe = Pipe(ctx, worker_scope(ctx), approve=False)
    pipe.ready()
    pipe.client.act(worker_scope(ctx), "wcontinue", {"send_text": [status["session"], "continue"]})
    pipe.wait(lambda f: f["type"] in ("ack", "error") and f.get("id") == "wcontinue", "worker continue reply")
    run.wait_until(lambda: ctx.fixture.worker_done.is_set() and ctx.fixture.worker_final_sent >= 1, "the worker's final answer", 90)
    time.sleep(2)
    bodies = ctx.fixture.of("worker")
    first = next(i for i, b in enumerate(bodies) if any(m["role"] == "user" and "continue" in str(m["content"]) for m in b["messages"][-2:]))
    view = worker_view(bodies[first])
    n = len(pre_worker)
    assert view[:n] == pre_worker, ("worker history changed", view[:n], pre_worker)
    print(f"  worker after continue: {view[n:]}")
    assert sum(1 for r in view if r[0] == "user" and r[1] == "continue") == 1
    check_worker_wellformed(ctx)
    row = row_of(pipe.frames, status["session"])
    assert row and (row["detail"].get("access") or row["detail"].get("resume_access")) == "accept_edits", ("worker access not preserved", row and row["detail"])
    return view


def case_2(ctx):
    """Host SIGKILLed after the worker's model emitted a tool call, before any tool result."""
    ctx.fixture.bash_sleep = 12
    run = Run(ctx).boot()
    run.prep()
    run.send(LEAD_PROMPT, "go")
    run.wait_until(lambda: ctx.side_effects() == 1 and len(ctx.fixture.of("worker")) == 1, "the worker's cd to be running")
    time.sleep(1.5)
    pre = lead_view(lead_requests(ctx)[-1])
    pre_worker = worker_view(ctx.fixture.of("worker")[-1]) + [("assistant", "", [(f"call_Bash_{ctx.fixture.calls}", "Bash", json.dumps({"command": f"echo ran >> {ctx.root}/Alpha/cd.log && sleep 12 && cd {ctx.root}/Alpha", "description": "Enter the project"}))])]
    before, ledger = restart(run)
    prefix_kept(ctx, before, ledger)
    # A catalog refresh after a restart can emit a single row before any user
    # resumes it. The next restart must still remember the interrupted turn.
    first_recovery = json.loads(ctx.ledger())
    saved = next(row for scope in first_recovery["scopes"] for row in scope["sessions"]
                 if row["id"] == run.thread)
    assert saved["active"] and saved["access"] == "accept_edits", saved
    restart(run)
    recovered = row_of(run.desk.frames, run.thread)
    assert recovered and recovered["detail"]["owned"] and recovered["status"] == "interrupted", recovered
    status = resume_checks(run, ctx, pre, "interrupted")
    assert status["result"] is None and status["partial"] is False, status
    continue_worker(run, ctx, status, pre_worker[:1])


def case_3(ctx):
    """Host SIGKILLed between the worker's two screenshot results."""
    ctx.fixture.arm("after_shot1")
    run = Run(ctx).boot()
    run.prep()
    run.send(LEAD_PROMPT, "go")
    run.wait_until(lambda: ctx.fixture.reached["after_shot1"].is_set(), "the worker to hold after its first screenshot")
    time.sleep(1.5)  # the stock client has flushed what it had written when the host dies
    pre = lead_view(lead_requests(ctx)[-1])
    pre_worker = worker_view(ctx.fixture.of("worker")[-1])
    assert sum(1 for r in pre_worker if r[0] == "user" and isinstance(r[1], list) and any(str(x).startswith("data:image") for x in r[1])) == 1
    before, ledger = restart(run)
    prefix_kept(ctx, before, ledger)
    status = resume_checks(run, ctx, pre, "interrupted")
    continue_worker(run, ctx, status, pre_worker)


def case_4(ctx):
    """The worker finished; the parent is reaped before it saw the final answer."""
    ctx.fixture.arm("worker_done")
    run = Run(ctx).boot()
    run.prep()
    run.send(LEAD_PROMPT, "go")
    run.wait_until(lambda: ctx.fixture.reached["worker_done"].is_set(), "the lead to be about to read the final answer")
    pre = lead_view(lead_requests(ctx)[-1])
    before, ledger = ctx.journals(), ctx.ledger()
    run.desk.client.act(ctx.scope, "reap", {"force_end_session": run.thread})
    run.desk.wait(lambda f: f["type"] in ("ack", "error") and f.get("id") == "reap", "reap reply")
    ctx.fixture.gates["worker_done"].set()
    time.sleep(1.5)
    prefix_kept(ctx, before, ledger)
    resume_checks(run, ctx, pre, "completed")


def case_5(ctx):
    """After the reap, just before the first resend."""
    run = Run(ctx).boot()
    run.prep()
    run.send(LEAD_PROMPT, "go")
    run.settle()
    check_end_to_end(ctx, run)
    pre = lead_view(lead_requests(ctx)[-1]) + [("assistant", [("text", "LEAD-SAW")])]
    before, ledger = ctx.journals(), ctx.ledger()
    run.desk.client.act(ctx.scope, "end", {"end_session": run.thread})
    reply = run.desk.wait(lambda f: f["type"] in ("ack", "error") and f.get("id") == "end", "end reply")
    assert reply["type"] == "ack", reply
    time.sleep(1)
    prefix_kept(ctx, before, ledger)
    resume_checks(run, ctx, pre, "completed")


def case_1a(ctx):
    """The socket died with half a send on the wire: nothing was delivered, the retry delivers once."""
    run = Run(ctx).boot()
    run.prep()
    frame = json.dumps({"type": "act", "id": "go", "scope": ctx.scope, "action": {"send_text": [run.thread, LEAD_PROMPT]}, "approval": None})
    run.desk.client.conn.sendall(frame[: len(frame) // 2].encode())
    drop(run.desk)
    time.sleep(1)
    assert not run.lead_saw(LEAD_PROMPT), "a torn frame was delivered"
    run.attach()
    run.send(LEAD_PROMPT, "go")
    run.settle()
    check_end_to_end(ctx, run)
    delivered_once(ctx, LEAD_PROMPT)
    one_route(ctx)


def case_6(ctx):
    """A torn ledger rewrite and a torn journal append, then resume."""
    run = Run(ctx).boot()
    run.prep()
    run.send(LEAD_PROMPT, "go")
    run.settle()
    check_end_to_end(ctx, run)
    pre = lead_view(lead_requests(ctx)[-1]) + [("assistant", [("text", "LEAD-SAW")])]
    ctx.stop_host(kill=True)
    ledger_path = ctx.root / "state/ox/host/ownership-v1.json"
    good = ledger_path.read_bytes()
    # a rewrite cut after the temp file was started, before the rename
    ledger_path.with_suffix(".tmp").write_bytes(good[: len(good) // 2])
    # and the lead's journal cut mid-line, as a kill during an append leaves it
    lead_journal = next(p for p in ctx.journals() if run.thread.rsplit(":", 1)[1] in p.name)
    with lead_journal.open("ab") as f:
        f.write(b'{"type":"user","message":{"role":"us')
    before = ctx.journals()
    ctx.start_host()
    run.watcher = Pipe(ctx, None)
    run.watcher.wait(lambda f: f["type"] == "snapshot", "catalog snapshot")
    run.attach()
    assert json.loads(ledger_path.read_bytes())["scopes"], "ledger unreadable after a torn temp file"
    prefix_kept(ctx, before, good)
    resume_checks(run, ctx, pre, "completed")


def worker_timeline(pipe, session):
    """The worker's transcript as the desktop shows it, from the newest snapshot of its scope."""
    snap = next((f for f in reversed(pipe.frames) if f["type"] == "snapshot"), None)
    return next((st["transcripts"].get(session) for st in snap["scopes"] if session in st.get("transcripts", {})), None)


def stop_partial_setup(ctx, release):
    ctx.fixture.partial = True
    ctx.fixture.release_rest = release
    ctx.fixture.arm("partial")
    run = Run(ctx).boot()
    run.prep()
    run.send(LEAD_PROMPT, "go")
    run.wait_until(lambda: ctx.fixture.reached["partial"].is_set(), "the worker to stream half an answer")
    time.sleep(1.5)
    return run


def case_stop(ctx):
    """The operator stops the worker mid-answer: interrupted, the partial text kept as partial, across reopen."""
    run = stop_partial_setup(ctx, release=False)
    status0 = None
    session = None
    # the worker's session id, from the host's catalog
    pipe_all = run.watcher
    for f in pipe_all.frames:
        for st in f.get("scopes", []):
            for r in st["sessions"]:
                if r["id"].startswith("claude:api:"):
                    session = r["id"]
        for c in f.get("changes", []):
            for r in c.get("sessions", []) + ([c["session"]] if "session" in c else []):
                if r["id"].startswith("claude:api:"):
                    session = r["id"]
    assert session, "the worker never appeared in the host catalog"
    ops = Pipe(ctx, worker_scope(ctx), approve=False)
    ops.ready()
    ops.client.act(worker_scope(ctx), "stop", {"stop": session})
    reply = ops.wait(lambda f: f["type"] in ("ack", "error") and f.get("id") == "stop", "stop reply")
    assert reply["type"] == "ack", reply
    time.sleep(2)
    ctx.fixture.worker_done.set()  # the lead now reads its worker's status
    run.wait_until(lambda: any(status_result(b) for b in lead_requests(ctx)), "the lead to read the worker status", 60)
    time.sleep(1.5)
    status = status_result(lead_requests(ctx)[-1])
    print(f"  parent-visible status after Stop: {status['status']} partial={status['partial']} result={status['result']!r}")
    assert status["status"] == "interrupted" and status["partial"] is True and status["result"] == PART1, status
    again = Pipe(ctx, worker_scope(ctx), approve=False)
    again.ready()
    timeline = worker_timeline(again, session)
    assert timeline and timeline["detail"]["interrupted"] is True, (timeline and timeline["detail"])
    assert [i["text"] for i in timeline["items"] if i["role"] == "assistant"][-1] == PART1
    # persisted outcome after the host restarts and the user reopens everything
    restart(run)
    resume_checks_status = None
    send_continue(run, "continue1")
    resumed, final = after_continue(run, 1)
    status2 = status_result(final)
    print(f"  after reopen: {status2['status']} partial={status2['partial']} result={status2['result']!r}")
    assert (status2["status"], status2["partial"], status2["result"]) == ("interrupted", True, PART1), status2
    reopened = Pipe(ctx, worker_scope(ctx), approve=False)
    reopened.ready()
    ledger = json.loads(ctx.ledger())
    last = next(x["marks"].get("last") for sc in ledger["scopes"] for x in sc["sessions"] if x["id"] == session)
    assert last and last["status"] == "interrupted" and last["result"] == PART1, last
    assert ctx.side_effects() == 1


def case_observer(ctx):
    """Only an observer disconnects mid-answer: no Stop, so the worker finishes and reads completed."""
    run = stop_partial_setup(ctx, release=True)
    drop(run.desk)
    for p in list(getattr(ctx, "pipes", [])):
        if p is not run.watcher:
            drop(p)
    time.sleep(1)
    ctx.fixture.gates["partial"].set()
    run.wait_until(lambda: ctx.fixture.worker_done.is_set(), "the worker to finish after the observer left", 60)
    run.attach()
    run.wait_until(lambda: any(status_result(b) for b in lead_requests(ctx)), "the lead to read the worker status", 60)
    time.sleep(1.5)
    status = status_result(lead_requests(ctx)[-1])
    print(f"  parent-visible status after an observer left: {status['status']} partial={status['partial']} result={status['result']!r}")
    assert status["status"] == "completed" and status["partial"] is False and status["result"] == PART1 + PART2, status


CASES = [("notice", case_worker_notice), ("0", case_0), ("1a", case_1a), ("1b", case_1b), ("2", case_2), ("3", case_3), ("4", case_4), ("5", case_5), ("6", case_6), ("stop", case_stop), ("observer", case_observer)]


def main_entry():
    code = 0
    try:
        main(sys.argv[1:])
    except BaseException:
        code = 1
        import traceback
        traceback.print_exc()
    sys.stdout.flush()
    sys.stderr.flush()
    # Held fixture replies and daemon reader threads must not keep a finished run alive.
    os._exit(code)


if __name__ == "__main__":
    main_entry()
