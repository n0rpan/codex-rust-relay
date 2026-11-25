"""Drive Tailscale Serve recovery in a packaged Linux window with an isolated mock sudo."""
from contextlib import ExitStack
import ctypes
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

from host_fixture import session_host
from mixed_runtime_window import Window

ENABLE = ["tailscale", "serve", "--bg", "--https=8443", "http://127.0.0.1:48162"]
DISABLE = ["tailscale", "serve", "--https=8443", "off"]
ERROR = "Access denied: serve config denied. Fixture advisory text."
NODE = "workstation.fixture.ts.net"
CANARY = "nimbuspasscanary"
TERMINAL = "OX Terminal"
SUDO = """#!/usr/bin/env python3
import json, os, sys, termios
from pathlib import Path
home = Path(os.environ["HOME"])
allowed = %r
assert sys.argv[1:] in allowed and sys.stdin.isatty()
with (home / "calls").open("a") as out:
    print(json.dumps(sys.argv[1:]), file=out)
fd = sys.stdin.fileno()
original = termios.tcgetattr(fd)
hidden = termios.tcgetattr(fd)
hidden[3] &= ~termios.ECHO
termios.tcsetattr(fd, termios.TCSANOW, hidden)
try:
    print("Fixture password:", flush=True)
    password = input()
finally:
    termios.tcsetattr(fd, termios.TCSANOW, original)
if password != %r:
    sys.exit(1)
os.environ["FIXTURE_PRIVILEGED"] = "1"
program = str(home / "bin/tailscale")
os.execv(program, [program, *sys.argv[2:]])
""" % ([ENABLE, DISABLE], CANARY)

TAILSCALE = """#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
home = Path(os.environ["HOME"])
mapping = home / "mapping"
args = sys.argv[1:]
if args == ["status", "--json", "--peers=false"]:
    print(json.dumps({"BackendState": "Running", "Self": {"DNSName": %r, "UserID": 1},
                      "User": {"1": {"LoginName": "fixture@example.test"}}}))
elif args == ["serve", "status", "--json"]:
    print(mapping.read_text() if mapping.exists() else "{}")
elif args in %r:
    if not os.environ.get("FIXTURE_PRIVILEGED"):
        sys.exit(%r)
    if (home / "unconfirmed").exists():
        sys.exit(0)
    if args[-1] == "off":
        mapping.unlink()
    else:
        mapping.write_text(json.dumps({"TCP": {"8443": {"HTTPS": True}},
            "Web": {%r: {"Handlers": {"/": {"Proxy": args[-1]}}}}}))
else:
    sys.exit("Unexpected fixture arguments")
""" % (NODE, [ENABLE[1:], DISABLE[1:]], ERROR, NODE + ":8443")


def clipboard():
    # Read the X11 selection from the real GPUI window without creating a visible widget.
    os.environ["NO_AT_BRIDGE"] = "1"
    gtk = ctypes.CDLL("libgtk-3.so.0")
    gtk.gtk_init_check.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    assert gtk.gtk_init_check(None, None)
    gtk.gdk_atom_intern.argtypes = [ctypes.c_char_p, ctypes.c_int]
    gtk.gdk_atom_intern.restype = ctypes.c_void_p
    gtk.gtk_clipboard_get.argtypes = [ctypes.c_void_p]
    gtk.gtk_clipboard_get.restype = ctypes.c_void_p
    gtk.gtk_clipboard_wait_for_text.argtypes = [ctypes.c_void_p]
    gtk.gtk_clipboard_wait_for_text.restype = ctypes.c_char_p
    return gtk.gtk_clipboard_wait_for_text(gtk.gtk_clipboard_get(gtk.gdk_atom_intern(b"CLIPBOARD", 0))).decode()


