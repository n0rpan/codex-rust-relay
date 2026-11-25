"""Discover conversations created outside Desktop and resume one in its original project."""
import argparse
from contextlib import ExitStack
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading

from host_fixture import session_host
from claude_stock_session import setup
from mixed_runtime_window import Window
from stock_session import Fixture


def seed(command, project, env):
    return subprocess.run(command, cwd=project, env=env, stdin=subprocess.DEVNULL,
                          capture_output=True, text=True, check=True, timeout=45).stdout


def run(gui, codex, claude, root, failure_image=None):
    with ExitStack() as stack:
        vendor, env = setup(root, claude, stack)
        responses = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), Fixture))
        responses.mode = "native"
        threading.Thread(target=responses.serve_forever, daemon=True).start()
        stack.callback(responses.shutdown)
        beta = root / "Beta"
        (root / "bin/codex").symlink_to(codex)
        if node := shutil.which("node"):
            (root / "bin/node").symlink_to(node)

        # Plain stock CLIs save these sessions in their normal (fixture-only) profiles.
        config = Path(env["CODEX_HOME"]) / "config.toml"
        config.write_text('model="demo"\nmodel_provider="seed"\n[model_providers.seed]\n'
                          f'name="Stock fixture"\nbase_url="http://127.0.0.1:{responses.server_port}/v1"\n'
                          'env_key="OPENAI_API_KEY"\nwire_api="responses"\nsupports_websockets=false\n')
        seed([codex, "--no-daemon", "exec", "--skip-git-repo-check", "Larch catalog history"],
             beta, dict(env, OPENAI_API_KEY="fixture-only"))
        native_env = dict(env, ANTHROPIC_API_KEY=vendor.expected_auth,
                          ANTHROPIC_BASE_URL=f"http://127.0.0.1:{vendor.server_port}")
        seed([claude, "-p", "--name", "Willow history", "--model", "demo", "Review the garden"], beta, native_env)

        # A third session has a billable OX alias. Select and run it from a fresh desktop layout.
        encoded = seed([gui.with_name("ox"), "claude", "--model", "fixture/demo", "-p",
                        "--output-format", "json", "Birch relay review"], beta, env)
        thread = "claude:api:" + json.loads(encoded)["session_id"]
        alpha = root / "Alpha"
        state = root / "state"
        recent = {"project": str(alpha), "runtime": "claude", "api_model": "fixture/demo"}
        workspace = state / "ox/desktop/workspace.json"
        workspace.parent.mkdir(parents=True)
        workspace.write_text(json.dumps({"project": str(alpha), "recent": [recent]}))
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        env.update(DISPLAY=os.environ["DISPLAY"], XDG_SESSION_TYPE="x11", LANG="C.UTF-8",
                   XDG_STATE_HOME=str(state), XDG_RUNTIME_DIR=str(runtime), OX_DESK_STATS_SOCKET=str(root / "stats"))
        for key in ("XAUTHORITY", "VK_DRIVER_FILES", "LD_LIBRARY_PATH"):
            if key in os.environ:
                env[key] = os.environ[key]
        stack.enter_context(session_host(gui.with_name("ox-host"),root,env))
        with subprocess.Popen([gui, "--desk-stats", alpha], env=env) as app, socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as probe:
            window = None
            try:
                probe.bind(str(root / "probe"))
                probe.settimeout(1)
                window = Window(root, app, env["OX_DESK_STATS_SOCKET"], probe)
                window.wait(lambda s: s["connected"], "active project")
                window.word("Larch")
                window.word("Willow")
                # Import another terminal session created while Desktop is already running.
                seed([claude, "-p", "--name", "Maple history", "--model", "demo", "Summarize the orchard"],
                     beta, native_env)
                calls_before_refresh = len(vendor.requests)
                window.choose("Refresh sessions")
                window.word("Maple")
                window.word("Birch", click=True)
                window.wait(lambda s: s["connected"] and s["selected_thread"] == thread, "imported relay session")
                window.word("Default", after_x=220, bottom=True)
                assert len(vendor.requests) == calls_before_refresh, "Refreshing or resuming generated a model request"
                window.press("Thread actions")  # An icon button: no visible word to OCR.
                window.word("Generate", click=True)
                window.word("Orchard")
                assert len(vendor.requests) == calls_before_refresh + 1, "Expected one title request"
                window.prompt("APPROVAL from the imported relay conversation")
                window.wait(lambda s: s["selected_requests"], "imported session approval")
                window.key("ctrl+alt+1")
                window.wait(lambda s: not s["visible_turn"] and (beta / "approved.txt").exists(), "write in the original project")
                assert not (alpha / "approved.txt").exists()
                window.key("ctrl+n")
                window.wait(lambda s: s["selected_thread"] is None, "new Claude draft")
                window.prompt("Plan the spring planting")
                window.word("Sapling")
                print("Native histories, refresh, manual and automatic titles, and original-project resume passed.")
            except Exception:
                if failure_image and window and app.poll() is None:
                    subprocess.run(["import", "-window", window.id, str(failure_image)], timeout=5, check=False)
                raise
            finally:
                app.terminate()
                app.wait(timeout=8)


if __name__ == "__main__":
    cli = argparse.ArgumentParser(description=__doc__)
    for name in ("gui", "codex", "claude"):
        cli.add_argument(name, type=Path)
    cli.add_argument("--failure-image", type=Path)
    args = cli.parse_args()
    with tempfile.TemporaryDirectory(prefix="ox-catalog-") as scratch:
        run(*(p.resolve(strict=True) for p in (args.gui, args.codex, args.claude)), Path(scratch), args.failure_image)
