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
        main = win.id
        def admin(operation):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(2)
                s.connect(str(Path(env["OX_HOST_STATE_DIR"]) / "remote-admin.sock"))
                s.sendall(json.dumps(operation).encode() + b"\n")
                with s.makefile("rb") as reader:
                    return json.loads(reader.readline())

        def status():
            return admin({"type": "status"})["Ok"]

        def click(word):
            win.word(word, click=True, bottom=True)

        def review():
            click("Run")
            win.id = win.command("xdotool", "search", "--sync", "--onlyvisible", "--pid", str(proc.pid),
                                 "--name", "^OX Terminal$").splitlines()[-1]
            win.command("xdotool", "windowfocus", win.id)
            win.word("Cancel")

        def confirm():
            # Some fonts render the short primary label "Run" poorly in OCR.
            # Locate the neighboring Cancel label and click the first button on that row.
            shot = home / "review.png"
            win.command("import", "-window", win.id, str(shot))
            rows = win.command("tesseract", str(shot), "stdout", "--psm", "11", "tsv")
            cancel = next(row.split("\t") for row in rows.splitlines() if row.endswith("\tCancel"))
            y = int(cancel[7]) + int(cancel[9]) // 2
            win.command("xdotool", "mousemove", "--window", win.id, "36", str(y), "click", "1")

        def close(word):
            click(word)
            win.id = main
            win.command("xdotool", "windowfocus", main)

        def submit(removing=False):
            confirm()
            win.word("password")
            if removing:
                assert "Err" in admin({"type": "enable", "origin": "https://" + NODE + ":8443", "port": 48162})
                assert status()["enabled"]
            win.text(CANARY)
            time.sleep(.15)
            pic = home / "typed.png"
            win.command("import", "-window", win.id, str(pic))
            screen = win.command("tesseract", str(pic), "stdout", "--psm", "11").lower()
            assert "password" in screen and "nimbus" not in screen, screen
            win.key("Return")
            win.word("Exited")
            close("Close")

        click("Settings")
        click("Remote")
        click("Enable")
        win.word("Copy", same_row="error", click=True)
        copied = clipboard()
        assert copied == ERROR, repr(copied)
        click("command")
        assert clipboard() == "sudo " + " ".join(ENABLE)
        review()
        close("Cancel")
        win.word("cancelled")
        assert not (home / "calls").exists() and not status()["enabled"]
        click("Enable")
        review()
        mapping = home / "mapping"
        mapping.write_text(json.dumps({"TCP": {"8443": {"TCPForward": "127.0.0.1:1111"}}}))
        confirm()
        win.word("another")
        close("Close")
        assert not (home / "calls").exists() and not status()["enabled"]
        mapping.unlink()
        click("Refresh")
        click("Enable")
        review()
        unconfirmed = home / "unconfirmed"
        unconfirmed.touch()
        submit()
        win.word("changed")
        assert not mapping.exists() and not status()["enabled"]
        unconfirmed.unlink()
        click("Enable")
        review()
        submit()
        win.word("Pair")
        assert mapping.exists() and status()["enabled"] and status()["origin"] == "https://" + NODE + ":8443"
        click("Disable")
        click("command")
        assert clipboard() == "sudo " + " ".join(DISABLE)
        assert status()["enabled"] and mapping.exists()
        review()
        close("Cancel")
        win.word("cancelled")
        assert status()["enabled"] and mapping.exists()
        click("Disable")
        review()
        submit(removing=True)
        win.word("Enable")
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
