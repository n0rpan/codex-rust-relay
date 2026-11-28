"""Private Sway test for a packaged GUI, stock Codex, image input and real Pinyin composition."""
import argparse
import base64
from contextlib import ExitStack
from http.server import ThreadingHTTPServer
import io
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

from stock_session import Fixture


class Provider(Fixture):
    def do_POST(self):
        if self.path == "/v1/chat/completions":
            body = self.rfile.read(int(self.headers["Content-Length"]))
            self.rfile = io.BytesIO(body)
            self.server.requests.append(json.loads(body))
        super().do_POST()


def wait_for(description, predicate, seconds=15):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.08)
    raise AssertionError(f"Timed out waiting for {description}")


def run(gui, codex, root, drop_probe=None, fleet_harness=None):
    home, project, runtime, bin_dir = [root / key for key in ("home", "project", "runtime", "bin")]
    for directory in (home / ".codex", project, runtime, bin_dir):
        directory.mkdir(mode=0o700, parents=True)
    (bin_dir / "codex").symlink_to(codex)
    if node := shutil.which("node"):
        (bin_dir / "node").symlink_to(node)
    env = {
        "PATH": f"{bin_dir}:/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": str(home),
        "CODEX_HOME": str(home / ".codex"), "XDG_RUNTIME_DIR": str(runtime),
        "DBUS_SESSION_BUS_ADDRESS": f"unix:path={runtime}/bus",
        "XDG_SESSION_TYPE": "wayland", "XDG_CURRENT_DESKTOP": "sway",
        "WLR_BACKENDS": "headless", "WLR_RENDERER": "pixman", "WLR_LIBINPUT_NO_DEVICES": "1",
        "OX_DESK_STATS_SOCKET": str(root / "stats"), "OX_FIXTURE_KEY": "fixture-only",
        "HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1",
        "ALL_PROXY": "http://127.0.0.1:1", "NO_PROXY": "127.0.0.1,localhost",
    }
    for var in ("LD_LIBRARY_PATH", "VK_DRIVER_FILES"):
        if var in os.environ:
            env[var] = os.environ[var]
    conf = root / "sway.conf"
    conf.write_text("output * resolution 1280x900\ndefault_border none\n"
                    'for_window [title="OX drag fixture"] floating enable, move position 45 100, resize set 180 110\n')
    fcitx = home / ".config/fcitx5"
    fcitx.mkdir(parents=True)
    (fcitx / "profile").write_text(
        "[Groups/0]\nName=Default\nDefault Layout=us\nDefaultIM=pinyin\n\n"
        "[Groups/0/Items/0]\nName=keyboard-us\nLayout=\n\n"
        "[Groups/0/Items/1]\nName=pinyin\nLayout=\n\n[GroupOrder]\n0=Default\n"
    )
    layout = home / ".local/state/ox/desktop/workspace.json"
    layout.parent.mkdir(parents=True)
    layout.write_text(json.dumps({"project": str(project), "recent": [
        {"project": str(project), "api_model": "fixture/demo"}
    ]}))
    src = project / "sample_cartographer.rs"
    src.write_text("fn main() {}\n")
    subprocess.run(["git", "init", "-q", str(project)], env=env, check=True)
    subprocess.run(["git", "-C", str(project), "add", src.name], env=env, check=True)

    with ExitStack() as stack:
        log = stack.enter_context((root / "process.log").open("w"))

        def launch(*args, stdin=None):
            proc = subprocess.Popen(args, env=env, stdin=stdin, stdout=log, stderr=log)
            def terminate(exc_type, _, __):
                status = proc.poll()
                if exc_type:
                    state = f"exited ({status})" if status is not None else "running"
                    print(f"Fixture process {Path(args[0]).name}: {state}", file=sys.stderr)
                if status is not None:
                    return
                proc.terminate()
                try:
                    proc.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
            stack.push(terminate)
            return proc

        def command(*args, stdin=None):
            return subprocess.check_output(args, env=env, stderr=log, stdin=stdin, timeout=8)

        def key(symbol, *mods):
            command("wtype", *[part for m in mods for part in ("-M", m)], "-k", symbol)

        def copy_selected():
            command("wtype", "-M", "ctrl", "-k", "a", "-s", "80", "-k", "c", "-m", "ctrl")
            offered = subprocess.run(["wl-paste", "--no-newline"], env=env,
                                     stdout=subprocess.PIPE, stderr=log, timeout=3)
            if offered.returncode == 0 and offered.stdout:
                return offered.stdout.decode()
            return None

        def selected_text():
            command("wl-copy", "--clear")
            return wait_for("the focused field to offer its selection on the private clipboard",
                            copy_selected, seconds=5)

        server = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), Provider))
        server.mode, server.requests = "allow", []
        threading.Thread(target=server.serve_forever, daemon=True).start()
        stack.callback(server.shutdown)
        (home / ".codex/codeox.toml").write_text(
            '[providers.fixture]\nname="Fixture"\nwire_api="chat_completions"\n'
            f'base_url="http://127.0.0.1:{server.server_port}/v1"\n'
            'discovery=false\nenv_key="OX_FIXTURE_KEY"\n'
            '[[providers.fixture.models]]\nid="demo"\ncontext_window=131072\n'
            'input_modalities=["text","image"]\n'
        )
        launch(os.environ.get("SWAY_BIN", "sway"), "-c", str(conf))
        env["WAYLAND_DISPLAY"] = wait_for("Sway", lambda: next(
            (p.name for p in runtime.glob("wayland-*") if p.is_socket()), None))
        env["SWAYSOCK"] = str(wait_for("the Sway IPC socket", lambda: next(runtime.glob("sway-ipc*.sock"), None)))
        # Keep the seat's keyboard capability between wtype invocations.
        launch("wtype", "-k", "Shift_L", "-s", "180000")
        launch("dbus-daemon", "--session", "--nofork", "--address=" + env["DBUS_SESSION_BUS_ADDRESS"])
        wait_for("the private D-Bus session", lambda: (runtime / "bus").is_socket())
        launch("fcitx5", "--disable", "classicui,cloudpinyin,notificationitem,notifications")
        wait_for("the private input method", lambda: b"true" in command(
            "busctl", "--address=" + env["DBUS_SESSION_BUS_ADDRESS"], "call",
            "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
            "NameHasOwner", "s", "org.fcitx.Fcitx5"))
        time.sleep(.7)
        stack.enter_context(session_host(gui.with_name("ox-host"),root,env,log))
        if fleet_harness:
            (home / ".codex/config.toml").write_text("[features]\ntool_search=false\n")
            from fleet_window import check_fleet
            check_fleet(fleet_harness, gui, root, project, env, stack, log, launch, command, key, server)
            return
        launch(gui, "--desk-stats", project)
        probe = stack.enter_context(socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM))
        probe.bind(str(root / "probe"))
        probe.settimeout(1)

        def stats():
            try:
                probe.sendto(b"snapshot", env["OX_DESK_STATS_SOCKET"])
                return json.loads(probe.recv(16384))
            except (FileNotFoundError, ConnectionRefusedError, TimeoutError):
                return {}
        wait_for("the route to become ready", lambda: stats().get("connected"))
        # Connection readiness precedes Wayland keyboard focus on a cold launch.
        # Establish a complete input/copy round trip before enabling the IME.
        # The temporary text is replaced on every attempt and never submitted.
        command("wl-copy", "--clear")
        def composer_accepts_keys():
            command("wtype", "-M", "ctrl", "-k", "l", "-s", "100", "-k", "a",
                    "-m", "ctrl", "-s", "50", "focus")
            return copy_selected() == "focus"
        wait_for("the visible composer to accept keyboard input", composer_accepts_keys)
        key("BackSpace")
        # Select Pinyin explicitly and compose with physical keysyms so fcitx5
        # sends real Wayland preedit/commit events to the two different controls.
        def compose_chinese(keys, expected, confirm="space"):
            command("fcitx5-remote", "-s", "pinyin")
            command("fcitx5-remote", "-o")
            wait_for("Pinyin activation", lambda: command("fcitx5-remote").strip() == b"2"
                     and command("fcitx5-remote", "-n").strip() == b"pinyin")
            time.sleep(.4)
            # Keep one virtual keyboard for the whole composition. Replacing it
            # between letters can restart Wayland keyboard focus mid-preedit.
            strokes = [part for letter in keys for part in ("-k", letter, "-s", "75")]
            command("wtype", *strokes, "-s", "300", "-k", confirm)
            time.sleep(.25)
            command("fcitx5-remote", "-c")
            time.sleep(.5)
            actual=selected_text()
            if expected is not None:
                assert actual == expected, f"Expected composed text {expected!r}, got {actual!r}"
            else:
                assert actual.strip(), "The composition did not commit any text"
        compose_chinese("nihao", None, confirm="Return")
        assert not stats().get("selected_thread") and not server.requests, "IME confirmation sent a draft"
        key("BackSpace")
        compose_chinese("nihao","你好")
        key("End")
        key("k", "ctrl")
        time.sleep(.2)
        compose_chinese("zhongguo","中国")
        key("Escape")
        assert selected_text() == "你好"
        print("Pinyin composed 你好 in the composer and 中国 in the palette, and Escape restored the draft.", flush=True)

        # Hide the sidebar to traverse just the Settings control tree. The seventh
        # page is Favorites. Its unsaved alias is an ordinary Settings text field.
        key("b", "ctrl")
        key("comma", "ctrl")
        time.sleep(.5)
        for _ in range(7):
            key("Tab")
        key("Return")
        def alias_tab_steps():
            controls = [c["id"] for c in stats().get("layout", []) if c["enabled"]]
            if "Starred models" in controls and "add-favorite" in controls:
                return controls.index("add-favorite") - controls.index("Starred models")
            return None

        # Each intervening button is a tab stop. The alias input takes the
        # final stop before Add alias. Avoid copying text while its IME focus
        # is being established.
        for _ in range(wait_for("Starred models settings to load", alias_tab_steps)):
            key("Tab")
        compose_chinese("beijing","北京")
        key("comma", "ctrl")
        key("b", "ctrl")
        assert selected_text() == "你好"
        print("Pinyin composed 北京 in the Settings model-alias field and closing Settings restored the draft.", flush=True)
        command("wtype", "Two lines")
        key("Return", "shift")
        command("wtype", "Second line")
        assert selected_text() == "Two lines\nSecond line"
        assert not stats().get("selected_thread")
        command("wtype", "Inspect @cartog")
        # Wait for the runtime's debounced fuzzyFileSearch without moving
        # focus away from the field while the result is still in flight.
        time.sleep(.8)
        key("Tab")
        assert selected_text() == "Inspect @sample_cartographer.rs ", "Codex file completion was not inserted"
        key("End")
        png = root / "attachment.png"
        png.write_bytes(base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
        ))
        with png.open("rb") as image:
            launch("wl-copy", "--foreground", "--type", "image/png", stdin=image)
        wait_for("a compositor clipboard image",
                 lambda: b"image/png" in command("wl-paste", "--list-types"))
        key("v", "ctrl")
        def received(requests):
            return next((parts for body in requests for message in body.get("messages", [])
                         if isinstance(parts := message.get("content"), list)
                         and any(part.get("type") == "image_url" for part in parts)), None)
        # Submit when conversion and native model discovery have finished.
        def send_image():
            if not received(server.requests) and not stats().get("selected_thread"):
                key("Return")
            return received(server.requests)
        content = wait_for("the actual upstream multimodal request", send_image, seconds=40)
        assert any("Inspect @sample_cartographer.rs" in p.get("text", "") for p in content), "Missing resolved file mention"
        url = next(p["image_url"]["url"] for p in content if p.get("type") == "image_url")
        assert url.startswith("data:image/") and base64.b64decode(url.split(",", 1)[1])
        print("The stock Codex relay sent the keyboard-selected file mention and clipboard PNG.", flush=True)
        wait_for("the thread approval", lambda: stats().get("selected_requests"))
        key("1", "ctrl", "alt")
        wait_for("approval completion", lambda: (project / "approved.txt").exists())
        thread = stats()["selected_thread"]
        key("n", "ctrl")
        wait_for("a new draft", lambda: stats().get("selected_thread") is None)
        key("Down", "ctrl", "alt")
        wait_for("the previous session", lambda: stats().get("selected_thread") == thread)
        print("Keyboard approval, new draft, and session navigation reached the stock thread.", flush=True)
        key("n", "ctrl")
        wait_for("a new draft for host-scoped navigation", lambda: stats().get("selected_thread") is None)
        key("k", "ctrl")
        command("wl-copy", "--clear")
        query = "@local " + str(project)
        def type_host_query():
            command("wtype", "-M", "ctrl", "-k", "a", "-m", "ctrl", "-s", "50", query)
            return copy_selected() == query
        wait_for("the palette to accept the host-scoped query", type_host_query)
        key("End")
        def open_scoped_session():
            key("Return")
            return stats().get("selected_thread") == thread
        wait_for("the exact native session from the host-scoped command palette", open_scoped_session)
        print("The @local palette opened the original stock session from a fresh draft.", flush=True)

        if drop_probe:
            from gtk_drop import drag_image
            key("n", "ctrl")
            wait_for("a fresh drop target", lambda: stats().get("selected_thread") is None)
            command("wtype", "-M", "ctrl", "-k", "l", "-m", "ctrl", "-s", "200", "Inspect dropped image")
            assert selected_text() == "Inspect dropped image"
            key("End")
            count = len(server.requests)
            drag_image(drop_probe, png, env, launch, command, wait_for)
            key("l", "ctrl")
            def dropped():
                if not stats().get("selected_thread"):
                    key("Return", "ctrl")
                return received(server.requests[count:])
            parts = wait_for("the external image drop to reach the upstream provider", dropped, seconds=40)
            assert any("Inspect dropped image" in p.get("text", "") for p in parts)
            print("An external GTK image dropped onto the composer reached the stock relay.", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("gui", type=Path)
    parser.add_argument("stock_codex", type=Path)
    parser.add_argument("--gtk-drop", metavar="PROBE", nargs="?", const="",
                        help="Test an external GTK drop using ox-desk-probe beside the GUI or at PROBE")
    parser.add_argument("--fleet-harness", type=Path, help="Exercise a paired TLS host with the ox-desktop test binary")
    args = parser.parse_args()
    gui = args.gui.resolve(strict=True)
    drop_probe = None
    if args.gtk_drop is not None:
        drop_probe = (Path(args.gtk_drop) if args.gtk_drop else gui.with_name("ox-desk-probe")).resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="ox-keyboard-") as fixture:
        run(gui, args.stock_codex.resolve(strict=True), Path(fixture), drop_probe, args.fleet_harness.resolve(strict=True) if args.fleet_harness else None)
