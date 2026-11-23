"""Exercise the release binary's terminal-only setup entry point with stub clients."""

import errno
import os
import pathlib
import pty
import select
import subprocess
import tempfile
import time
import tomllib

binary = pathlib.Path(os.environ.get("OX_TEST_BINARY", "target/release/ox")).resolve()


def run(args, choices, signed_in=False):
    with tempfile.TemporaryDirectory(prefix="ox-selector-") as tmp:
        home = pathlib.Path(tmp)
        bindir = home / "bin"
        bindir.mkdir()
        trace = home / "trace"
        original = 'model_provider = "openai"\n'
        (home / "config.toml").write_text(original)
        for client in ("codex", "claude"):
            fake = bindir / client
            fake.write_text('''#!/bin/sh
if [ "$3" = login ] && [ "$4" = status ]; then
    if [ "$FIXTURE_SIGNED_IN" = yes ]; then
        printf '%s\\n' 'Logged in using ChatGPT' >&2
        exit 0
    fi
    exit 1
fi
printf '%s\\n' "$@" > "$FIXTURE_TRACE"
''')
            fake.chmod(0o755)
        env = {
            "HOME": tmp, "CODEX_HOME": tmp, "CLAUDE_CONFIG_DIR": tmp,
            "PATH": f"{bindir}:/usr/bin:/bin", "FIXTURE_TRACE": str(trace),
            "FIXTURE_SIGNED_IN": "yes" if signed_in else "no", "OPENROUTER_API_KEY": "dummy-fixture-credential",
        }
        master, slave = pty.openpty()
        proc = subprocess.Popen([binary, *args], env=env, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        capture = bytearray()
        deadline = time.monotonic() + 8

        def read():
            if select.select([master], [], [], .2)[0]:
                try:
                    capture.extend(os.read(master, 8192))
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    return False
            return True

        try:
            while b"Choose 1, 2, or 3:" not in capture and time.monotonic() < deadline:
                assert read(), capture.decode()
            assert b"Choose 1, 2, or 3:" in capture, capture.decode()
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


text, argv, registry = run([], b"1\n")
assert argv == ["-c", 'model_provider="openai"', "login"], argv
assert "ox codex --chatgpt" in text and registry is None, text
print("bare ox -> ChatGPT: launched native Codex sign-in, printed the next command")
text, argv, registry = run(["setup"], b"1\n", signed_in=True)
assert argv == [] and "ox codex --chatgpt" in text and registry is None, text
print("ox setup -> already signed in to ChatGPT: no second sign-in")
text, argv, registry = run(["setup"], b"2\n")
assert argv == ["auth", "login", "--claudeai"], argv
assert "ox claude --native" in text and registry is None, text
print("ox setup -> Claude: launched native Claude Code sign-in, printed the next command")
text, argv, registry = run(
    [], b"nope\n3\nsk-or-fixture\nopenrouter\n\n\n\nOX_GATEWAY_KEY\n\nn\nfixture-model\n"
)
assert "enter 1, 2, or 3" in text
assert "looks like a credential" in text
assert "OX_GATEWAY_KEY is reserved for the shared gateway key" in text
assert "Provider name (e.g. openrouter, anthropic, openai)" in text
assert "API key variable name [OPENROUTER_API_KEY]" in text
provider = tomllib.loads(registry)["providers"]["openrouter"]
assert provider["base_url"] == "https://openrouter.ai/api/v1"
assert provider["env_key"] == "OPENROUTER_API_KEY"
assert provider["models"] == [{"id": "fixture-model"}]
assert argv == []
print("bare ox -> API provider: retry on invalid choice, clear labels, expected registry")
