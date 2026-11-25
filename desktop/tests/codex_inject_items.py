"""Stock Codex must accept handoff history through thread/inject_items.

A cross-runtime handoff into Codex starts a thread, injects the selected earlier
messages as raw Responses message items with their user and assistant roles,
then starts the turn with the new request. Codex 0.171.0 and 0.172.0 accept
the items on an empty thread without the experimental API capability, place
them after the environment context and before the new request, and keep them
in the rollout so they are still model-visible after thread/resume. They are
not listed by thread/read, so OX renders the handoff marker itself.

Usage: python3 desktop/tests/codex_inject_items.py /path/to/codex

A local HTTP server plays the Responses provider and records each request's
input. The fixture fails if any of the behaviours above changes.
"""
import json, os, signal, subprocess, sys, tempfile, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

codex = sys.argv[1]
signal.alarm(180)
requests = []


def sse(events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        requests.append(json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}"))
        n = len(requests)
        item = {"type": "message", "role": "assistant", "id": f"msg_{n}", "status": "completed",
                "content": [{"type": "output_text", "text": f"reply {n}", "annotations": []}]}
        body = sse([
            {"type": "response.created", "response": {"id": f"resp_{n}"}},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": {"id": f"resp_{n}", "usage": {
                "input_tokens": 1, "output_tokens": 1, "total_tokens": 2,
                "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}},
        ])
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


server = ThreadingHTTPServer(("127.0.0.1", 0), H)
threading.Thread(target=server.serve_forever, daemon=True).start()
home = tempfile.mkdtemp(dir=os.environ.get("TMPDIR"))
work = tempfile.mkdtemp(dir=os.environ.get("TMPDIR"))
env = dict(os.environ, CODEX_HOME=home, FX_KEY="fixture-only", NO_PROXY="127.0.0.1,localhost")
for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy"]:
    env.pop(k, None)
cfg = [
    "-c", f'model_providers.fx={{name="fx",base_url="http://127.0.0.1:{server.server_port}/v1",wire_api="responses",env_key="FX_KEY"}}',
    "-c", 'model_provider="fx"', "-c", 'model="demo"',
]


class AppServer:
    def __init__(self):
        self.p = subprocess.Popen([codex, *cfg, "app-server", "--listen", "stdio://"], cwd=work, env=env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.n = 0
        self.call("initialize", {"clientInfo": {"name": "ox-fixture", "version": "1"}, "capabilities": {}})
        self.send({"method": "initialized"})

    def send(self, message):
        self.p.stdin.write(json.dumps(message) + "\n")
        self.p.stdin.flush()

    def read(self):
        line = self.p.stdout.readline()
        if not line:
            raise SystemExit("FAIL: app-server exited")
        return json.loads(line)

    def call(self, method, params):
        self.n += 1
        self.send({"id": self.n, "method": method, "params": params})
        while True:
            message = self.read()
            if message.get("id") == self.n and "method" not in message:
                if "error" in message:
                    raise SystemExit(f"FAIL: {method}: {message['error']}")
                return message["result"]

    def turn(self, thread, text):
        self.call("turn/start", {"threadId": thread, "input": [{"type": "text", "text": text}]})
        while self.read().get("method") != "turn/completed":
            pass

    def close(self):
        self.p.kill()
        self.p.wait()


def conversation(request):
    """User and assistant message texts in model order, without Codex's own context."""
    out = []
    for item in request.get("input", []):
        if item.get("type") == "message" and item.get("role") in ("user", "assistant"):
            text = " ".join(c.get("text", "") for c in item.get("content", []))
            if not text.startswith("<"):
                out.append((item["role"], text))
    return out


history = [
    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "HANDOFF original request"}]},
    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "HANDOFF earlier answer"}]},
]
carried = [("user", "HANDOFF original request"), ("assistant", "HANDOFF earlier answer")]

s = AppServer()
thread = s.call("thread/start", {"cwd": work})["thread"]["id"]
s.call("thread/inject_items", {"threadId": thread, "items": history})
s.turn(thread, "NEW request")
first = conversation(requests[-1])
listed = json.dumps(s.call("thread/read", {"threadId": thread, "includeTurns": True}))
s.close()

s = AppServer()
s.call("thread/resume", {"threadId": thread, "cwd": work})
s.turn(thread, "AFTER resume")
resumed = conversation(requests[-1])
s.close()

print("first turn:", first)
print("after resume:", resumed)
checks = {
    "injected items precede the new request": first == carried + [("user", "NEW request")],
    "injected items survive thread/resume": resumed[:3] == carried + [("user", "NEW request")]
    and resumed[-1] == ("user", "AFTER resume"),
    "thread/read does not list injected items": "HANDOFF" not in listed,
}
for name, ok in checks.items():
    print("PASS:" if ok else "FAIL:", name)
sys.exit(0 if all(checks.values()) else 1)
