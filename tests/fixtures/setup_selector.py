"""Exercise the release binary's terminal-only setup entry point with stub clients."""

import errno
import http.server
import json
import os
import pathlib
import pty
import select
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request

binary = pathlib.Path(os.environ.get("OX_TEST_BINARY", "target/release/ox")).resolve()


def run(
    args, choices=None, signed_in=False, claude_signed_in=False,
    config=None, prompt="Choose 1–5".encode(), saved=None, model=None,
):
    with tempfile.TemporaryDirectory(prefix="ox-selector-") as tmp:
        home = pathlib.Path(tmp)
        bindir = home / "bin"
        bindir.mkdir()
        trace = home / "trace"
        original = 'model_provider = "openai"\n'
        (home / "config.toml").write_text(original)
        if config is not None:
            (home / "codeox.toml").write_text(config)
        for client in ("codex", "claude"):
            fake = bindir / client
            fake.write_text('''#!/bin/sh
if [ "$1" = debug ] && [ "$2" = models ]; then
    printf '{"models":[]}'
    exit 0
fi
if [ "$1" = auth ] && [ "$2" = status ]; then
    if [ "$FIXTURE_CLAUDE_SIGNED_IN" = yes ]; then
        printf '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}'
        exit 0
    fi
    exit 1
fi
if [ "$3" = login ] && [ "$4" = status ]; then
    if [ "$FIXTURE_SIGNED_IN" = yes ]; then
        printf '%s\\n' 'Logged in using ChatGPT' >&2
        exit 0
    fi
    exit 1
fi
if env | grep -q '^CUSTOM_.*_API_KEY='; then
    exit 9
fi
printf '%s\\n' "$(basename "$0")" "$@" > "$FIXTURE_TRACE"
''')
            fake.chmod(0o755)
        env = {
            "HOME": tmp, "CODEX_HOME": tmp, "CLAUDE_CONFIG_DIR": tmp,
            "PATH": f"{bindir}:/usr/bin:/bin", "FIXTURE_TRACE": str(trace),
            "FIXTURE_SIGNED_IN": "yes" if signed_in else "no",
            "FIXTURE_CLAUDE_SIGNED_IN": "yes" if claude_signed_in else "no",
            "OPENROUTER_API_KEY": "dummy-fixture-credential",
            "ANTHROPIC_API_KEY": "dummy-anthropic-credential",
            "ALIBABA_API_KEY": "dummy-alibaba-credential",
        }
        master, slave = pty.openpty()
        proc = subprocess.Popen([binary, *args], env=env, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        capture = bytearray()
        deadline = time.monotonic() + 8

        def read():
            if select.select([master], [], [], .2)[0]:
                try:
                    chunk = os.read(master, 8192)
                    if not chunk:
                        return False
                    capture.extend(chunk)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    return False
            return True

        try:
            steps = [(prompt, choices)] if isinstance(choices, bytes) else choices or []
            previous = 0
            for marker, answer in steps:
                while marker not in capture[previous:] and time.monotonic() < deadline:
                    assert read(), f"ox exited before prompt {marker!r}"
                assert marker in capture[previous:], f"Missing prompt {marker!r}"
                previous = len(capture)
                os.write(master, answer)
            while time.monotonic() < deadline and read():
                pass
            assert proc.wait(timeout=2) == 0, capture.decode()
            text = capture.decode()
            assert "Provider key:" not in text
            assert "dummy-fixture-credential" not in text
            assert (home / "config.toml").read_text() == original
            registry = (home / "codeox.toml").read_text() if (home / "codeox.toml").exists() else None
            if saved:
                secret_file = home / "ox-api-keys.json"
                assert secret_file.stat().st_mode & 0o777 == 0o600
                keys = json.loads(secret_file.read_text())
                for name, (scope, key) in saved.items():
                    assert keys.get(name) == {"scope": scope, "value": key}, name
                    assert key not in text and key not in registry, "Key exposed during setup"
                doctor = subprocess.run(
                    [binary, "doctor"], env=env, capture_output=True, text=True, timeout=5,
                )
                assert doctor.returncode == 0, doctor.stdout
                for name in saved:
                    assert f"{name} is available to ox (value hidden)" in doctor.stdout
            if model:
                for client in ("codex", "claude"):
                    launched = subprocess.run(
                        [binary, client, "--model", model], env=env,
                        capture_output=True, text=True, timeout=5,
                    )
                    assert launched.returncode == 0, launched.stderr
                    argv = trace.read_text().splitlines()
                    assert argv[0] == client and argv[-2:] == ["--model", model], argv
                with socket.socket() as sock:
                    sock.bind(("127.0.0.1", 0))
                    port = sock.getsockname()[1]
                gateway_key = "fixture-gateway-key-not-an-upstream-key"
                with subprocess.Popen(
                    [binary, "serve", "--port", str(port)],
                    env={**env, "OX_GATEWAY_KEY": gateway_key},
                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
                ) as daemon:
                    try:
                        assert select.select([daemon.stderr], [], [], 5)[0], "ox serve timed out"
                        assert daemon.stderr.readline().startswith("ox: serving on ")
                        req = urllib.request.Request(
                            f"http://127.0.0.1:{port}/v1/models",
                            headers={"Authorization": f"Bearer {gateway_key}"},
                        )
                        http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                        with http.open(req, timeout=5) as response:
                            assert model in {item["id"] for item in json.load(response)["data"]}
                    finally:
                        daemon.terminate()
                        daemon.wait(timeout=5)
                (home / "codeox.toml").write_text(registry.replace("/v1", "/different/v1"))
                doctor = subprocess.run(
                    [binary, "doctor"], env=env, capture_output=True, text=True, timeout=5,
                )
                assert doctor.returncode != 0 and "is not set" in doctor.stdout
            return text, trace.read_text().splitlines() if trace.exists() else [], registry
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            os.close(master)


text, argv, registry = run([], b"1\n5\n")
assert argv == ["codex", "-c", 'model_provider="openai"', "login"], argv
assert "ox codex --chatgpt" in text and registry is None, text
print("bare ox -> ChatGPT: launched native Codex sign-in, printed the next command")
text, argv, registry = run(["setup"], b"1\n5\n", signed_in=True)
assert argv == [] and "ox codex --chatgpt" in text and registry is None, text
print("ox setup -> already signed in to ChatGPT: no second sign-in")
text, argv, registry = run(["setup"], b"2\n5\n")
assert argv == ["claude", "auth", "login", "--claudeai"], argv
assert "ox claude --native" in text and registry is None, text
print("ox setup -> Claude: launched native Claude Code sign-in, printed the next command")
menu = "Choose 1–5".encode()
hidden = b"API key (hidden): "
go_key = "dummy-go-pasted-key"
brave_key = "dummy-brave-pasted-key"
text, argv, registry = run(
    [], [
        (menu, b"nope\n3\n"),
        (b"Service number:", b"1\n"),
        (hidden, f"{go_key}\n".encode()),
        (menu, b"4\n"),
        (hidden, f"{brave_key}\n".encode()),
        (menu, b"5\n"),
    ],
    saved={
        "OPENCODE_API_KEY": ("https://opencode.ai/inference/go/openai/v1", go_key),
        "BRAVE_SEARCH_API_KEY": ("https://api.search.brave.com/", brave_key),
    },
)
assert "enter a number from 1 to 5" in text
assert registry is not None
for expected in (
    "[providers.opencode-go]",
    'base_url = "https://opencode.ai/inference/go/openai/v1"',
    'models_url = "https://opencode.ai/inference/go/v1/models"',
    'wire_api = "chat_completions"',
    'env_key = "OPENCODE_API_KEY"',
    "[search]",
    'env_key = "BRAVE_SEARCH_API_KEY"',
):
    assert expected in registry.splitlines(), registry
assert argv == []
assert "Run `ox doctor` to check setup" in text
print("bare ox: guided Go and Brave setup with hidden keys usable after restart")


class Catalog(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        ok = self.path == "/v1/models" and self.headers.get("Authorization") == "Bearer fixture-custom-key"
        self.server.calls.append(ok)
        self.send_response(200 if ok else 401)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"data":[{"id":"fixture"}]}')

    def log_message(self, *args):
        pass


with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Catalog) as vendor:
    vendor.calls = []
    thread = threading.Thread(target=vendor.serve_forever)
    thread.start()
    port = vendor.server_port
    base = f"http://127.0.0.1:{port}/v1"
    try:
        text, argv, registry = run(
            ["provider", "add"], [
                (b"Service number:", b"6\n"),
                (b"Paste the API base URL", f"{base}\n".encode()),
                (hidden, b"fixture-custom-key\n"),
            ],
            saved={f"CUSTOM_127_0_0_1_{port}_API_KEY": (base, "fixture-custom-key")},
            model=f"custom-127-0-0-1-{port}/fixture",
        )
        assert "Using OpenAI Responses" in text
        assert vendor.calls == [True, True, True], vendor.calls
    finally:
        vendor.shutdown()
        thread.join()