def run(gui, home):
    assert Path("/usr/bin/sudo").read_bytes() == SUDO.encode()
    binary = home / "bin"
    binary.mkdir()
    (binary / "tailscale").write_text(TAILSCALE)
    (binary / "tailscale").chmod(0o700)
    (binary / "codex").write_text("#!/bin/sh\nexit 1\n")
    (binary / "codex").chmod(0o700)
    project = home / "project"
    project.mkdir()
    runtime = home / "run"
    runtime.mkdir(mode=0o700)
    env = {k: os.environ[k] for k in ["DISPLAY", "LD_LIBRARY_PATH", "XAUTHORITY", "VK_DRIVER_FILES"] if k in os.environ}
    env.update(PATH=str(binary) + ":" + os.environ["PATH"], HOME=str(home),
               XDG_STATE_HOME=str(home / "state"), XDG_CONFIG_HOME=str(home / "config"),
               XDG_RUNTIME_DIR=str(runtime), XDG_SESSION_TYPE="x11", CODEX_HOME=str(home / ".codex"),
               CLAUDE_CONFIG_DIR=str(home / ".claude"), OX_DESK_STATS_SOCKET=str(home / "stats"))
    with ExitStack() as resources:
        resources.enter_context(session_host(gui.with_name("ox-host"), home, env))
        proc = subprocess.Popen([gui, "--desk-stats", project], env=env)
        resources.callback(proc.wait, 10)
        resources.callback(proc.terminate)
        probe = resources.enter_context(socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM))
        probe.bind(str(home / "probe"))
        probe.settimeout(2)
        win = Window(home, proc, env["OX_DESK_STATS_SOCKET"], probe)

        def admin(operation):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(2)
                s.connect(str(Path(env["OX_HOST_STATE_DIR"]) / "remote-admin.sock"))
                s.sendall(json.dumps(operation).encode() + b"\n")
                with s.makefile("rb") as reader:
                    return json.loads(reader.readline())

        def status():
            return admin({"type": "status"})["Ok"]

        # Navigation and waits use the app's own state and layout from the stats
        # socket. OCR remains only where rendering itself is the point.
        def remote(state):
            return (state.get("settings") or {}).get("remote")

        def terminal(state):
            return next((w["state"] for w in state.get("windows", []) if w["title"] == TERMINAL), None)

        def until_remote(predicate, description):
            return remote(win.wait(lambda s: remote(s) is not None and predicate(remote(s)), description))

        def idle():
            return until_remote(lambda r: not r["checking"], "Remote access to finish checking")

        def act(label):
            # Wait for the operation this press starts, not a result already shown.
            before = idle()["finished"]
            win.press(label)
            return until_remote(lambda r: not r["checking"] and r["finished"] > before, f"Remote access after {label}")

        def until_clipboard(expected):
            deadline = time.monotonic() + 10
            while (copied := clipboard()) != expected and time.monotonic() < deadline:
                time.sleep(0.1)
            assert copied == expected, repr(copied)

        def review():
            before = idle()["finished"]
            win.press("Run in OX terminal")
            win.wait(lambda s: (t := terminal(s)) is not None and t["reviewing"], "the OX Terminal command review")
            return before

        def close(label, before):
            win.press(label, TERMINAL)
            win.wait(lambda s: terminal(s) is None, "the OX Terminal window to close")
            return until_remote(lambda r: not r["checking"] and r["finished"] > before, "Remote access after the terminal")

        def ended(word):
            state = win.wait(lambda s: (t := terminal(s)) is not None and t["exited"], "the terminal command to end")
            assert word in terminal(state)["status"], terminal(state)

        def submit(before, removing=False):
            win.press("Run", TERMINAL)
            # The fixture turns echo off before it prints its prompt; typing earlier would echo.
            win.wait(lambda s: (t := terminal(s)) is not None and t["running"] and t["echo"] is False,
                     "the password prompt with echo off")
            if removing:
                assert "Err" in admin({"type": "enable", "origin": "https://" + NODE + ":8443", "port": 48162})
                assert status()["enabled"]
            win.text(CANARY)
            # OCR proves what the PTY really drew: the prompt, never the typed password.
            pic = home / "typed.png"
            deadline = time.monotonic() + 30
            while True:
                time.sleep(.15)
                win.command("import", "-window", win.window(TERMINAL), str(pic))
                screen = win.command("tesseract", str(pic), "stdout", "--psm", "11").lower()
                assert "nimbus" not in screen, screen
                if "password" in screen:
                    break
                assert time.monotonic() < deadline, f"No password prompt drawn. Read {screen!r}"
            win.key("Return")
            ended("Exited")
            return close("Close", before)

        win.press("Settings")
        win.press("Remote access")
        failed = act("Enable Tailscale Serve")
        assert failed["notice"] == ERROR and failed["permission"] == "sudo " + " ".join(ENABLE), failed
        win.press("Copy error")
        until_clipboard(ERROR)
        win.press("Copy command")
        until_clipboard("sudo " + " ".join(ENABLE))
        cancelled = close("Cancel", review())
        assert "cancelled" in cancelled["notice"], cancelled
        win.word("cancelled")  # The notice is really drawn, not only held in state.
        assert not (home / "calls").exists() and not status()["enabled"]
        act("Enable Tailscale Serve")
        before = review()
        mapping = home / "mapping"
        mapping.write_text(json.dumps({"TCP": {"8443": {"TCPForward": "127.0.0.1:1111"}}}))
        win.press("Run", TERMINAL)
        ended("another")
        conflict = close("Close", before)
        assert not conflict["enabled"], conflict
        assert not (home / "calls").exists() and not status()["enabled"]
        mapping.unlink()
        act("Refresh")
        act("Enable Tailscale Serve")
        before = review()
        unconfirmed = home / "unconfirmed"
        unconfirmed.touch()
        changed = submit(before)
        assert "changed" in changed["notice"], changed
        assert not mapping.exists() and not status()["enabled"]
        unconfirmed.unlink()
        act("Enable Tailscale Serve")
        enabled = submit(review())
        assert enabled["enabled"] and not enabled["permission"], enabled
        win.control("Pair a device")
        assert mapping.exists() and status()["enabled"] and status()["origin"] == "https://" + NODE + ":8443"
        pending = act("Disable remote access")
        assert pending["permission"] == "sudo " + " ".join(DISABLE), pending
        win.press("Copy command")
        until_clipboard("sudo " + " ".join(DISABLE))
        assert status()["enabled"] and mapping.exists()
        cancelled = close("Cancel", review())
        assert "cancelled" in cancelled["notice"], cancelled
        assert status()["enabled"] and mapping.exists()
        act("Disable remote access")
        disabled = submit(review(), removing=True)
        assert not disabled["enabled"], disabled
        win.control("Enable Tailscale Serve")
        assert not mapping.exists() and not status()["enabled"]
        assert [json.loads(line) for line in (home / "calls").read_text().splitlines()] == [ENABLE, ENABLE, DISABLE]
        print("Tailscale permission recovery passed for clipboard, cancel, conflict, PTY non-echo, exact commands and verified host origin.")


if __name__ == "__main__":
    gui = Path(sys.argv[1]).resolve(strict=True)
    if len(sys.argv) == 3:
        run(gui, Path(sys.argv[2]))
    else:
        with tempfile.TemporaryDirectory(prefix="ox-permission-", dir="/tmp") as tmp:
            fake = Path(tmp) / "sudo"
            fake.write_text(SUDO)
            fake.chmod(0o700)
            subprocess.run(["bwrap", "--unshare-net", "--bind", "/", "/", "--dev-bind", "/dev", "/dev", "--ro-bind", fake, "/usr/bin/sudo", "--",
                            sys.executable, __file__, gui, tmp], check=True, timeout=180)
