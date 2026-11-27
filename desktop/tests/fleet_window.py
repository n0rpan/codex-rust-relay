"""Two isolated Linux host engines, a signed desktop pairing and real keyboard input."""
import json
from pathlib import Path
import shutil
import ssl
import subprocess
import sys

from host_fixture import session_host
from host_stock_session import Client, change
from keyboard_window import wait_for

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ox-host-web/tests"))
from native_client import Forward, ThreadedTCP, admin, certificate, managed_server


def check_fleet(harness, gui, root, project, env, stack, log, launch, command, key):
    studio = root / "studio"
    home = studio / "home"
    codex = home / ".codex"
    codex.mkdir(mode=0o700, parents=True)
    shutil.copy2(Path(env["CODEX_HOME"]) / "codeox.toml", codex / "codeox.toml")
    # The test binary lives under deps/ instead of beside the settings companion.
    for name in ("ox", "ox-host"):
        shutil.copy2(gui.with_name(name), root / "bin" / name)
    target = dict(env, HOME=str(home), CODEX_HOME=str(codex), XDG_STATE_HOME=str(home / "state"),
                  XDG_CONFIG_HOME=str(home / "config"), CLAUDE_CONFIG_DIR=str(home / ".claude"))
    stack.enter_context(session_host(gui.with_name("ox-host"), studio, target, log))
    state = Path(target["OX_HOST_STATE_DIR"])
    wait_for("the receiving host admin socket", lambda: (state / "remote-admin.sock").exists())
    subprocess.run([root / "bin/ox", "host", "name", "Studio"], env=target, check=True, stdout=log)
    remote_project = studio / "alternate-checkout"
    duplicate = studio / "duplicate"
    for path in (remote_project, duplicate):
        subprocess.run(["git", "init", "-q", str(path)], env=target, check=True)
    for path, origin in [(project, "git@code.fixture.test:team/ox.git"),
                         (remote_project, "https://code.fixture.test/team/ox"),
                         (duplicate, "https://code.fixture.test/team/ox")]:
        subprocess.run(["git", "-C", str(path), "remote", "add", "origin", origin], env=target, check=True)
    for path, host_env in [(project, env), (remote_project, target), (duplicate, target)]:
        subprocess.run([root / "bin/ox", "host", "project", "add", path], env=host_env, check=True, stdout=log)
    scope = {"project": str(remote_project), "route": {"runtime": "codex", "billing": {"api": "fixture/demo"}}}
    peer = Client(target["OX_HOST_SOCKET"])
    stack.callback(peer.close)
    def directory(query):
        peer.send(type="discover", id="check-projects", projects=query)
        frame = peer.until(lambda f: f["type"] == "discovered_end" and f["id"] == "check-projects")
        return frame["directory"]

    # A catalog socket outlives settings edits. Keep it open through both edits.
    config = codex / "codeox.toml"
    original = config.read_text()
    extra_route = {"runtime": "codex", "billing": {"api": "fixture/extra"}}
    assert extra_route not in directory("list")["routes"]
    config.write_text(original + '\n[[providers.fixture.models]]\nid="extra"\n')
    assert extra_route in directory("list")["routes"]
    config.write_text(original)
    assert extra_route not in directory({"checkout": str(remote_project)})["routes"]
    assert directory({"checkout": str(root / "unknown")})["checkouts"] == []
    peer.open(scope)
    snapshot = peer.until(lambda f: f["type"] == "snapshot")["scopes"][0]
    if "start" not in snapshot["capabilities"]:
        peer.until(lambda f: (s := change(f, "state")) and "start" in s["capabilities"])
    peer.act(scope, "seed", {"send_text": [None, "Inspect the remote navigation fixture"]})
    remote_id = peer.until(lambda f: f["type"] == "opened")["thread"]
    public = ThreadedTCP(("127.0.0.1", 0), Forward)
    port = public.server_address[1]
    origin = f"https://studio.fixture.ts.net:{port}"
    public.backend = admin(state, {"type": "enable", "origin": origin, "port": 0})["port"]
    invite = admin(state, {"type": "pair", "login": "phone@example.test", "terminal": False})["pairing"]
    (studio / "code").write_text(invite["short_code"])
    certificate(studio)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(studio / "server.pem", studio / "server.key")
    public.socket = tls.wrap_socket(public.socket, server_side=True)
    managed_server(stack, public)
    env.update(OX_TEST_NATIVE_DIR=str(studio), OX_TEST_ORIGIN=origin, OX_TEST_LOOPBACK=f"127.0.0.1:{port}",
               OX_TEST_PROJECT=str(project), DBUS_SESSION_BUS_ADDRESS="unix:path=" + str(root / "unused-bus"))
    app = launch(harness, "--exact", "paired_fleet_window", "--ignored", "--nocapture")
    wait_for("the paired desktop window", lambda: Path(env["OX_DESK_STATS_SOCKET"]).is_socket(), seconds=40)

    def screen():
        shot = root / "fleet.png"
        command("grim", str(shot))
        return command("tesseract", str(shot), "stdout", "--psm", "6").decode().casefold()

    wait_for("the paired host toolbar", lambda: "manage hosts" in screen())

    def type_in_focused_field(text):
        def type_and_copy():
            command("wtype", "-M", "ctrl", "-k", "a", "-m", "ctrl", "-s", "40", text)
            command("wtype", "-M", "ctrl", "-k", "a", "-s", "50", "-k", "c", "-m", "ctrl")
            result = subprocess.run(["wl-paste", "--no-newline"], env=env, capture_output=True, timeout=3)
            return result.stdout == text.encode()
        wait_for("the focused field to receive " + text, type_and_copy)
        key("End")

    def choose(text, expected):
        key("k", "ctrl")
        wait_for("the command palette", lambda: "command palette" in screen())
        type_in_focused_field(text)
        wait_for("the destination row", lambda: expected in screen())
        key("Return")

    selected = root / "home/.local/state/ox/desktop/selected-host.json"
    host_id = (state / "host-id").read_text().strip()
    choose("@studio " + str(remote_project), "inspect the remote navigation fixture")
    wait_for("selection of the paired host", lambda: selected.exists() and json.loads(selected.read_text()) == host_id)
    # Its private workspace layout records the actual selected native thread.
    wait_for("the remote thread on its own workbench", lambda: any(remote_id in f.read_text()
             for f in selected.parent.rglob("*layout*.json")), seconds=30)
    choose("@local", "this computer")
    wait_for("keyboard navigation back to the original host", lambda: json.loads(selected.read_text()) is None)
    # The last session opened in this repository was on Studio. Its checkout
    # has a different name and directory, and the local workbench is visible.
    key("n", "ctrl")
    wait_for("the shared checkout selector", lambda: "duplicate" in screen())
    key("Return")
    wait_for("the default checkout to be the most recently used host", lambda: json.loads(selected.read_text()) == host_id)
    key("l", "ctrl")
    type_in_focused_field("Fresh alternate checkout session")
    key("Return")
    def has_fresh_thread():
        peer.open(scope)
        snap = peer.until(lambda frame: frame["type"] == "snapshot")["scopes"][0]
        return any(row["id"] != remote_id and "fresh alternate" in row["title"].casefold()
                   for row in snap["sessions"])
    wait_for("a second stock thread on the selected checkout and route", has_fresh_thread)
    history = selected.with_name("project-hosts.json")
    wait_for("the exact remembered checkout", lambda: history.exists() and
             {"host": host_id, "path": str(remote_project)} in json.loads(history.read_text()).values())
    choose("@local", "this computer")
    wait_for("the local workbench", lambda: json.loads(selected.read_text()) is None)
    key("n", "ctrl")
    wait_for("both checkouts before changing the remote origin", lambda: "duplicate" in screen())
    subprocess.run(["git", "-C", remote_project, "remote", "set-url", "origin",
                    "https://code.fixture.test/team/different"], env=target, check=True)
    key("Return")
    wait_for("the stale checkout to be rejected", lambda: "changed" in screen() or "unavailable" in screen())
    assert json.loads(selected.read_text()) is None
    key("Escape")
    subprocess.run(["git", "-C", remote_project, "remote", "set-url", "origin",
                    "https://code.fixture.test/team/ox"], env=target, check=True)
    app.terminate()
    app.wait(timeout=5)

    launch("wtype", "-k", "Shift_L", "-s", "180000")
    env["OX_TEST_CLIENT_ONLY"] = "1"
    app = launch(harness, "--exact", "paired_fleet_window", "--ignored", "--nocapture")
    wait_for("the client-only workspace", lambda: "choose a host" in screen(), seconds=30)
    key("k", "ctrl")
    wait_for("workspace search before any host has been opened", lambda: "filter by title" in screen())
    key("Escape")
    wait_for("Escape to restore the client-only workspace", lambda: "choose a host" in screen())
    key("k", "ctrl")
    wait_for("the workspace filter to reopen", lambda: "filter by title" in screen())
    type_in_focused_field("@studio " + str(remote_project))
    wait_for("the remote thread in the workspace filter", lambda: "inspect the remote" in screen())
    key("Return")
    wait_for("the client-only window to open the remote workbench", lambda: json.loads(selected.read_text()) == host_id)
    type_in_focused_field("Fleet selection keyboard focus")
    assert app.poll() is None
    print("PASS signed navigation, live billing routes, alternate and duplicate checkouts, remembered host, changed origin, and client-only search.", flush=True)
