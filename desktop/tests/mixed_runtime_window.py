"""Exercise the production project picker, composer and approval sheet with both stock clients on Xvfb."""
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
import time

from host_fixture import session_host
from claude_stock_session import setup
from stock_session import Fixture


class CodexProvider(Fixture):
    def do_POST(self):
        super().do_POST()
        if self.path == "/v1/chat/completions":
            self.server.replied.set()


class Window:
    def __init__(self, root, app, stats, probe):
        self.root, self.app, self.stats, self.probe = root, app, stats, probe
        self.id = self.command(
            "xdotool", "search", "--sync", "--onlyvisible", "--pid", str(app.pid), "--name", "OX Desktop"
        ).splitlines()[-1]
        self.command("xdotool", "windowfocus", self.id)

    @staticmethod
    def command(*args):
        return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL, timeout=20)

    def snapshot(self):
        self.probe.sendto(b"snapshot", self.stats)
        return json.loads(self.probe.recv(65536))

    def wait(self, predicate, description):
        deadline = time.monotonic() + 25
        state = {}
        while time.monotonic() < deadline:
            if self.app.poll() is not None:
                raise AssertionError(f"OX Desktop exited during {description}")
            try:
                state = self.snapshot()
            except (FileNotFoundError, socket.timeout):
                continue
            if predicate(state):
                return state
            time.sleep(0.08)
        raise AssertionError(f"Timed out waiting for {description}. Window state {state}")

    def key(self, chord):
        self.command("xdotool", "key", "--clearmodifiers", chord)

    def text(self, text):
        self.command("xdotool", "type", "--clearmodifiers", "--delay", "5", text)

    def prompt(self, text):
        self.key("ctrl+l")
        self.text(text)
        self.key("Return")

    def choose(self, query):
        self.key("ctrl+k")
        self.text(query)
        self.key("Return")

    def word(self, word, *, click=False, after_x=0, bottom=False, same_row=None):
        """Locate a rendered label, allowing for scale and font differences."""
        deadline = time.monotonic() + 10
        shot = self.root / "window.png"
        visible = ""
        while time.monotonic() < deadline:
            self.command("import", "-window", self.id, str(shot))
            for scale in (1, 2):
                page = shot
                if scale == 2:
                    page = self.root / "large.png"
                    self.command(shutil.which("magick") or "convert", str(shot), "-resize", "200%", "-colorspace", "gray", "-negate", str(page))
                data = self.command("tesseract", str(page), "stdout", "--psm", "11", "tsv")
                matches = []
                neighbors = []
                visible = []
                for row in data.splitlines()[1:]:
                    cell = row.split("\t")
                    if len(cell) < 12 or not cell[11].strip():
                        continue
                    visible.append(cell[11])
                    label = cell[11].strip(" .,:·↓↑▾‘’'\"“”()[]").casefold()
                    x = (int(cell[6]) + int(cell[8]) // 2) // scale
                    y = (int(cell[7]) + int(cell[9]) // 2) // scale
                    if label == word.casefold() and x >= after_x:
                        matches.append((x, y))
                    if same_row and label == same_row.casefold():
                        neighbors.append((x, y, int(cell[9]) / scale))
                if same_row:
                    matches = [(x, y) for x, y in matches if any(
                        nx > x and abs(y - ny) <= height / 2 for nx, ny, height in neighbors
                    )]
                if matches:
                    x, y = max(matches, key=lambda xy: xy[1]) if bottom else matches[0]
                    if click:
                        self.command("xdotool", "mousemove", "--window", self.id, str(x), str(y), "click", "1")
                    return
            time.sleep(.1)
        raise AssertionError(f"Missing visible {word!r}. Read {visible}")


def run(gui, codex, claude, root, failure_image=None):
    with ExitStack() as stack:
        vendor, env = setup(root, claude, stack)
        codex_vendor = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), CodexProvider))
        codex_vendor.mode = "parallel"
        codex_vendor.replied = threading.Event()
        threading.Thread(target=codex_vendor.serve_forever, daemon=True).start()
        stack.callback(codex_vendor.shutdown)
        project = root / "Alpha"
        (root / "bin/codex").symlink_to(codex)
        if node := shutil.which("node"):
            (root / "bin/node").symlink_to(node)
        with (root / "codex/codeox.toml").open("a") as registry:
            registry.write(
                '\n[providers.copper]\nname="Copper"\nwire_api="chat_completions"\n'
                f'base_url="http://127.0.0.1:{codex_vendor.server_port}/v1"\n'
                'discovery=false\nenv_key="OX_FIXTURE_KEY"\n'
                '[[providers.copper.models]]\nid="demo"\ncontext_window=131072\n'
            )
        state, runtime = root / "state", root / "runtime"
        runtime.mkdir(mode=0o700)
        workspace = state / "ox/desktop/workspace.json"
        workspace.parent.mkdir(parents=True)
        workspace.write_text(json.dumps({"project": str(project), "recent": [
            {"project": str(project), "api_model": "copper/demo"},
        ]}))
        env.update({
            "DISPLAY": os.environ["DISPLAY"], "XDG_SESSION_TYPE": "x11", "XDG_STATE_HOME": str(state),
            "XDG_RUNTIME_DIR": str(runtime), "OX_DESK_STATS_SOCKET": str(root / "stats"),
            "OX_FIXTURE_KEY": "fixture-only", "LANG": "C.UTF-8",
        })
        for name in ("XAUTHORITY", "VK_DRIVER_FILES", "LD_LIBRARY_PATH"):
            if name in os.environ:
                env[name] = os.environ[name]
        stack.enter_context(session_host(gui.with_name("ox-host"),root,env))
        with subprocess.Popen([gui, "--desk-stats", project], env=env) as app, socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as probe:
            w = None
            try:
                probe.bind(str(root / "probe"))
                probe.settimeout(1)
                w = Window(root, app, env["OX_DESK_STATS_SOCKET"], probe)
                w.wait(lambda s: s["connected"], "Codex connection")
                w.word("copper/demo", after_x=200)
                w.prompt("Copper meadow model-switch-window")
                copper = w.wait(lambda s: codex_vendor.replied.is_set() and s["selected_thread"] and not s["visible_turn"], "stock Codex reply")["selected_thread"]
                w.choose("Claude Code fixture/demo billing route")
                w.wait(lambda s: s["pane"] != 0 and s["connected"], "Claude connection")
                w.word("Claude", after_x=200)
                w.word("fixture/demo", after_x=200)
                w.prompt("APPROVAL Cedar window")
                cedar = w.wait(lambda s: s["selected_requests"] and s["visible_turn"], "Claude permission request")["selected_thread"]
                assert cedar and cedar != copper
                w.prompt("Cedar clarification while waiting")
                w.choose("Copper meadow")
                w.wait(lambda s: s["selected_thread"] == copper, "original Codex thread")
                marker = project / "approved.txt"
                assert not marker.exists(), "A backgrounded approval executed before user input"
                # The background button reads “1 waiting · Alpha”. A still-painted
                # Claude approval or transcript can also contain the word “waiting”.
                w.word("waiting", click=True, same_row=project.name)
                w.wait(lambda s: s["selected_thread"] == cedar and s["selected_requests"], "Claude background approval")
                w.key("ctrl+alt+1")
                w.wait(lambda s: s["selected_thread"] == cedar and not s["visible_turn"] and marker.exists(), "approved Claude tool")
                assert marker.read_text().strip() == "accepted"
                assert any("Cedar clarification while waiting" in json.dumps(r["messages"]) for r in vendor.requests)
                assert not vendor.errors, vendor.errors
                print("One project retained its Codex and Claude sessions. Enter, live Claude steering and background approval reached stock.", flush=True)
                w.word("Default", click=True, bottom=True)
                w.word("YOLO", click=True, bottom=True)
                w.word("YOLO", after_x=200, bottom=True)
                marker.rename(project / "approved-manually.txt")
                w.prompt("APPROVAL Verify confirmed mode")
                w.wait(lambda s: not s["visible_turn"] and marker.exists(), "stock Claude bypassPermissions execution")
                assert marker.read_text().strip() == "accepted"
                print("The confirmed Claude YOLO mode executed a second local tool without another approval.", flush=True)
            except Exception:
                if failure_image and w and app.poll() is None:
                    subprocess.run(["import", "-window", w.id, str(failure_image)], timeout=5, check=False)
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
    with tempfile.TemporaryDirectory(prefix="ox-mixed-window-") as scratch:
        run(*(p.resolve(strict=True) for p in (args.gui, args.codex, args.claude)), Path(scratch).resolve(), args.failure_image)
