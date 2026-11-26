"""Upgrade the session host in the middle of a stock Claude Code turn.

With the real host, OX helper and installed Claude Code against a local
Messages API, the fixture:

- starts a turn whose model reply is held open by the local provider;
- runs `ox-host restart` while the turn is running;
- checks that the draining host refuses a new turn from a second client
  ("phone") with the update reason, but keeps the running turn;
- releases the reply and checks the turn finishes on the old host;
- checks the restart command starts a new host (new boot epoch), the phone
  reconnects to it, the session is idle rather than interrupted, and a new
  turn in the same session runs on the new host;
- holds a second turn past `--timeout 2` and checks that restart reports one
  interrupted turn and the new host shows the session as interrupted.

Usage: python3 desktop/tests/host_upgrade_session.py /path/to/ox-host /path/to/ox /path/to/claude
"""
from contextlib import ExitStack
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from claude_stock_session import setup
from host_fixture import session_host
from host_stock_session import Client, change, ask


def hello(path):
    client = Client.__new__(Client)
    client.conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.conn.settimeout(25)
    client.conn.connect(path)
    client.input = client.conn.makefile("rb")
    client.send(type="hello", version=1)
    frame = client.until(lambda f: f["type"] == "hello")
    return client, frame


def reserve_idle(client):
    client.send(type="reserve_idle_update")
    return client.until(lambda frame: frame["type"] == "idle_update")["ready"]


def finish(client, scope):
    def step(frame):
        if pending := ask(frame):
            client.act(scope, uuid.uuid4().hex, {"answer_ask": [pending["id"], 0]}, pending["token"])
        return (n := change(frame, "notification")) and n["event"] == "finished"
    client.until(step)


def ready(client, scope):
    client.open(scope)
    state = client.until(lambda f: f["type"] == "snapshot")["scopes"][0]
    if "start" not in state["capabilities"]:
        client.until(lambda f: (c := change(f, "state")) and "start" in c["capabilities"])
    return state


