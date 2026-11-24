"""Exercise a packaged app inside a private Hyprland 0.57 session on a headless labwc output."""
import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("--app", type=Path, required=True)
parser.add_argument("--fake", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
args = parser.parse_args()


def wait(fn, timeout=10):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = fn()
        if value:
            return value
        time.sleep(.08)
    raise TimeoutError(fn)


def stop(p):
    if p.poll() is None:
        os.killpg(p.pid, signal.SIGTERM)
        try:
            p.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()


# Hyprland adds a build hash, timestamp, random suffix and socket name to the runtime path.
# A long TMPDIR can exceed Linux's 108-byte Unix socket address before IPC starts.
with tempfile.TemporaryDirectory(prefix="oxh-", dir="/tmp") as tmp, ExitStack() as cleanup:
    root = Path(tmp)
    env = {k: os.environ[k] for k in ("PATH", "LD_LIBRARY_PATH", "VK_DRIVER_FILES", "WLR_RENDERER",
                                       "WLR_RENDER_DRM_DEVICE") if k in os.environ}
    for key in ("HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                "XDG_CACHE_HOME", "CODEX_HOME"):
        directory = root / key.lower()
        directory.mkdir(mode=0o700)
        env[key] = str(directory)
    env.update(WLR_BACKENDS="headless", WLR_LIBINPUT_NO_DEVICES="1", XDG_SESSION_TYPE="wayland",
               HYPRLAND_NO_SD_VARS="1", HYPRLAND_NO_SD_TARGET="1", HYPRLAND_NO_SD_NOTIFY="1",
               HYPRLAND_NO_CRASHREPORTER="1", AQ_DRM_DEVICES="/dev/null")
    log = cleanup.enter_context((root / "compositor.log").open("w+"))

    def start(command, child_env):
        process = subprocess.Popen(command, env=child_env, stdout=log, stderr=log, start_new_session=True)
        cleanup.callback(stop, process)
        return process

    def socket_for(runtime):
        return next((str(p) for p in Path(runtime).glob("wayland-*") if p.is_socket()), None)

    outer = start(["labwc", "-C", env["XDG_CONFIG_HOME"]], env)
    env["WAYLAND_DISPLAY"] = wait(lambda: socket_for(env["XDG_RUNTIME_DIR"]))
    start(["wtype", "-s", "120000"], env)
    inner = root / "r"
    inner.mkdir(mode=0o700)
    env["XDG_RUNTIME_DIR"] = str(inner)
    config = root / "test.lua"
    config.write_text('hl.monitor({output="",mode="1920x1080@60",position="auto",scale=1.25})\n'
                      'hl.config({general={layout="dwindle",gaps_in=2,gaps_out=4},input={kb_layout="us,no"},'
                      'animations={enabled=false},misc={disable_hyprland_guiutils_check=true,disable_hyprland_logo=true},'
                      'ecosystem={no_update_news=true,no_donation_nag=true}})\n')
    start(["dbus-run-session", "--", "Hyprland", "-c", str(config)], env)
    ipc = wait(lambda: next((p for p in inner.glob("hypr/*/.socket.sock") if p.is_socket()), None))
    env["HYPRLAND_INSTANCE_SIGNATURE"] = ipc.parent.name
    env["WAYLAND_DISPLAY"] = wait(lambda: socket_for(inner))

    def ctl(*command):
        return subprocess.check_output(["hyprctl", *command], env=env, timeout=5, text=True).strip()

    def monitor():
        return json.loads(ctl("-j", "monitors", "all"))[0]

    def dispatch(lua):
        result = ctl("dispatch", lua)
        assert result == "ok", result

    app = root / "ox-desktop"
    shutil.copy2(args.app.resolve(), app)
    (root / "ox").write_text("#!/bin/sh\nexec " + shlex.quote(str(args.fake.resolve())) + ' --synthetic "$@"\n')
    (root / "ox").chmod(0o755)
    project = root / "project"
    project.mkdir()
    gui = start([app, project], env)

    def window(pid):
        return next((w for w in json.loads(ctl("-j", "clients")) if w["pid"] == pid and w["mapped"]), None)

    def screenshot_words():
        shot = root / "screen.png"
        capture = subprocess.run(["grim", str(shot)], env=env, stderr=log, timeout=8)
        if capture.returncode != 0:
            return ""  # The compositor can briefly lack a capturable frame after a mode change.
        return subprocess.check_output(["tesseract", str(shot), "stdout", "--psm", "11"], env=env,
                                       timeout=8, stderr=subprocess.DEVNULL, text=True).lower()

    single = wait(lambda: window(gui.pid))
    assert not single["floating"]
    report = {"hyprland": json.loads(ctl("-j", "version"))["tag"], "scales": []}
    for scale in (1.25, 1.5):
        ctl("eval", f'hl.monitor({{output="",mode="1920x1080@60",position="auto",scale={scale}}})')
        wait(lambda: monitor()["scale"] == scale)
        wait(lambda: "connected" in screenshot_words())
        report["scales"].append({"scale": scale, "size": window(gui.pid)["size"]})
    before = window(gui.pid)["size"]
    second = start([app, project], env)
    wait(lambda: window(second.pid))
    split = wait(lambda: size if (size := window(gui.pid)["size"]) != before else None)
    assert not window(gui.pid)["floating"] and not window(second.pid)["floating"]
    stop(second)
    wait(lambda: window(gui.pid)["size"] == before)
    report["tiling"] = {"single":before,"split":split,"restored":True}
    for workspace in (2, 1):
        dispatch(f"hl.dsp.focus({{workspace={workspace}}})")
        wait(lambda: monitor()["activeWorkspace"]["id"] == workspace)
    report["workspace_restored"] = wait(lambda: "connected" in screenshot_words())
    assert report["workspace_restored"]
    for action in ("off", "on"):
        dispatch(f'hl.dsp.dpms({{action="{action}"}})')
        wait(lambda: monitor()["dpmsStatus"] == (action == "on"))
    wait(lambda: "connected" in screenshot_words())
    report["output_power_cycle"] = True
    keyboard = wait(lambda: next((k for k in json.loads(ctl("-j","devices"))["keyboards"]
                                 if k["layout"] == "us,no"), None))
    def layout():
        return next(k["active_keymap"] for k in json.loads(ctl("-j","devices"))["keyboards"]
                    if k["name"] == keyboard["name"])
    original = layout()
    ctl("switchxkblayout", keyboard["name"], "next")
    changed = wait(lambda: value if (value := layout()) != original else None)
    ctl("switchxkblayout", keyboard["name"], "prev")
    wait(lambda: layout() == original)
    report["keymaps"] = [original,changed,layout()]
    report["hardware_suspend_resume"] = "unverified"
    args.out.write_text(json.dumps(report,indent=2) + "\n")
    print(json.dumps(report))
