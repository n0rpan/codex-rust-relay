#!/bin/sh
# Run inside an isolated X server with xdotool available.
set -eu
archive_dir=$(cd "$(dirname "$1")" && pwd)
fixture=$(mktemp -d)
pid=''
daemon=''
cleanup() {
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || :; fi
    if [ -n "$daemon" ]; then kill "$daemon" 2>/dev/null || :; wait "$daemon" 2>/dev/null || :; fi
    rm -rf "$fixture"
}
trap cleanup EXIT
mkdir -p "$fixture/home" "$fixture/project" "$fixture/runtime"
chmod 700 "$fixture/runtime"
bin="$fixture/home/.local/bin"
run_app() {
    env HOME="$fixture/home" CODEX_HOME="$fixture/home/.codex" XDG_RUNTIME_DIR="$fixture/runtime" \
        XDG_CONFIG_HOME="$fixture/home/.config" XDG_DATA_HOME="$fixture/home/.local/share" \
        OX_HOST_SOCKET="$fixture/runtime/host.sock" OX_HOST_STATE_DIR="$fixture/host-state" \
        XDG_SESSION_TYPE=x11 WAYLAND_DISPLAY='' "$@"
}
run_app env OX_INSTALL_DIR="$bin" OX_INSTALL_BASE_URL="file://$archive_dir" \
    sh "$(dirname "$0")/../../install.sh" --desktop
HOME="$fixture/home" CODEX_HOME="$fixture/home/.codex" XDG_RUNTIME_DIR="$fixture/runtime" \
    "$bin/ox-host" run --socket "$fixture/runtime/host.sock" --state-dir "$fixture/host-state" &
daemon=$!
for _ in $(seq 1 100); do [ -S "$fixture/runtime/host.sock" ] && break; kill -0 "$daemon" || exit 1; sleep 0.05; done
test -S "$fixture/runtime/host.sock"
run_app timeout 30 "$bin/ox-desktop" --desk-smoke "$fixture/project" >"$fixture/frame"
grep -E '^desk-first-frame ms=[0-9]+$' "$fixture/frame"
run_app "$bin/ox" desktop "$fixture/project"
if window=$(timeout 30 xdotool search --sync --onlyvisible --name '^OX Desktop$'); then
    pid=$(xdotool getwindowpid "$window")
    test "$(readlink -f "/proc/$pid/exe")" = "$(readlink -f "$bin/ox-desktop")"
    test "$(readlink -f "/proc/$pid/cwd")" = "$fixture/project"
    echo 'ox desktop opened the installed project window.'
else
    echo 'Timed out waiting for the installed OX Desktop window.' >&2
    exit 1
fi
