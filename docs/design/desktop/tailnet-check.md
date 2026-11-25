# Remote access: real-tailnet check

The automated tests cover the remote access rules in [OX session host](host-notes.md#remote-access-and-privacy) without a browser or a tailnet. This procedure repeats those checks on a real tailnet with a real phone. Run it before a release that changes `desktop/ox-host-web`, `desktop/ox-desktop/src/platform/remote.rs` or the Tailscale setup steps.

## Automated coverage

Each rule has one automated test. Run them with `cargo test --manifest-path desktop/Cargo.toml --workspace` or `cargo nextest run`.

1. Loopback-only listener: `ox-host-web` `server::tests::listener_binds_loopback_whatever_the_configured_port`
2. Identity headers only via Serve: `ox-host-web` `server::tests::identity_headers_count_only_on_the_serve_path_and_never_alone` (with `server::tests::serve_identity_from_a_non_loopback_peer_or_foreign_origin_is_rejected`)
3. Invitation and pairing expiry: `ox-host-web` `state::tests::pairing_invitations_and_unused_pairings_expire_on_a_pinned_clock`
4. Revocation cuts the live socket: `ox-host-web` `server::tests::revoking_a_device_closes_its_live_socket_immediately`
5. Funnel refused: `ox-desktop` `platform::remote::tests::funnel_in_serve_status_fixture_refuses_remote_access` (fixture `desktop/ox-desktop/tests/fixtures/serve-status.json`)
6. Push payloads hold only thread, title and kind: `ox-host-web` `push::tests::push_payloads_carry_only_scoped_thread_title_and_kind`

## Prerequisites

- The host computer runs OX Desktop and `ox host`. `ox host status` reports it running.
- Tailscale is signed in on the host and the phone, under the same user. MagicDNS and HTTPS certificates are enabled for the tailnet.
- A second tailnet machine (a laptop is fine) for requests from another node.
- A way to reach the host from outside the tailnet: the phone on cellular with Tailscale turned off works.
- Remote access is enabled in OX Desktop, Settings, Remote access, and the phone is paired and has notifications turned on.

Below, `HOST` is the host's MagicDNS name, for example `work.tail1234.ts.net`, and `TSIP` is its tailnet IPv4 address from `tailscale ip -4`. OX uses HTTPS port 8443 on the tailnet and port 48162 on loopback.

Record before starting:

```sh
git rev-parse HEAD          # or the OX release version
tailscale version
tailscale serve status --json
```

## 1. The listener binds to loopback only

On the host:

```sh
ss -ltnp | grep 48162                          # Linux
lsof -nP -iTCP:48162 -sTCP:LISTEN              # macOS
tailscale serve status
```

Expected: one listener on `127.0.0.1:48162`. Nothing on `0.0.0.0`, `[::]` or `TSIP`. Serve status shows `https://HOST:8443` proxying to `http://127.0.0.1:48162`.

From the second tailnet machine:

```sh
curl -m 5 -sS http://TSIP:48162/
```

Expected: connection refused or timeout.

## 2. Identity headers count only on the Serve path

On the host, send a forged identity directly to the loopback port:

```sh
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:48162/ \
  -H 'Tailscale-User-Login: you@example.com'
```

Expected: `403`. The Host header is not the Serve hostname.

From the second tailnet machine, which is not paired, call the API through Serve:

```sh
curl -s -o /dev/null -w '%{http_code}\n' https://HOST:8443/api/challenge \
  -H 'Origin: https://HOST:8443' -H 'Content-Type: application/json' \
  -d '{"purpose":"connect"}'
```

Expected: `403`. Serve attaches a real login header, but without a paired device cookie it grants nothing. Repeat with an extra `-H 'Tailscale-User-Login: someone-else@example.com'`. Expected: still `403`.

## 3. Pairing invitations expire and are single use

1. In OX Desktop, Settings, Remote access, create a pairing code. Wait six minutes. Scan it from a second browser profile or device on the tailnet. Expected: pairing fails.
2. Create a fresh code and pair one browser with it. Scan the same code from a second browser. Expected: the second pairing fails.
3. Unused pairings expire after 30 days. The unit test covers the boundary. To check it by hand, stop the host (quit OX Desktop; if the host runs as a login service, run `ox host uninstall` and `ox host install` again afterwards), then in `~/.local/state/ox/host/remote.json` (macOS: `~/Library/Application Support/ox/host/remote.json`) set one device's `last_used` to a Unix time 31 days ago. Keep the file mode `0600`. Start the host again. Expected: the device is gone from the Desktop list and that browser is asked to pair again.

## 4. Revoking a device cuts its live connection

1. Open a session on the paired phone and leave the screen on.
2. In OX Desktop, Settings, Remote access, revoke the phone.

Expected: within about a second, without touching the phone, the session stops updating and the phone shows it is disconnected or asks to pair again. Reloading shows the pairing screen. If a terminal was open on the phone, it closes too.

## 5. Funnel is refused

On the host, publish the OX port through Funnel:

```sh
tailscale funnel --bg --https=8443 http://127.0.0.1:48162
tailscale serve status --json        # shows "AllowFunnel": {"HOST:8443": true}
```

Expected:

- OX Desktop, Settings, Remote access shows "Funnel is enabled on HTTPS port 8443. Remove public sharing on that port in Tailscale before enabling OX." Enable and Pair fail with that message.
- From outside the tailnet (phone on cellular, Tailscale off): `curl -i https://HOST:8443/` returns `403` with a body saying OX remote access is private to the tailnet.

Clean up:

```sh
tailscale funnel --https=8443 off
tailscale serve status
```

If the Serve mapping is gone, enable Remote access again in OX Desktop.

## 6. Notifications carry no content

1. On the host, start a session and ask it to run a command that needs approval and contains a marker, for example `echo OX-PUSH-MARKER-4821 > marker.txt`.
2. Lock the phone before the approval arrives.
3. Let a second session finish and make a third one fail, so that the approval, finished and failed notifications all arrive. Ask a question through the agent for the question kind.

Expected: each notification shows only the session title (or a generic label) and the event kind. The marker, the command, file names and diff text never appear on the lock screen or in the notification centre. Tapping a notification opens that session after the phone reconnects.

## Recording results

Add a row to the table below in the same commit as any fix. Use `pass`, `fail` or `skipped`, and give the reason for anything other than `pass` in Notes.

| Date | OX commit | Tailscale | Host OS | Phone | 1 | 2 | 3 | 4 | 5 | 6 | Notes |
| ---- | --------- | --------- | ------- | ----- | - | - | - | - | - | - | ----- |
