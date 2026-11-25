#!/bin/sh
# Install, run, upgrade and remove the OX session host as a brand-new account,
# with the real service manager: a systemd user service on Linux, a launchd
# agent on macOS. Needs passwordless sudo (CI runners have it).
#
# Usage: desktop/tests/host_clean_account.sh DIST_DIR
#   DIST_DIR holds the packaged ox-desktop-* archive and its .sha256.
#
# Checks:
#   - install.sh --desktop works for an account that has never run OX;
#   - `ox host install` registers the service and the host answers;
#   - `ox host doctor` reports the running host (stock clients are absent here,
#     so doctor itself exits non-zero for that reason and no other);
#   - `ox host restart` drains and comes back under a new process;
#   - the service manager restarts a host that crashes;
#   - rerunning the installer restarts the host on the new binary;
#   - `ox host uninstall` keeps OX state; `--purge` deletes it and leaves
#     Codex and Claude Code data alone.
set -eu
dist=$(cd "${1:?pass the dist directory}" && pwd)
repo=$(cd "$(dirname "$0")/../.." && pwd)
user="oxclean$(date +%s | tail -c 5)"
os=$(uname -s)
stage=$(mktemp -d /tmp/ox-clean.XXXXXX)
chmod 755 "$stage"
cp "$repo/install.sh" "$dist"/ox-desktop-* "$stage/"
chmod 644 "$stage"/*

fail() { echo "FAIL: $*" >&2; exit 1; }
cleanup() {
    if [ "$os" = Linux ]; then
        sudo loginctl disable-linger "$user" 2>/dev/null || true
        sudo pkill -KILL -u "$user" 2>/dev/null || true
        sudo userdel -r "$user" 2>/dev/null || true
    else
        sudo pkill -KILL -u "$user" 2>/dev/null || true
        sudo sysadminctl -deleteUser "$user" >/dev/null 2>&1 || true
    fi
    rm -rf "$stage"
}
trap cleanup EXIT

if [ "$os" = Linux ]; then
    sudo useradd --create-home --shell /bin/sh "$user"
    home="/home/$user"
    uid=$(id -u "$user")
    sudo loginctl enable-linger "$user"
    # Lingering starts user@UID.service. systemctl --user talks to it over
    # its private socket, so no D-Bus session bus is needed (the runner image
    # may not ship dbus-user-session).
    sudo systemctl start "user@$uid.service" || true
    for _ in $(seq 1 300); do [ -S "/run/user/$uid/systemd/private" ] && break; sleep 0.1; done
    if [ ! -S "/run/user/$uid/systemd/private" ]; then
        systemctl status "user@$uid.service" --no-pager || true
        fail "systemd --user did not start for $user"
    fi
    bus=""
    if [ -S "/run/user/$uid/bus" ]; then bus="unix:path=/run/user/$uid/bus"; fi
    as_user() {
        sudo -u "$user" env -i HOME="$home" USER="$user" LOGNAME="$user" \
            XDG_RUNTIME_DIR="/run/user/$uid" ${bus:+DBUS_SESSION_BUS_ADDRESS="$bus"} \
            PATH="$home/.local/bin:/usr/local/bin:/usr/bin:/bin" "$@"
    }
    service_active() { as_user systemctl --user is-active --quiet ox-host.service; }
    unit="$home/.config/systemd/user/ox-host.service"
    state="$home/.local/state/ox/host"
else
    password=$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')
    sudo sysadminctl -addUser "$user" -password "$password" -home "/Users/$user" >/dev/null 2>&1
    sudo createhomedir -c -u "$user" >/dev/null
    home="/Users/$user"
    uid=$(id -u "$user")
    as_user() {
        sudo launchctl asuser "$uid" sudo -u "$user" env -i HOME="$home" USER="$user" LOGNAME="$user" \
            PATH="$home/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" TMPDIR=/tmp "$@"
    }
    service_active() {
        as_user launchctl print "user/$uid/dev.ox.session-host" 2>/dev/null | grep -q 'state = running' ||
            as_user launchctl print "gui/$uid/dev.ox.session-host" 2>/dev/null | grep -q 'state = running'
    }
    unit="$home/Library/LaunchAgents/dev.ox.session-host.plist"
    state="$home/Library/Application Support/ox/host"
fi
ox="$home/.local/bin/ox"

wait_for() {
    for _ in $(seq 1 150); do
        if "$@"; then return 0; fi
        sleep 0.2
    done
    return 1
}
host_pid() {
    as_user "$ox" host doctor 2>/dev/null | sed -n 's/.*host: running, .* pid \([0-9]*\),.*/\1/p'
}

echo "== install as a new account ($user)"
as_user sh -c "cd /tmp && OX_INSTALL_BASE_URL=file://$stage sh $stage/install.sh --desktop"
as_user "$ox" --version

# Stock client data that OX must never touch.
as_user mkdir -p "$home/.codex/sessions" "$home/.claude/projects"
as_user sh -c "echo keep > '$home/.codex/sessions/sentinel' && echo keep > '$home/.claude/projects/sentinel'"

echo "== ox host install"
as_user "$ox" host install
[ -f "$unit" ] || sudo test -f "$unit" || fail "no service definition at $unit"
wait_for service_active || fail "the login service is not running"
as_user "$ox" host status | grep -q '^OX host: running' || fail "ox host status does not see the host"

echo "== ox host doctor"
set +e
report=$(as_user "$ox" host doctor 2>&1)
code=$?
set -e
echo "$report"
echo "$report" | grep -q 'ok   host: running, version' || fail "doctor did not report the running host"
echo "$report" | grep -q 'ok   login service: installed' || fail "doctor did not see the login service"
failures=$(echo "$report" | grep -c '  FAIL' || true)
[ "$failures" = 1 ] && echo "$report" | grep -q 'FAIL stock clients' ||
    fail "doctor reported a blocking problem other than the absent stock clients"
[ "$code" != 0 ] || fail "doctor exited 0 with a blocking problem"

echo "== ox host restart drains and comes back"
first=$(host_pid)
[ -n "$first" ] || fail "no host pid"
as_user "$ox" host restart --timeout 20
wait_for service_active || fail "the service is not running after restart"
second=$(host_pid)
[ -n "$second" ] && [ "$second" != "$first" ] || fail "restart kept pid $first"

echo "== the service manager restarts a crashed host"
sudo kill -KILL "$second"
crashed() { pid=$(host_pid); [ -n "$pid" ] && [ "$pid" != "$second" ]; }
wait_for crashed || fail "the host did not come back after a crash"

echo "== rerunning the installer restarts the host on the new binary"
before=$(host_pid)
as_user sh -c "cd /tmp && OX_INSTALL_BASE_URL=file://$stage sh $stage/install.sh --desktop" | tee "$stage/upgrade.log"
grep -q 'Restarting the OX session host' "$stage/upgrade.log" || fail "the installer did not restart the host"
after=$(host_pid)
[ -n "$after" ] && [ "$after" != "$before" ] || fail "the host kept running the old process after an update"

echo "== uninstall keeps state, --purge removes it, stock data stays"
as_user "$ox" host uninstall
sudo test ! -e "$unit" || fail "the service definition is still installed"
sudo test -d "$state" || fail "uninstall deleted OX state"
as_user "$ox" host uninstall --purge
sudo test ! -e "$state" || fail "--purge left OX state at $state"
sudo test -f "$home/.codex/sessions/sentinel" || fail "Codex data was touched"
sudo test -f "$home/.claude/projects/sentinel" || fail "Claude Code data was touched"
echo "PASS: clean-account install, restart, crash recovery, upgrade and uninstall on $os"
