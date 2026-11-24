"""Run inside a 1280x900 Xvfb display. Arguments are the packaged GUI and stock Codex."""
import json
import os
from pathlib import Path
from host_fixture import session_host
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from http.server import ThreadingHTTPServer

from stock_boundaries import CatalogClient
from stock_session import Fixture


def run(gui, stock, root):
    home = root / "home"
    state = root / "state"
    alpha, beta = root / "Alpha", root / "Beta"
    codex = home / ".codex"
    for directory in [codex, state, alpha, beta, root / "bin"]:
        directory.mkdir(mode=0o700, parents=True)
    (root / "bin/codex").symlink_to(stock)
    if node := shutil.which("node"):
        (root / "bin/node").symlink_to(node)
    terminal = root / "bin/xdg-terminal-exec"
    terminal.write_text("#!/usr/bin/python3\nimport json, os, sys\nfrom pathlib import Path\n"
                        "Path(os.environ['HOME'], 'terminal.json').write_text(json.dumps(sys.argv[1:]))\n")
    terminal.chmod(0o700)
    child = {
        "PATH": f"{root}/bin:/usr/bin:/bin", "HOME": str(home), "CODEX_HOME": str(codex),
        "XDG_RUNTIME_DIR": str(home), "XDG_STATE_HOME": str(state), "XDG_SESSION_TYPE": "x11",
        "DISPLAY": os.environ["DISPLAY"], "OX_FIXTURE_KEY": "fixture-only",
        "ALL_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1",
        "HTTP_PROXY": "http://127.0.0.1:1", "NO_PROXY": "127.0.0.1,localhost",
    }
    for k in ["XAUTHORITY", "VK_DRIVER_FILES", "LD_LIBRARY_PATH"]:
        if k in os.environ:
            child[k] = os.environ[k]
    layout = state / "ox/desktop/workspace.json"
    layout.parent.mkdir(parents=True)
    layout.write_text(json.dumps({"project": str(alpha), "recent": [
        {"project": str(p), "api_model": "fixture/demo"} for p in (alpha, beta)
    ]}))
    theme = state / "omarchy/current/theme/colors.toml"
    theme.parent.mkdir(parents=True)
    theme.write_text("background='#1a1b26'\nforeground='#c0caf5'\naccent='#7aa2f7'\n")

    def command(*args, timeout=12):
        return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL, timeout=timeout)

    with ThreadingHTTPServer(("127.0.0.1", 0), Fixture) as http:
        http.mode = "parallel"
        http.steered = threading.Event()
        http.followup_requested = threading.Event()
        threading.Thread(target=http.serve_forever, daemon=True).start()
        (codex / "codeox.toml").write_text(
            '[providers.fixture]\nname="Fixture"\n'
            f'base_url="http://127.0.0.1:{http.server_port}/v1"\n'
            'env_key="OX_FIXTURE_KEY"\nwire_api="chat_completions"\ndiscovery=false\n'
            '[[providers.fixture.models]]\nid="demo"\nname="Meadow"\n'
            'context_window=131072\nreasoning_efforts=["low","high"]\n'
            '[[providers.fixture.models]]\nid="alternate"\nname="Orchard"\ncontext_window=131072\n'
        )
        child["OX_DESK_STATS_SOCKET"] = str(root / "stats")
        with (session_host(gui.with_name("ox-host"),root,child), subprocess.Popen([gui, "--desk-stats", alpha], env=child) as app,
              socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as probe):
            probe.bind(str(root / "probe"))
            probe.settimeout(2)
            try:
                window = command("xdotool", "search", "--sync", "--onlyvisible", "--pid",
                                 str(app.pid), "--name", "OX Desktop").splitlines()[-1]
                command("xdotool", "windowfocus", window)

                def word(wanted, *, sidebar=False, between_rows=None):
                    end = time.monotonic() + 14
                    shot = str(root / "window.png")
                    while time.monotonic() < end:
                        command("import", "-window", window, shot)
                        for scale in (1, 3):
                            source = shot
                            if scale > 1:
                                source = str(root / "scaled.png")
                                command(shutil.which("magick") or "convert", shot, "-resize", f"{100 * scale}%",
                                        "-colorspace", "gray", "-negate", source)
                            data = command("tesseract", source, "stdout", "--psm", "11", "tsv")
                            for line in data.splitlines()[1:]:
                                fields = line.split("\t")
                                if len(fields) < 12:
                                    continue
                                value = fields[-1].strip(" .↓↑▾‘’\'\"“”()").lower()
                                if value == wanted or (wanted.startswith("/") and wanted in value):
                                    center = ((int(fields[6]) + int(fields[8]) // 2) // scale,
                                              (int(fields[7]) + int(fields[9]) // 2) // scale)
                                    if sidebar and center[0] >= 245:
                                        continue
                                    if between_rows and not (between_rows[0] < center[1] < between_rows[1]):
                                        continue
                                    return center
                        time.sleep(0.15)
                    visible = command("tesseract",shot,"stdout","--psm","11")
                    raise AssertionError(f"Missing {wanted!r} in the visible window: {visible}")

                def click(x, y):
                    command("xdotool", "mousemove", "--window", window, str(x), str(y), "click", "1")

                def wait_view(predicate):
                    end = time.monotonic() + 12
                    while time.monotonic() < end:
                        probe.sendto(b"snapshot", child["OX_DESK_STATS_SOCKET"])
                        view = json.loads(probe.recv(16384))
                        if predicate(view):
                            return view
                        time.sleep(.06)
                    raise AssertionError(f"Timed out waiting for thread state: {view}")

                def enter(prompt):
                    command("xdotool", "key", "ctrl+l")
                    command("xdotool", "type", prompt)
                    command("xdotool", "key", "ctrl+Return")

                def send(prompt):
                    wait_view(lambda state: state["connected"])
                    enter(prompt)
                    word("once")

                send("Alpha approval")
                click(*word("alpha", sidebar=True))
                click(*word("/beta"))
                send("Beta approval")
                for p in (alpha, beta):
                    assert not (p / "approved.txt").exists()
                click(*word("waiting"))
                click(*word("once"))
                word("complete")
                assert (alpha / "approved.txt").read_text().strip() == "accepted"
                assert not (beta / "approved.txt").exists()

                # Leave Beta's command pending while starting and steering a child in Alpha.
                enter("/new")
                wait_view(lambda s: s["selected_thread"] is None)
                # The parent title contains the same word as the composer placeholder.
                send("Ask the desktop fixture worker to check the project")
                parent = wait_view(lambda s: s["selected_thread"] and s["visible_turn"])["selected_thread"]
                with CatalogClient(gui.with_name("ox"), alpha, child, api=True) as observer:
                    deadline = time.monotonic() + 10
                    while time.monotonic() < deadline:
                        descendants = observer.rpc("thread/list", {
                            "ancestorThreadId": parent, "sourceKinds": ["subAgentThreadSpawn"],
                            "modelProviders": ["codeox"], "limit": 10,
                        })["data"]
                        if descendants:
                            break
                        time.sleep(.08)
                    assert descendants, "The live parent did not create a stock child"
                    child_title = descendants[0].get("name") or descendants[0]["agentNickname"]
                click(*word(child_title.split()[0].lower(), sidebar=True))
                worker = wait_view(lambda s: s["selected_parent"] and s["selected_requests"]
                                   and s["visible_turn"])["selected_thread"]
                enter("Apply the window clarification.")
                click(*word("waiting"))
                word("once")
                command("xdotool", "key", "ctrl+alt+1")
                word("complete")
                assert (beta / "approved.txt").read_text().strip() == "accepted"
                click(*word("waiting"))
                wait_view(lambda s: s["selected_thread"] == worker and s["selected_requests"])
                click(*word("once"))
                assert http.steered.wait(10), "Stock worker did not receive the UI clarification"
                wait_view(lambda s: s["selected_thread"] == worker and not s["visible_turn"])
                enter("Confirm the worker follow-up.")
                assert http.followup_requested.wait(10), "Stock worker did not receive the follow-up from its composer"
                wait_view(lambda s: s["selected_thread"] == worker and s["selected_requests"] and s["visible_turn"])
                click(*word("stop"))
                wait_view(lambda s: s["selected_thread"] == worker and not s["visible_turn"])
                click(*word("once"))  # Alpha parent's remaining approval
                click(*word("alpha", sidebar=True))
                click(*word("/beta"))
                beta_thread = wait_view(lambda s: s["selected_thread"] and not s["visible_turn"])["selected_thread"]
                _, controls = word("speed")

                def chip(label):
                    return word(label, between_rows=(controls - 18, controls + 18))

                click(*chip("fixture/demo"))
                click(*word("orchard"))
                click(*chip("fixture/alternate"))
                click(*word("meadow"))
                click(*chip("effort"))
                click(*word("high"))
                chip("high")
                wait_view(lambda s: s["selected_thread"] == beta_thread)
                enter("/speed default")
                word("default")  # Codex acknowledged the requested speed.
                enter("/future-fixture-command")
                word("paste")
                argv = json.loads((home / "terminal.json").read_text())
                assert str(beta) in argv and argv[-1] == beta_thread == str(uuid.UUID(argv[-1])), argv
                assert argv[-3:-1] == ["resume", "--"] and argv[argv.index("--model")+1] == "fixture/demo", argv
                click(*word("here"))
                word("complete")
                original = argv[-1]
                click(*word("actions"))
                click(*word("fork"))
                fork = wait_view(lambda s: s["selected_thread"] and s["selected_thread"] != original)["selected_thread"]
                word("complete")
                click(*word("actions"))
                click(*word("rename"))
                word("cancel")
                command("xdotool", "key", "ctrl+a")
                command("xdotool", "type", "Cherry fixture")
                command("xdotool", "key", "Return")
                with CatalogClient(gui.with_name("ox"), beta, child, api=True) as observer:
                    deadline = time.monotonic() + 8
                    while time.monotonic() < deadline:
                        result = observer.rpc("thread/read", {"threadId": fork, "includeTurns": False})
                        if result["thread"]["name"] == "Cherry fixture":
                            break
                        time.sleep(.06)
                    else:
                        raise AssertionError(f"Rename did not reach stock Codex: {result['thread']['name']!r}")
                click(*word("actions"))
                click(*word("archive"))
                click(*word("cancel"))
                click(*word("actions"))
                click(*word("archive"))
                # The confirmation precedes Cancel in keyboard focus order.
                x, y = word("cancel")
                click(x - 110, y)
                wait_view(lambda s: s["selected_thread"] is None)
                print("Forked, renamed and archived a copy of the API thread.")
                print("Switched between two models on Beta, selected high effort, and kept its original thread.")
                print("Steered and stopped an Alpha subagent across a switch to approve Beta’s command. The workbench applied /speed and handed the same API thread to the terminal.")
            finally:
                app.terminate()
                app.wait(timeout=5)

        fresh = root / "fresh"
        fresh.mkdir(mode=0o700)
        fresh_env = {**child, "HOME": str(fresh), "CODEX_HOME": str(fresh / ".codex"),
                     "XDG_STATE_HOME": str(fresh / "state")}
        with session_host(gui.with_name("ox-host"),root,fresh_env), subprocess.Popen([gui, alpha], env=fresh_env) as app:
            try:
                window = command("xdotool", "search", "--sync", "--onlyvisible", "--pid",
                                 str(app.pid), "--name", "OX Desktop").splitlines()[-1]
                command("xdotool", "windowfocus", window)
                heading = word("connect")
                configure = word("configure")
                # The primary sign-in button sits between the last line of the
                # account description and the secondary provider button. Its
                # tinted label is unreadable to OCR on some Ubuntu fonts.
                description = word("subscription", between_rows=(heading[1], configure[1]))
                click(configure[0], (description[1] + configure[1]) // 2)
                terminal_file = fresh / "terminal.json"
                end = time.monotonic() + 5
                while not terminal_file.exists() and time.monotonic() < end:
                    time.sleep(.05)
                assert json.loads(terminal_file.read_text())[-2:] == ["login", "chatgpt"]
                click(*word("configure"))
                word("custom")
                print("A fresh profile reached native sign-in and the provider settings form.")
            finally:
                app.terminate()
                app.wait(timeout=5)


if __name__ == "__main__":
    if len(sys.argv) != 3 or not all(shutil.which(t) for t in ["xdotool", "import", "tesseract"]):
        sys.exit("Usage under Xvfb with xdotool, ImageMagick and Tesseract: concurrent_window.py GUI STOCK_CODEX")
    with tempfile.TemporaryDirectory(prefix="ox-projects-") as tmp:
        run(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve(), Path(tmp))
