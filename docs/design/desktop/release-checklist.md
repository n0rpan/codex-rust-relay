# Release checklist

Run this before tagging a release. CI covers most of it. The manual section covers what CI cannot do: sleep, reboot, a real network and a real tailnet.

## What CI already proves

| Area | Check | Where it runs |
| --- | --- | --- |
| Fast feedback | fmt, clippy, unit tests, Linux workbench, stock-client protocol on baseline and latest | `fast.yml`, every push |
| Packages and windows | four platform packages, window tests, stock sessions in the packaged app | `desktop.yml`, `full-ci` label, main, nightly, tags |
| Install as a new account | install, `ox host install`, doctor, restart, crash restart, reinstall restarts the host, uninstall keeps state, `--purge` removes it and leaves Codex and Claude Code data | `desktop/tests/host_clean_account.sh` (Linux systemd, macOS launchd) |
| Upgrade mid-turn | turn keeps running during the drain, new turns refused with the reason, new host under a new epoch, client reconnects, session idle and continues | `desktop/tests/host_upgrade_session.py`, Claude floor and latest |
| Host crash | SIGKILL mid-approval: new epoch, approval retired, session interrupted, stale answer refused, no native child left | `desktop/tests/host_stock_session.py` |
| Restart retirements | YOLO requests declined, unstarted handoffs failed, reasons name the update | `ox-host` `durable` and `handover` unit tests |
| Remote rules | loopback bind, identity only via Serve, invitation and pairing expiry, revocation, Funnel refused, push payloads | `ox-host-web` and `ox-desktop` unit tests, listed in [tailnet-check.md](tailnet-check.md) |
| Diagnostics | doctor fails on a stopped host, Serve off, Funnel, a stock client below its floor | `ox-host` `doctor` unit tests |

Before tagging, the latest `main` run of `CI` and `OX Desktop` must be green on all four legs, and the last nightly must be green, quarantined tests included.

## Manual checks

Use a real laptop, a real phone and the release candidate from the dev release. For each case, start a Claude Code turn and a Codex turn that take at least a minute (for example "run the test suite, then summarise it"), and leave one approval pending in a third session. Run `ox host doctor` after each case; it must end with "OX host is healthy" apart from expected warnings.

Write the result of each line as pass, fail (with what happened) or skipped (with why).

### Fresh install (macOS and Linux)

- [ ] On an account that has never run OX: `curl -fsSL https://raw.githubusercontent.com/n0rpan/ox/main/install.sh | sh -s -- --desktop`, then `ox desktop .`. The app opens and the host starts.
- [ ] `ox host install`. Log out and in: `ox host status` says running and installed.
- [ ] Pair the phone (README, "Pair a phone"). Run one Claude and one Codex session from the phone and from the desktop.

### Upgrade

- [ ] With a long turn running, rerun the installer. The installer says it is restarting the host; the turn finishes; the phone shows the turn's result without re-pairing; `ox host doctor` shows the new version.
- [ ] Repeat with a turn that outlasts `ox host restart --timeout 30`. The turn is reported as interrupted, and the installer output says how many turns were interrupted.

### Sleep

- [ ] Close the laptop lid mid-turn for 5 minutes, then open it. Expected: the turn either completes or fails with the client's network error. The session is not stuck in running; a new turn starts. The pending approval is still answerable from both devices.
- [ ] The phone reconnects on its own within 30 seconds of the laptop waking.

### Reboot

- [ ] Reboot mid-turn with an approval and a YOLO worker request pending. After login (login service installed): the host is running, the interrupted sessions show "interrupted", the approval is gone (never replayed), the YOLO request shows "declined" with a reason that says the host restarted, and reopening each session shows its native history.
- [ ] Without the login service: opening OX Desktop starts the host.

### Lost network

- [ ] Turn Wi-Fi off mid-turn for 2 minutes, then on. Expected: the stock client retries or the turn fails with a network error; the session is not stuck; the next turn works. The phone reconnects.
- [ ] With the network off, answer a pending approval on the desktop. It reaches the runtime.

### Lost Tailscale

- [ ] Stop Tailscale on the laptop mid-turn (`tailscale down`), wait a minute, `tailscale up`. Expected: the turn continues on the laptop; the phone shows disconnected, then reconnects and catches up; no approval is answered twice.
- [ ] `ox host doctor` while Tailscale is down: remote access is reported as a blocking problem with the fix.
- [ ] Turn on Funnel for port 8443 (`tailscale funnel 8443 on`): doctor reports it as blocking and the phone's requests are refused. Turn it off again.

### Real tailnet

- [ ] Run [tailnet-check.md](tailnet-check.md) end to end and attach its results table.

### Uninstall

- [ ] `ox host uninstall`: the login service is gone, sessions and pairings are kept (`ox desktop .` still shows them).
- [ ] `ox host uninstall --purge`: OX host state is gone, `~/.codex` and `~/.claude` are untouched.

## Cutting the release

1. Merge to `main` and wait for `CI` (which calls the four-leg `OX Desktop` build) to publish the dev release.
2. Write the release notes in `docs/releases/vX.Y.Z.md`: what changed for users, upgrade notes, known issues with their issue numbers.
3. Tag: `git tag -a vX.Y.Z -m "OX vX.Y.Z" && git push origin vX.Y.Z`. The tag runs `CI`, which builds all four CLI and desktop legs, including macos-26-intel, and publishes the release.
4. Paste the notes into the GitHub release, then install with the plain installer command on a clean machine and run `ox host doctor`.