print("Other: plain /v1 defaults to Responses and both launchers and ox serve discover the model")


for endpoint, wire in (
    ("https://chat.example/v1/chat/completions", "chat_completions"),
    ("https://messages.example/v1/messages", "anthropic_messages"),
):
    text, argv, registry = run(
        ["provider", "add"], [
            (b"Service number:", b"6\n"),
            (b"Paste the API base URL", f"{endpoint}\n".encode()),
            (hidden, b"\n"),
        ],
    )
    assert f'wire_api = "{wire}"' in registry
    assert f'base_url = "{endpoint.split("/v1")[0]}/v1"' in registry
    assert "needs a key" in text
print("Other: full Chat Completions and Messages endpoints select their own format")


text, argv, registry = run([
    "provider", "add", "openrouter",
    "--base-url", "https://openrouter.ai/api/v1",
    "--wire", "responses",
    "--no-discovery", "--model", "fixture-model",
])
assert "Display name" not in text
assert "API key variable name" not in text
assert registry is not None
assert 'env_key = "OPENROUTER_API_KEY"' in registry.splitlines()
assert argv == []
print("flag-driven ox provider add in a terminal: applied defaults without prompting")


for args, sign_in, expected in (
    (["codex"], {"signed_in": True}, ["codex", "-c", 'model_provider="openai"']),
    (["claude"], {"claude_signed_in": True}, ["claude"]),
    ([], {"claude_signed_in": True}, ["claude"]),
):
    text, argv, registry = run(args, **sign_in)
    assert argv == expected and registry is None, (text, argv)
    assert "What would you like to use?" not in text
