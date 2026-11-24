#!/bin/sh
# Launch both entry points against the packaged Wayland backend in a private compositor.
set -eu
if [ "$1" = "--run" ]; then
    shift
    app=''
else
    app=$(realpath "$1")
fi
fixture=$(mktemp -d)
compositor=''
keyboard=''
unset WAYLAND_DISPLAY DISPLAY SWAYSOCK
cleanup() {
    if [ -n "$keyboard" ]; then kill "$keyboard" 2>/dev/null || :; wait "$keyboard" 2>/dev/null || :; fi
    if [ -n "$compositor" ]; then kill "$compositor" 2>/dev/null || :; wait "$compositor" 2>/dev/null || :; fi
    rm -rf "$fixture"
}
trap cleanup EXIT
export XDG_RUNTIME_DIR="$fixture/runtime" XDG_SESSION_TYPE=wayland
mkdir -m 700 "$XDG_RUNTIME_DIR" "$fixture/home" "$fixture/project"
cat >"$fixture/sway.conf" <<'EOF'
output * resolution 1280x900
default_border none
EOF
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman sway --config "$fixture/sway.conf" >"$fixture/sway.log" 2>&1 &
compositor=$!
for _ in $(seq 1 100); do
    if ! kill -0 "$compositor" 2>/dev/null; then cat "$fixture/sway.log" >&2; exit 1; fi
    for socket in "$XDG_RUNTIME_DIR"/wayland-*; do
        if [ -S "$socket" ]; then export WAYLAND_DISPLAY="${socket##*/}"; break 2; fi
    done
    sleep 0.1
done
test -n "${WAYLAND_DISPLAY:-}" || { cat "$fixture/sway.log" >&2; exit 1; }
if [ -z "$app" ]; then
    for ipc in "$XDG_RUNTIME_DIR"/sway-ipc*.sock; do
        if [ -S "$ipc" ]; then export SWAYSOCK="$ipc"; break; fi
    done
    test -S "${SWAYSOCK:-}" || { cat "$fixture/sway.log" >&2; exit 1; }
    # Keep the virtual seat's keyboard capability throughout input driver restarts.
    wtype -s 600000 &
    keyboard=$!
    "$@"
    exit
fi
env -u DISPLAY HOME="$fixture/home" CODEX_HOME="$fixture/home/.codex" \
    XDG_STATE_HOME="$fixture/state" python3 "$(dirname "$0")/../tests/host_fixture.py" "$(dirname "$app")/ox-host" timeout 30 "$app" --desk-smoke "$fixture/project" >"$fixture/frame"
grep -E '^desk-first-frame ms=[0-9]+$' "$fixture/frame"
