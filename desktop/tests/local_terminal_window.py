"""Exercise a user-operated terminal from the desktop palette in an isolated project."""
from contextlib import ExitStack
from pathlib import Path
import os
import socket
import subprocess
import sys
import tempfile
import time

from host_fixture import session_host
from mixed_runtime_window import Window


def run(gui, root):
    project = root / "terminal project"
    project.mkdir()
    home = root / "home"
    home.mkdir()
    runtime = root / "runtime"
    runtime.mkdir(mode=0o700)
    env = {key: os.environ[key] for key in ("PATH", "DISPLAY", "XAUTHORITY", "LD_LIBRARY_PATH", "VK_DRIVER_FILES") if key in os.environ}
    env.update(HOME=str(home), XDG_STATE_HOME=str(root / "state"), XDG_CONFIG_HOME=str(root / "config"),
               XDG_RUNTIME_DIR=str(runtime), XDG_SESSION_TYPE="x11", CODEX_HOME=str(home / ".codex"),
               CLAUDE_CONFIG_DIR=str(home / ".claude"), SHELL="/bin/sh", OX_DESK_STATS_SOCKET=str(root / "stats"))
    with ExitStack() as resources:
        resources.enter_context(session_host(gui.with_name("ox-host"), root, env))
        app = subprocess.Popen([gui, "--desk-stats", project], env=env)
        resources.callback(app.wait, 10)
        resources.callback(app.terminate)
        probe = resources.enter_context(socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM))
        probe.bind(str(root / "probe"))
        probe.settimeout(2)
        window = Window(root, app, env["OX_DESK_STATS_SOCKET"], probe)
        terminal = lambda s: next((w["state"] for w in s.get("windows", []) if w["title"] == "OX Terminal"), None)
        window.key("ctrl+k")
        window.text("Open project terminal")
        window.word("terminal")  # The palette has filtered to the rendered command.
        window.key("Return")
        window.wait(lambda s: (t := terminal(s)) is not None and t["reviewing"], "the terminal review")
        window.control("Open shell", "OX Terminal")
        window.command("xdotool", "windowfocus", window.window("OX Terminal"))
        # The review's primary action is its first keyboard tab stop.
        window.key("Tab")
        window.key("Return")
        # The PTY grid takes keyboard focus from the review.
        window.wait(lambda s: (t := terminal(s)) is not None and t["running"], "the interactive shell")
        # A direct file operation proves that the PTY owns a real shell in the chosen project.
        window.text("test -t 0 && test -t 1 && pwd -P > ox-terminal-working-directory")
        window.key("Return")
        witness = project / "ox-terminal-working-directory"
        until = time.monotonic() + 8
        while not witness.exists() and time.monotonic() < until:
            time.sleep(.05)
        assert witness.read_text().strip() == str(project.resolve())
        window.text("exit 7")
        window.key("Return")
        state = window.wait(lambda s: (t := terminal(s)) is not None and t["exited"], "the shell to exit")
        assert terminal(state)["status"] == "Exited with status 7", terminal(state)
        window.press("Close", "OX Terminal")
        window.wait(lambda s: terminal(s) is None, "the terminal window to close")
        assert app.poll() is None
        print("Local GPUI terminal: palette, explicit open, interactive PTY, project cwd, exit status and close passed.")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="ox-tty-", dir="/tmp") as tmp:
        run(Path(sys.argv[1]).resolve(strict=True), Path(tmp))
