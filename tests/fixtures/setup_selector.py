"""Exercise the release binary's terminal-only setup entry point with stub clients."""

import errno
import os
import pathlib
import pty
import select
import subprocess
import tempfile
import time

binary = pathlib.Path(os.environ.get("OX_TEST_BINARY", "target/release/ox")).resolve()


def run(
    args, choices=None, signed_in=False, claude_signed_in=False,
    config=None, prompt="Choose 1–5".encode(),
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
            if choices is not None:
                while prompt not in capture and time.monotonic() < deadline:
                    assert read(), capture.decode()
                assert prompt in capture, capture.decode()
                os.write(master, choices)
            while time.monotonic() < deadline and read():
                pass
            assert proc.wait(timeout=2) == 0, capture.decode()
            text = capture.decode()
            assert "Provider key:" not in text
            assert "dummy-fixture-credential" not in text
            assert (home / "config.toml").read_text() == original
            registry = (home / "codeox.toml").read_text() if (home / "codeox.toml").exists() else None
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
text, argv, registry = run(
    [], b"nope\n3\nsk-or-fixture\nopenrouter\n\n\n\nOX_GATEWAY_KEY\n\nn\nfixture-model\n4\n5\n"
)
assert "enter a number from 1 to 5" in text
assert "looks like a credential" in text
assert "OX_GATEWAY_KEY is reserved for the shared gateway key" in text
assert "Provider name (e.g. openrouter, anthropic, openai)" in text
assert "API key variable name [OPENROUTER_API_KEY]" in text
assert registry is not None
for expected in (
    "[providers.openrouter]",
    'base_url = "https://openrouter.ai/api/v1"',
    'env_key = "OPENROUTER_API_KEY"',
    "[[providers.openrouter.models]]",
    'id = "fixture-model"',
):
    assert expected in registry.splitlines(), registry
assert argv == []
assert "[search]" in registry and 'env_key = "BRAVE_SEARCH_API_KEY"' in registry
assert "Load the Brave Search API key into $BRAVE_SEARCH_API_KEY" in text
assert "Run `ox doctor` to check setup" in text
print("bare ox: add an API provider, enable Brave, then finish the setup wizard")


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
text, argv, registry = run(["setup"], b"4\n4\n5\n", config=previous)
assert registry == previous.replace("false # manual opt-out", "true # manual opt-out")
assert "Load the Brave Search API key into $CUSTOM_BRAVE_KEY" in text and argv == []
print("setup: re-enable an existing Brave configuration without rewriting other settings")


text, argv, registry = run(
    ["claude"], b":native\n", config=shared,
    claude_signed_in=True, prompt=b"Model number or search",
)
assert "Type :native to use your Claude subscription" in text
assert argv == ["claude"] and registry == shared
print("shared model picker: native subscription remains available alongside API models")
