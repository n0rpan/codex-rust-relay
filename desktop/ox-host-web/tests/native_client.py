"""Exercise the native desktop client over TLS against a disposable headless host."""
import argparse
import json
import socket
from contextlib import ExitStack
from http.server import ThreadingHTTPServer
from pathlib import Path
import shutil
import ssl
import subprocess
import sys
import tempfile

from browser import Forward, ThreadedTCP, admin, managed_server, wait
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests"))
from host_fixture import session_host


def certificate(root, hostname="studio.fixture.ts.net"):
    def openssl(*args):
        subprocess.run(["openssl", *args], cwd=root, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj",
            "/CN=OX native fixture CA", "-addext", "basicConstraints=critical,CA:TRUE",
            "-keyout", "ca.key", "-out", "ca.pem")
    openssl("req", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={hostname}",
            "-keyout", "server.key", "-out", "server.csr")
    (root / "extensions").write_text("basicConstraints=critical,CA:FALSE\n"
                                     f"subjectAltName=DNS:{hostname}\n"
                                     "extendedKeyUsage=serverAuth\n")
    openssl("x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key",
            "-CAcreateserial", "-days", "1", "-extfile", "extensions", "-out", "server.pem")


def parent_inventory(env, project, host_id):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.settimeout(12)
        conn.connect(env["OX_HOST_SOCKET"])
        with conn.makefile("rwb") as frames:
            def request(frame):
                frames.write(json.dumps(frame).encode() + b"\n")
                frames.flush()
                return json.loads(frames.readline())

            hello = request({"type": "hello", "version": 1})
            assert "worker_inventory" in hello["capabilities"]
            reply = request({"type": "worker_call", "id": "inventory",
                             "caller": {"project": str(project), "runtime": "codex",
                                        "launch": "fixture-list", "session": None},
                             "call": {"op": "list_hosts"}})
            row, = reply["result"]["hosts"]
            assert row["host"]["id"] == host_id
            assert row["parent_host"] and row["has_project"] and row["status"] == "online"


def run(host, cli, harness, stock=None):
    with tempfile.TemporaryDirectory(prefix="ox-native-") as tmp, ExitStack() as stack:
        root = Path(tmp)
        (root / "workspace").mkdir()
        home = root / "home"
        package = root / "bin"
        if stock:
            from claude_stock_session import setup
            from stock_session import Fixture
            codex, claude = stock
            _, env = setup(root, claude, stack)
            (package / "codex").symlink_to(codex)
            vendor = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
            vendor.mode = "allow"
            managed_server(stack, vendor)
            with (Path(env["CODEX_HOME"]) / "codeox.toml").open("a") as cfg:
                cfg.write('[providers.garden]\nname="Garden"\nwire_api="chat_completions"\n'
                          f'base_url="http://127.0.0.1:{vendor.server_port}/v1"\n'
                          'env_key="OX_FIXTURE_KEY"\ndiscovery=false\n'
                          '[[providers.garden.models]]\nid="demo"\ncontext_window=131072\n')
            env.update(OX_FIXTURE_KEY="fixture-only", OX_TEST_STOCK_TURNS="1")
        else:
            package.mkdir()
            home.mkdir()
            env = dict(HOME=str(home), CODEX_HOME=str(home / ".codex"), SHELL="/bin/sh",
                       PATH=str(package), CLAUDE_CONFIG_DIR=str(home / ".claude"))
            for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
                env[name] = env[name.lower()] = "http://127.0.0.1:1"
            env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost"
        # The host locates the settings helper beside its own binary.
        for src, name in [(host, "ox-host"), (cli, "ox")]:
            shutil.copy2(src, package / name)
        env.update(XDG_STATE_HOME=str(root / "state"), XDG_CONFIG_HOME=str(home / "config"),
                   XDG_DATA_HOME=str(home / "data"), OX_NO_BACKGROUND_UPDATE="1")
        # Run the packaged public CLI before first startup, then exercise the
        # live admin commands. The host requires an owner-only state directory.
        def host_cli(command, *args):
            return subprocess.check_output([package / "ox", "host", command, *args],
                                           env=env, text=True, timeout=10).strip()
        assert host_cli("policy", "set", "always-write") == "Always write"
        assert host_cli("policy") == "Always write"
        assert host_cli("policy", "set", "follow-interface") == "Follow interface"
        stack.enter_context(session_host(package / "ox-host", root, env))
        host_state = Path(env["OX_HOST_STATE_DIR"])
        wait(lambda: (host_state / "remote-admin.sock").exists())
        parent_inventory(env, root, (host_state / "host-id").read_text().strip())
        assert host_cli("name", "Studio") == host_cli("name") == "Studio"
        assert host_cli("labels", "set", "build") == host_cli("labels") == "build"
        public = ThreadedTCP(("127.0.0.1", 0), Forward)
        port = public.server_address[1]
        origin = f"https://studio.fixture.ts.net:{port}"
        public.backend = admin(host_state, {"type": "enable", "origin": origin, "port": 0})["port"]
        invitation = admin(host_state, {"type": "pair", "login": "phone@example.test",
                                        "terminal": True})["pairing"]
        (root / "code").write_text(invitation["short_code"])
        (root / "host-id").write_text((host_state / "host-id").read_text())
        certificate(root)
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.num_tickets = 0
        tls.load_cert_chain(root / "server.pem", root / "server.key")
        public.socket = tls.wrap_socket(public.socket, server_side=True)
        managed_server(stack, public)
        subprocess.run([harness, "--exact", "client::fixture::native_desktop", "--ignored"],
                       check=True, timeout=300 if stock else 60, env=dict(env, OX_TEST_NATIVE_DIR=tmp,
                       OX_TEST_LOOPBACK=f"127.0.0.1:{port}", OX_TEST_ORIGIN=origin))
        if stock:
            for runtime in ("claude", "codex"):
                assert (root / "workspace" / runtime / "approved.txt").read_text().strip() == "accepted"
            print("PASS stock Claude and Codex command approvals, fixed starts, YOLO review across reconnect and lower-mode resume", flush=True)
        assert not admin(host_state, {"type": "status"})["devices"]
        assert "fixture-native-brave" in (Path(env["CODEX_HOME"]) / "ox-api-keys.json").read_text()
        print("PASS native manual pairing, signed key import, host identity on reconnect, "
              "remote PTY review and exit, device revocation", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["host", "cli", "harness"]:
        parser.add_argument(name, type=Path)
    parser.add_argument("--stock", nargs=2, type=Path, metavar=("CODEX", "CLAUDE"))
    args = parser.parse_args()
    run(args.host.resolve(), args.cli.resolve(), args.harness.resolve(),
        [path.resolve() for path in args.stock] if args.stock else None)
