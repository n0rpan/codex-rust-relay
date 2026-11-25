"""Kill the host during a stock Codex turn, then continue on the recovered thread."""
from contextlib import closing, ExitStack
from http.server import ThreadingHTTPServer
import os
from pathlib import Path
import shutil
import sys
import tempfile
import threading

from host_fixture import descendants, session_host, wait_gone
from host_interruptions import row_of
from host_stock_session import Client, change
from stock_session import Fixture


def ready(client, scope):
    client.open(scope)
    state = client.until(lambda f: f["type"] == "snapshot")["scopes"][0]
    if state["connection"] != "ready":
        client.until(lambda f: (s := change(f, "state")) and s["connection"] == "ready")


def run(root, host, helper, codex):
    first, second = "OX interrupted turn", "OX explicit continuation"
    prompts = []
    reached, release = threading.Event(), threading.Event()

    def on_turn(body):
        assert body["model"] == "demo", body["model"]
        history = [part["text"] for msg in body["input"] if msg.get("role") == "user"
                   for part in msg["content"] if part.get("text") in (first, second)]
        prompts.append(history)
        if history and history[-1] == first:
            reached.set()
            release.wait(30)

    with ExitStack() as stack:
        provider = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), Fixture))
        provider.mode, provider.on_turn = "host-cut", on_turn
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        stack.callback(provider.shutdown)
        stack.callback(release.set)
        for name in ("bin", "home", "codex", "project"):
            (root / name).mkdir()
        (root / "bin/codex").symlink_to(codex)
        if node := shutil.which("node"):  # npm may install its launcher outside the system PATH.
            (root / "bin/node").symlink_to(node)
        for name, source in (("ox-host", host), ("ox", helper)):
            shutil.copy2(source, root / "bin" / name)
        address = f"http://127.0.0.1:{provider.server_port}"
        (root / "codex/codeox.toml").write_text(
            f'[providers.fixture]\nname="Fixture"\nbase_url="{address}/v1"\n'
            'wire_api="responses"\nenv_key="OX_FIXTURE_KEY"\ndiscovery=false\n'
            '[[providers.fixture.models]]\nid="demo"\ncontext_window=131072\n')
        (root / "codex/config.toml").write_text(f'chatgpt_base_url="{address}/backend-api"\n')
        env = {"PATH": str(root / "bin") + os.pathsep + os.defpath,
               "HOME": str(root / "home"), "CODEX_HOME": str(root / "codex"),
               "XDG_STATE_HOME": str(root / "state"), "OX_FIXTURE_KEY": "fixture-only",
               "NO_PROXY": "127.0.0.1,localhost", "LANG": "C", "SHELL": "/bin/sh"}
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            env[key] = env[key.lower()] = "http://127.0.0.1:1"
        scope = {"project": str(root / "project"),
                 "route": {"runtime": "codex", "billing": {"api": "fixture/demo"}}}
        binary = root / "bin/ox-host"
        with session_host(binary, root, env) as proc, closing(Client(env["OX_HOST_SOCKET"])) as client:
            ready(client, scope)
            client.act(scope, "original", {"send_text": [None, first]})
            thread = client.until(lambda f: f["type"] == "opened")["thread"]
            assert reached.wait(20), "Codex never sent the first turn to the provider"
            client.open(scope)
            client.until(lambda f: (row := row_of([f], thread)) and row["status"] == "running")
            children = descendants(proc.pid)
            assert children, "Expected the host to own an active stock process"
            proc.kill()
            proc.wait(timeout=5)
            assert not (alive := wait_gone(children)), f"Host children survived SIGKILL: {alive}"
            release.set()

        with session_host(binary, root, env), closing(Client(env["OX_HOST_SOCKET"])) as client:
            ready(client, scope)
            # Take a fresh snapshot after the scope is ready, including any catalog refresh.
            client.open(scope)
            client.until(lambda f: (row := row_of([f], thread)) and
                         row["detail"]["owned"] and row["status"] == "interrupted")
            assert prompts == [[first]], prompts
            client.act(scope, "continue", {"send_text": [thread, second]})
            client.until(lambda f: (n := change(f, "notification")) and n["event"] == "finished")
            assert prompts == [[first], [first, second]], prompts
    print("Stock Codex recovered its interrupted thread and delivered only the explicit follow-up.")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="ox-codex-cut-", dir="/tmp") as tmp:
        run(Path(tmp).resolve(), *(Path(arg).resolve(strict=True) for arg in sys.argv[1:]))
