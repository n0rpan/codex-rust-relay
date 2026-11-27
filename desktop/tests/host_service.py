"""Exercise the packaged gateway and real daemon with a disposable user-service manager."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time

MANAGER = """#!/usr/bin/env python3
import json, os, signal, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
state = Path(os.environ["OX_HOST_STATE_DIR"])
pidfile = state / "mock-service-pid"
with (state / "manager-events").open("a") as out: print(json.dumps(args), file=out)
if args[0] == "print":
    raise SystemExit(0 if pidfile.exists() else 1)
if "start" in args or args[0] in ["bootstrap", "kickstart"]:
    p = subprocess.Popen([os.environ["FIXTURE_HOST"], "run", "--socket", os.environ["OX_HOST_SOCKET"],
                          "--state-dir", str(state)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True)
    pidfile.write_text(str(p.pid))
if args[0] == "bootout" or "--now" in args:
    if pidfile.exists():
        os.kill(int(pidfile.read_text()), signal.SIGTERM)
        pidfile.unlink()
"""


def wait(predicate):
    until = time.monotonic() + 8
    while not predicate():
        if time.monotonic() >= until:
            raise AssertionError("Session host did not change service state")
        time.sleep(.025)


def reserve_serve_port(state):
    endpoint = state / "remote-admin.sock"
    wait(endpoint.is_socket)

    def request(kind, lease=None, **args):
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            client.connect(str(endpoint))
            client.settimeout(2)
            client.sendall(json.dumps({"type": kind, **args}).encode() + b"\n")
            with client.makefile("rb") as reader:
                reply = json.loads(reader.readline())
            if lease is not None and "Ok" in reply:
                lease.append(client)
                client = None
            return reply
        finally:
            if client is not None:
                client.close()

    origin = "https://workstation.fixture.ts.net:8443"
    with socket.socket() as occupied:
        occupied.bind(("127.0.0.1", 0))
        port = occupied.getsockname()[1]
        assert "Err" in request("reserve", origin=origin, port=port)
    leases = []
    try:
        token = request("reserve", leases, origin=origin, port=port)["Ok"]["reservation"]
        assert "Err" in request("reserve", origin=origin, port=port)
        assert "Err" in request("disable")
        assert "Err" in request("commit", ticket="another-ticket")
        assert "Err" in request("pair", login="fixture@example.test")
        assert request("status")["Ok"]["origin"] is None
        leases.pop().close()
        def reserve_again():
            return "Ok" in request("reserve", leases, origin=origin, port=port)
        wait(reserve_again)
        assert "Err" in request("commit", ticket=token)
        assert request("status")["Ok"]["enabled"] is False
        leases.pop().close()
        def enable():
            return "Ok" in request("enable", origin=origin, port=port)
        wait(enable)
        request("reserve", leases, origin=origin, port=port)["Ok"]
        assert "Err" in request("pair", login="fixture@example.test")
        leases.pop().close()
        wait(lambda: "Ok" in request("pair", login="fixture@example.test"))
        status = request("status")["Ok"]
        assert status["enabled"] and status["origin"] == origin and status["port"] == port
        assert "Err" in request("reserve_removal", origin=origin, expected_enabled=False)
        assert "Ok" in request("reserve_removal", leases, origin=origin, expected_enabled=True)
        assert request("status")["Ok"]["enabled"] is True
        assert "Err" in request("enable", origin=origin, port=port)
        leases.pop().close()
        wait(enable)
        removal = request("reserve_removal", leases, origin=origin, expected_enabled=True)["Ok"]
        assert request("commit", ticket=removal["reservation"])["Ok"]["enabled"] is False
    finally:
        for client in leases:
            client.close()


def run(ox, host, root):
    directories = {name: root / name for name in ["bin", "home", "runtime", "state", "config"]}
    for directory in directories.values():
        directory.mkdir(mode=0o700)
    binary = directories["bin"]
    fake_manager = binary / ("launchctl" if sys.platform == "darwin" else "systemctl")
    fake_manager.write_text(MANAGER)
    fake_manager.chmod(0o700)
    (binary / "ox-host").symlink_to(host)
    (binary / "python3").symlink_to(sys.executable)
    sock = directories["runtime"] / "host.sock"
    env = dict(os.environ, PATH=str(binary) + ":/usr/bin:/bin", HOME=str(directories["home"]),
               XDG_CONFIG_HOME=str(directories["config"]), XDG_STATE_HOME=str(directories["state"]),
               XDG_RUNTIME_DIR=str(directories["runtime"]), OX_HOST_SOCKET=str(sock),
               OX_HOST_STATE_DIR=str(directories["state"]), FIXTURE_HOST=str(host),
               OX_TEST_VENDOR_TOKEN="unshared-service-fixture")
    env["CODEX_HOME"] = str(root / "empty-codex")
    env["CLAUDE_CONFIG_DIR"] = str(root / "empty-claude")
    def command(*argv, check=True):
        return subprocess.run([ox, "host", *argv], env=env, text=True, capture_output=True, check=check, timeout=15)
    try:
        assert "stopped" in command("status").stdout
        command("install")
        wait(sock.is_socket)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
            probe.connect(str(sock))
        assert sock.stat().st_mode & 0o777 == 0o600
        status = command("status").stdout
        assert "running" in status and "Login service: installed" in status
        unit = (directories["config"] / "systemd/user/ox-host.service" if sys.platform != "darwin" else
                directories["home"] / "Library/LaunchAgents/dev.ox.session-host.plist")
        assert unit.is_file() and b"unshared-service-fixture" not in unit.read_bytes()
        if sys.platform == "darwin":
            import plistlib
            args = plistlib.loads(unit.read_bytes())["ProgramArguments"]
            assert Path(args[0]).resolve() == host and args[1:] == ["run", "--socket", str(sock), "--state-dir", str(directories["state"])]
        else:
            import shlex
            args = shlex.split(next(line.removeprefix("ExecStart=") for line in unit.read_text().splitlines() if line.startswith("ExecStart=")))
            assert Path(args[0]).resolve() == host and args[1:] == ["run", "--socket", str(sock), "--state-dir", str(directories["state"])]
        reserve_serve_port(directories["state"])
        tailscale = binary / "tailscale"
        tailscale.write_text("""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
mapping = Path(os.environ["OX_HOST_STATE_DIR"]) / "mock-serve.json"
args = sys.argv[1:]
node = "workstation.fixture.ts.net"
if args == ["status", "--json", "--peers=false"]:
    print(json.dumps({"BackendState": "Running", "Self": {"DNSName": node + ".", "UserID": 1},
                      "User": {"1": {"LoginName": "fixture@example.test"}}}))
elif args == ["serve", "status", "--json"]:
    print(mapping.read_text() if mapping.exists() else "{}")
elif len(args) == 4 and args[:2] == ["serve", "--bg"] and args[2].startswith("--https="):
    port = args[2].removeprefix("--https=")
    data = json.loads(mapping.read_text()) if mapping.exists() else {}
    data.setdefault("TCP", {})[port] = {"HTTPS": True}
    data.setdefault("Web", {})[f"{node}:{port}"] = {"Handlers": {"/": {"Proxy": args[3]}}}
    mapping.write_text(json.dumps(data))
elif len(args) == 3 and args[0] == "serve" and args[1].startswith("--https=") and args[2] == "off":
    port = args[1].removeprefix("--https=")
    if mapping.exists():
        data = json.loads(mapping.read_text())
        data.get("TCP", {}).pop(port, None)
        data.get("Web", {}).pop(f"{node}:{port}", None)
        mapping.write_text(json.dumps(data))
else:
    raise SystemExit(f"unexpected tailscale args: {args}")
""")
        tailscale.chmod(0o700)
        (directories["state"] / "mock-serve.json").write_text(json.dumps({
            "TCP": {"8443": {"HTTPS": True}},
            "Web": {"workstation.fixture.ts.net:8443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:9999"}}}},
        }))
        conflict = command("pair", check=False)
        assert conflict.returncode != 0 and "--port" in conflict.stderr, conflict
        paired = command("pair", "--port", "9443", "--listen-port", "49162")
        assert "https://workstation.fixture.ts.net:9443/#pair=" in paired.stdout, paired
        assert "https://workstation.fixture.ts.net:9443 → 127.0.0.1:49162" in command("status").stdout
        # Subsequent pair without flags remembers the configured custom ports.
        paired_again = command("pair")
        assert "https://workstation.fixture.ts.net:9443/#pair=" in paired_again.stdout, paired_again
        doctor = command("doctor", check=False)
        assert "remote access: https://workstation.fixture.ts.net:9443 → 127.0.0.1:49162 (tailnet only)" in doctor.stdout, doctor
        first = (directories["state"] / "mock-service-pid").read_text()
        command("install")
        assert (directories["state"] / "mock-service-pid").read_text() == first
        saved_unit = unit.read_text()
        command("uninstall")
        wait(lambda: not sock.exists())
        assert not unit.exists() and "stopped" in command("status").stdout
        command("uninstall")
        # A previous apply switched the package, drained the old host, then lost
        # the restart. The marker must survive if a service revives an older host.
        version = subprocess.check_output([host, "--version"], text=True).split()[1]
        marker = directories["state"] / "pending-update-restart"
        marker.write_text(version)
        unit.write_text(saved_unit)
        wrong_host = binary / "older-host"
        wrong_host.write_text("""#!/usr/bin/env python3
import json, os, socket
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
    path = os.environ["OX_HOST_SOCKET"]
    server.bind(path)
    os.chmod(path, 0o600)
    server.listen()
    while True:
        conn, _ = server.accept()
        with conn:
            if conn.recv(1024):
                reply = dict(type="hello", version=1, capabilities=[], routes=[], projects=[],
                             host=dict(version="old-fixture", epoch="old"))
                conn.sendall(json.dumps(reply).encode() + bytes([10]))
                break
    os.unlink(path)
""")
        wrong_host.chmod(0o700)
        helper = [host, "restart-if-updated"]
        failed = subprocess.run(helper, env=dict(env, FIXTURE_HOST=str(wrong_host)), text=True,
                                capture_output=True, timeout=15)
        assert failed.returncode and "version differs" in failed.stderr, failed
        assert marker.read_text() == version
        wait(lambda: not sock.exists())
        unit.unlink()
        (directories["state"] / "mock-service-pid").unlink(missing_ok=True)
        subprocess.run(helper, env=env, check=True, timeout=15)
        assert sock.is_socket() and not marker.exists()
        command("stop")
        wait(lambda: not sock.exists())
        subprocess.run(helper, env=env, check=True, timeout=15)
        assert not sock.exists()
        print("PASS: an incorrect host retains the restart marker; retry recovers a stopped host on the installed version.")
        print("Gateway service install, idempotent reinstall/status, private daemon socket and SIGTERM uninstall passed.")
    finally:
        if (directories["state"] / "mock-service-pid").exists():
            try:
                os.kill(int((directories["state"] / "mock-service-pid").read_text()), signal.SIGTERM)
            except ProcessLookupError:
                pass


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="ox-host-service-", dir="/tmp") as tmp:
        run(Path(sys.argv[1]).resolve(strict=True), Path(sys.argv[2]).resolve(strict=True), Path(tmp))
