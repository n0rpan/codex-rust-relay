"""Stock Codex must tell an MCP server which thread is calling.

The OX session host resolves a worker's parent on a shared Codex app-server
from the `_meta.threadId` of each tools/call. Codex 0.171.0 sets it in
`with_mcp_tool_call_ids_meta` (codex-rs/core/src/mcp_tool_call.rs). This
fixture checks the installed binary. Without it the host refuses every parent
call on a route with several running sessions.

Usage: python3 desktop/tests/codex_thread_meta.py /path/to/codex

One HTTP server plays both the Responses provider and a streamable-HTTP MCP
server. The provider asks the model to call mcp__capture__probe once per
thread. Every tools/call body and header set is recorded and printed.
"""
import json, os, subprocess, sys, tempfile, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

codex = sys.argv[1]
calls = []
lock = threading.Lock()


def sse(events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def body(self):
        return json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")

    def send(self, code, data, kind="application/json"):
        self.send_response(code)
        self.send_header("content-type", kind)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.send(405, b"")

    def do_POST(self):
        req = self.body()
        if self.path.startswith("/mcp"):
            method = req.get("method")
            if "id" not in req:
                return self.send(202, b"")
            if method == "initialize":
                result = {"protocolVersion": req["params"].get("protocolVersion", "2025-06-18"),
                          "capabilities": {"tools": {}}, "serverInfo": {"name": "capture", "version": "1"}}
            elif method == "tools/list":
                result = {"tools": [{"name": "probe", "description": "probe",
                                     "inputSchema": {"type": "object", "properties": {}}}]}
            elif method == "tools/call":
                with lock:
                    calls.append({"params": req.get("params"), "headers": dict(self.headers)})
                result = {"content": [{"type": "text", "text": "probed"}]}
            else:
                result = {}
            return self.send(200, json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}).encode())
        # Responses provider
        items = req.get("input", [])
        done = any(i.get("type") == "function_call_output" for i in items)
        rid = "resp_1"
        if done:
            item = {"type": "message", "role": "assistant", "id": "msg_1", "status": "completed",
                    "content": [{"type": "output_text", "text": "probe done", "annotations": []}]}
        else:
            tool = next(t for t in req.get("tools", []) if "probe" in json.dumps(t))
            name = tool.get("name") or tool.get("function", {}).get("name")
            ns = {}
            if tool.get("type") == "namespace" or tool.get("tools"):
                child = next(c for c in tool["tools"] if "probe" in c["name"])
                ns, name = {"namespace": tool["name"]}, child["name"]
            item = {"type": "function_call", "id": "fc_1", "call_id": "call_probe", "status": "completed",
                    "name": name, "arguments": "{}", **ns}
        events = [
            {"type": "response.created", "response": {"id": rid}},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": {"id": rid, "usage": {
                "input_tokens": 1, "output_tokens": 1, "total_tokens": 2,
                "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}},
        ]
        self.send(200, sse(events), "text/event-stream")


server = ThreadingHTTPServer(("127.0.0.1", 0), H)
threading.Thread(target=server.serve_forever, daemon=True).start()
port = server.server_port
home = tempfile.mkdtemp(dir=os.environ.get("TMPDIR"))
work = tempfile.mkdtemp(dir=os.environ.get("TMPDIR"))
env = dict(os.environ, CODEX_HOME=home, FX_KEY="fixture-only", NO_PROXY="127.0.0.1,localhost")
for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy"]:
    env.pop(k, None)
cfg = [
    "-c", f'model_providers.fx={{name="fx",base_url="http://127.0.0.1:{port}/v1",wire_api="responses",env_key="FX_KEY"}}',
    "-c", 'model_provider="fx"', "-c", 'model="demo"',
    "-c", f'mcp_servers.capture.url="http://127.0.0.1:{port}/mcp"',
    "-c", 'mcp_servers.capture.tools.probe.approval_mode="approve"',
]
threads = []
for n in range(2):
    out = subprocess.run([codex, *cfg, "exec", "--json", "--skip-git-repo-check",
                          "--dangerously-bypass-approvals-and-sandbox", "Call the probe tool."],
                         cwd=work, env=env, capture_output=True, text=True, timeout=120)
    for line in out.stdout.splitlines():
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if ev.get("type") == "thread.started":
            threads.append(ev.get("thread_id"))
    if out.returncode:
        print("exec failed", out.returncode, out.stderr[-2000:])
print("threads:", threads)
for c in calls:
    meta = (c["params"] or {}).get("_meta")
    print("tools/call _meta:", json.dumps(meta, sort_keys=True))
    print("  session-ish headers:", {k: v for k, v in c["headers"].items() if "session" in k.lower() or "thread" in k.lower()})
ok = len(calls) == 2 and len(set(threads)) == 2 and [
    (c["params"].get("_meta") or {}).get("threadId") for c in calls
] == threads
print("PASS: _meta.threadId names each calling thread" if ok else "FAIL: missing or wrong _meta.threadId")
sys.exit(0 if ok else 1)