print("plain commands: launch signed-in stock clients without reopening setup")

text, argv, registry = run(["setup"], b"1\n2\n5\n", signed_in=True, claude_signed_in=True)
assert "already signed in with Claude" in text and "already signed in with ChatGPT" in text
assert argv == [] and registry is None
print("setup: checks both existing native sign-ins in one pass")

text, argv, registry = run(["doctor"], signed_in=True, claude_signed_in=True)
assert "API gateway providers: 0" in text and "signed in with ChatGPT" in text and "signed in with Claude" in text
assert argv == [] and registry is None
print("doctor: zero API providers and two working native sign-ins")

shared = """
[providers.anthropic]
base_url = "https://api.anthropic.com/v1"
wire_api = "anthropic_messages"
env_key = "ANTHROPIC_API_KEY"
discovery = false

[[providers.anthropic.models]]
id = "claude-fixture"

[providers.alibaba]
base_url = "https://alibaba.example/v1"
wire_api = "chat_completions"
env_key = "ALIBABA_API_KEY"
discovery = false

[[providers.alibaba.models]]
id = "qwen-fixture"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
env_key = "OPENROUTER_API_KEY"
discovery = false

""" + "\n".join(f'[[providers.openrouter.models]]\nid = "model-{i}"\n' for i in range(14))

for client, search, expected in (
    ("codex", "anthropic", "anthropic/claude-fixture"),
    ("claude", "alibaba", "alibaba/qwen-fixture"),
):
    text, argv, registry = run(
        [client], f"{search}\n1\n".encode(), config=shared,
        signed_in=True, claude_signed_in=True, prompt=b"Model number or search",
    )
    assert registry == shared and argv[0] == client and argv[-2:] == ["--model", expected], argv
    assert "Providers: alibaba, anthropic, openrouter" in text
    assert "[alibaba]" in text and "[anthropic]" in text
    assert "dummy-anthropic-credential" not in text and "dummy-alibaba-credential" not in text
print("shared model picker: both clients show API providers and launch the selected alias")

text, argv, registry = run(["claude"], b":n\n:q\n", config=shared, prompt=b"Model number or search")
assert "Showing 13–16 of 16" in text and argv == [] and registry == shared
print("shared model picker: page through a large catalog and cancel without launching")


previous = shared + '\n[search]\nenabled = false # manual opt-out\nenv_key = "CUSTOM_BRAVE_KEY"\nmax_results = 3\n'
text, argv, registry = run(["setup"], [(menu, b"4\n"), (hidden, b"\n"), (menu, b"4\n"), (hidden, b"\n"), (menu, b"5\n")], config=previous)
assert registry == previous.replace("false # manual opt-out", "true # manual opt-out")
assert "Brave search needs a key" in text and argv == []
print("setup: re-enable an existing Brave configuration without rewriting other settings")


for client, plan, expected in (
    ("codex", "ChatGPT", ["codex", "-c", 'model_provider="openai"']),
    ("claude", "Claude", ["claude"]),
):
    text, argv, registry = run(
        [client], b"0\n", config=shared,
        signed_in=True, claude_signed_in=True, prompt=b"Model number or search",
    )
    assert f"0. {plan} subscription  [native]" in text
    assert "anthropic/claude-fixture" in text
    assert argv == expected and registry == shared, argv
print("shared model picker: option 0 launches the matching subscription alongside API choices")