def run(root, host, ox, claude):
    with ExitStack() as stack:
        vendor, env = setup(root, claude, stack)
        env = dict(env, ENABLE_TOOL_SEARCH="false", XDG_STATE_HOME=str(root / "state"))
        for source, target in ((host, root / "bin" / "ox-host"), (ox, root / "bin" / "ox")):
            try:
                os.link(source, target)
            except OSError:
                shutil.copy2(source, target)
        started, release = threading.Event(), threading.Event()
        stuck, held = threading.Event(), threading.Event()

        def script(body, prompt):
            if prompt.startswith("LONG-TURN"):
                started.set()
                release.wait(120)
                return [{"type": "text", "text": "LONG-TURN finished on the old host"}]
            if prompt.startswith("STUCK-TURN"):
                # Its own event: `release` is already set by the first phase.
                stuck.set()
                held.wait(120)
                return [{"type": "text", "text": "never shown"}]
            if prompt.startswith("AFTER-UPGRADE"):
                return [{"type": "text", "text": "AFTER-UPGRADE answered by the new host"}]
            return [{"type": "text", "text": "ok"}]

        vendor.script = script
        log_path = root / "host.log"
        log = stack.enter_context(log_path.open("w+"))
        scope = {"project": str(root / "Beta"), "route": {"runtime": "claude", "billing": {"api": "fixture/demo"}}}
        restart = None
        try:
            with session_host(root / "bin" / "ox-host", root, env, log) as old:
                socket_path = env["OX_HOST_SOCKET"]
                desktop, first = hello(socket_path)
                epoch = first["host"]["epoch"]
                ready(desktop, scope)
                desktop.act(scope, "warm", {"send_text": [None, "WARMUP first turn"]})
                thread = desktop.until(lambda f: f["type"] == "opened")["thread"]
                finish(desktop, scope)
                phone, _ = hello(socket_path)
                phone.open(scope)
                phone.until(lambda f: f["type"] == "snapshot")

                reservation, caps = hello(socket_path)
                assert "idle_update" in caps["capabilities"]
                assert reserve_idle(reservation)
                assert not reserve_idle(phone), "only one updater can reserve the host"
                phone.act(scope, "still-readable", {"select_thread": thread})
                assert phone.until(lambda f: f.get("id") == "still-readable")["type"] == "ack"
                phone.act(scope, "idle-race", {"send_text": [thread, "must not start"]})
                refused = phone.until(lambda f: f.get("id") == "idle-race")
                assert refused["type"] == "error" and "restarting" in refused["message"], refused
                reservation.close()
                # A disconnect and a send from another connection can reach the mailbox in either order.
                until = time.monotonic() + 5
                while True:
                    key = uuid.uuid4().hex
                    desktop.act(scope, key, {"send_text": [thread, "LONG-TURN keep working"]})
                    reply = desktop.until(lambda f: f.get("id") == key and f["type"] in ("ack", "error"))
                    if reply["type"] == "ack":
                        break
                    assert "restarting" in reply["message"] and time.monotonic() < until, reply
                    time.sleep(.025)
                assert started.wait(90), "the long turn never reached the provider"
                assert not reserve_idle(phone), "an update must wait for the real agent turn"
                print("PASS: idle restart reserves an empty host, releases on cancel, and waits for active Claude turns")
                restart = subprocess.Popen(
                    [root / "bin" / "ox-host", "restart", "--timeout", "120"], env=env,
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                deadline = time.monotonic() + 30
                while "draining for an update, 1 running turn(s)" not in log_path.read_text():
                    assert time.monotonic() < deadline, log_path.read_text()
                    time.sleep(0.05)
                print("PASS: the host drains with the turn still running")

                phone.act(scope, "refused", {"send_text": [thread, "NEW-TURN during drain"]})
                refusal = phone.until(lambda f: f["type"] == "error" and f.get("id") == "refused" or None)
                assert "restarting for an update" in refusal["message"], refusal
                print("PASS: a new turn during the drain is refused with the update reason")

                release.set()
                finish(desktop, scope)
                print("PASS: the running turn finished on the old host")
                old.wait(30)
                assert old.returncode == 0, old.returncode

                output, _ = restart.communicate(timeout=60)
                assert restart.returncode == 0, output
                assert "is running" in output and "interrupted" not in output, output
                print("PASS: ox-host restart started the new host after the drain")

                phone, again = hello(socket_path)
                assert again["host"]["epoch"] != epoch, (epoch, again["host"])
                state = ready(phone, scope)
                row = next(r for r in state["sessions"] if r["id"] == thread)
                assert row["status"] != "interrupted", row
                print(f"PASS: the phone reconnected to the new host; the session is {row['status']}, not interrupted")
                # Reopen the saved session on the new host, then continue it.
                phone.act(scope, "select", {"select_thread": thread})
                phone.until(lambda f: f.get("id") == "select" and f["type"] in ("ack", "error"))
                deadline = time.monotonic() + 60
                while True:
                    key = uuid.uuid4().hex
                    phone.act(scope, key, {"send_text": [thread, "AFTER-UPGRADE next turn"]})
                    reply = phone.until(lambda f: f.get("id") == key and f["type"] in ("ack", "error")
                                        or (f["type"] == "error" and None))
                    if reply["type"] == "ack":
                        break
                    assert "ready parent" in reply["message"] and time.monotonic() < deadline, reply
                    time.sleep(0.5)
                finish(phone, scope)
                print("PASS: the same session takes a new turn on the new host")

                # A turn that outlasts the drain bound is cut and reported, never replayed.
                phone.act(scope, "stuck", {"send_text": [thread, "STUCK-TURN hold forever"]})
                assert stuck.wait(90), "the held turn never reached the provider"
                bounded = subprocess.run([root / "bin" / "ox-host", "restart", "--timeout", "2"], env=env,
                                         capture_output=True, text=True, timeout=60)
                assert bounded.returncode == 0, bounded.stdout + bounded.stderr
                assert "1 turn(s) were still running at the bound" in bounded.stdout, bounded.stdout
                phone.close()
                phone, _ = hello(socket_path)
                state = ready(phone, scope)
                row = next(r for r in state["sessions"] if r["id"] == thread)
                assert row["status"] == "interrupted", row
                print("PASS: a turn past the drain bound is reported interrupted, by restart and by the new host")
                phone.close()
                subprocess.run([root / "bin" / "ox-host", "stop"], env=env, check=True,
                               stdout=subprocess.DEVNULL)
        finally:
            release.set()
            held.set()
            subprocess.run([root / "bin" / "ox-host", "stop"], env=env, check=False,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if restart and restart.poll() is None:
                restart.kill()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="ox-upgrade-", dir="/tmp") as directory:
        run(Path(directory).resolve(), *(Path(p).resolve(strict=True) for p in sys.argv[1:]))
