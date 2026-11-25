"""What stock Claude Code tells an MCP server about the calling session.

The OX worker gateway learns a new `ox claude` launch's session only from
`--session-id`, which OX adds when the user passed no session flag. Claude Code
2.1.332 sends no session identity on MCP requests: `tools/call` carries only
`_meta.claudecode/toolUseId` and `progressToken`, and no session header.

Usage: python3 desktop/tests/claude_mcp_meta.py /path/to/claude

One HTTP server plays both the Messages API and a streamable-HTTP MCP server.
The provider asks the model to call mcp__capture__probe once. The fixture
fails when `--session-id` is not honoured or the tool call never arrives. It
passes either way on the session identity and prints a NOTE when Claude Code
starts naming the session, so OX can use it instead of `--session-id`.
"""
import json, os, subprocess, sys, tempfile, threading, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

claude = sys.argv[1]
calls = []
lock = threading.Lock()


def sse(events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()


def message(blocks, stop):
    start = {"id": "msg_1", "type": "message", "role": "assistant", "model": "fixture", "content": [],
             "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 1, "output_tokens": 1}}
    return sse([
        {"type": "message_start", "message": start},
        {"type": "content_block_start", "index": 0, "content_block": blocks[0]},
        {"type": "content_block_delta", "index": 0, "delta": blocks[1]},
        {"type": "content_block_stop", "index": 0},
        {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": 1}},
        {"type": "message_stop"},
    ])


TEXT = message([{"type": "text", "text": ""}, {"type": "text_delta", "text": "probe done"}], "end_turn")
TOOL = message([{"type": "tool_use", "id": "toolu_probe", "name": "mcp__capture__probe", "input": {}},
                {"type": "input_json_delta", "partial_json": "{}"}], "tool_use")


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, code, data, kind="application/json"):
        self.send_response(code)
        self.send_header("content-type", kind)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.send(405, b"")

    def do_DELETE(self):
        self.send(200, b"")

    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        if self.path.startswith("/mcp"):
            if "id" not in req:
                return self.send(202, b"")
            method = req.get("method")
            if method == "initialize":
                result = {"protocolVersion": req["params"].get("protocolVersion", "2025-06-18"),
                          "capabilities": {"tools": {}}, "serverInfo": {"name": "capture", "version": "1"}}
            elif method == "tools/list":
                result = {"tools": [{"name": "probe", "description": "probe",
                                     "inputSchema": {"type": "object", "properties": {}}}]}
            elif method == "tools/call":
                with lock:
                    calls.append({"params": req.get("params") or {}, "headers": dict(self.headers)})
                result = {"content": [{"type": "text", "text": "probed"}]}
            else:
                result = {}
            return self.send(200, json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}).encode())
        if "count_tokens" in self.path:
            return self.send(200, b'{"input_tokens":1}')
        # Messages API
        tools = [t.get("name") for t in req.get("tools", [])]
        last = req.get("messages", [{}])[-1].get("content")
        done = isinstance(last, list) and any(b.get("type") == "tool_result" for b in last)
        self.send(200, TOOL if "mcp__capture__probe" in tools and not done else TEXT, "text/event-stream")


server = ThreadingHTTPServer(("127.0.0.1", 0), H)
threading.Thread(target=server.serve_forever, daemon=True).start()
port = server.server_port
home = tempfile.mkdtemp(dir=os.environ.get("TMPDIR"))
work = tempfile.mkdtemp(dir=os.environ.get("TMPDIR"))
mcp = os.path.join(home, "mcp.json")
with open(mcp, "w") as f:
    json.dump({"mcpServers": {"capture": {"type": "http", "url": f"http://127.0.0.1:{port}/mcp"}}}, f)
env = dict(os.environ, HOME=home, CLAUDE_CONFIG_DIR=os.path.join(home, "profile"),
           ANTHROPIC_BASE_URL=f"http://127.0.0.1:{port}", ANTHROPIC_API_KEY="fixture-only",
           ENABLE_TOOL_SEARCH="false", DISABLE_TELEMETRY="1", CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1",
           NO_PROXY="127.0.0.1,localhost")
for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "CLAUDECODE",
          "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"]:
    env.pop(k, None)
requested = str(uuid.uuid4())
out = subprocess.run([claude, "-p", "--session-id", requested, "--mcp-config", mcp, "--strict-mcp-config",
                      "--allowedTools", "mcp__capture__probe", "--output-format", "json", "Call the probe tool."],
                     cwd=work, env=env, capture_output=True, text=True, timeout=180)
try:
    session = json.loads(out.stdout).get("session_id")
except ValueError:
    session = None
    print("claude failed", out.returncode, out.stderr[-2000:], out.stdout[-2000:])
print("requested session:", requested, "reported:", session)
named = False
for c in calls:
    meta = c["params"].get("_meta") or {}
    headers = {k: v for k, v in c["headers"].items() if "session" in k.lower() or "claude" in k.lower()}
    print("tools/call _meta:", json.dumps(meta, sort_keys=True))
    print("  session-ish headers:", headers)
    named |= requested in json.dumps(meta) or requested in json.dumps(headers)
ok = len(calls) == 1 and session == requested
if named:
    print("NOTE: Claude Code now names the session on MCP calls; OX could read it instead of adding --session-id")
print("PASS: --session-id is honoured and the MCP call arrives" if ok else "FAIL: --session-id or the MCP call")
sys.exit(0 if ok else 1)
