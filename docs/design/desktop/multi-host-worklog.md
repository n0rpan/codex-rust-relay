# Multi-host implementation handoff

Worktree `/home/oscar/Work/ox-multi-host`, branch `feat/multi-host`, based on `f44befe`. Oscar and the implementer will review the tradeoffs together before opening a PR.

## Decisions for review

- A host's UUID lives in a private, atomically created `host-id` file. An older host can rewrite its own ledger without dropping the UUID. Renaming the machine or restarting OX preserves its identity. Purging the state creates a new identity. Older hello frames can omit the UUID.
- The QR code and pasted invitation keep the existing long secret. The manual code uses twelve case-insensitive base32 characters, displayed in groups of four. Typing twelve characters costs more than a six-digit PIN. I think that cost is worth it for a credential that can enroll a device able to approve commands. All three forms spend the same single-use invitation and require the invitee's Tailscale login.
- Headless pairing and desktop Settings use the same Serve checks, listener reservation and confirmation. A public Funnel or another application's mapping on port 8443 stops pairing. I think requiring the owner to fix that mapping is worth the friction because a new device can control sessions.
- Device class is stored at registration. The authenticated bridge supplies the surface to the host on each connection. Existing registrations default to phone. Local argv handoffs and the remote terminal grant are distinct permissions.

- A desktop can explicitly copy selected provider and Brave Search keys to a paired host. The review names the target, scopes, possible charges, and continued use after revocation. Replacing existing keys requires selecting them. Copies retain their provider endpoint and the host's environment takes precedence.
- Managed headless installations default to automatic updates at idle. The operator can switch to download-only or manual and select Dev or Stable. An active turn, pending approval or active remote terminal delays installation. I think this wait is worth preserving live work.
- Wave 1 used Claude Opus 5.5 via OX on the subscription. Its findings prompted the separate host ID file, an older-host pairing error and prefix/label revocation. One invitation can still enroll either operator device. A host-to-host link grants different capabilities and needs separate enrollment. The read-only worker's approval prompt for returning findings is tracked in [#26](https://github.com/oxstack/ox/issues/26).

## Acceptance ledger

| Milestone | Evidence still needed |
| --- | --- |
| 1. Identity and first remote host | Real native tailnet pairing, macOS credential storage, terminal and Codex/Claude turns through reconnect. Linux first-frame, signed gateway, attachment bytes and adjacent hello fixtures pass. |
| 2. Multiple hosts | Live multi-machine acceptance, sidebar and palette integration, host labels, repository matching. Host-keyed inbox, offline catalog, stored project and client-only window implemented. |
| 3. Host policy | Host enforcement on session opens, changes, workers and handoffs, signed settings and CLI, asks on all clients, stamps and ceiling tests |
| 4. Resources | Host metrics and live subscription, UI, agent listing, placement evidence |
| 5. Linked read workers | Explicit pairing, verified caller, tracked bundle and untracked transfer, worktree, calls and durable notices through outages |
| 6. Write results and placement | Branch, ref and tailnet delivery, fallback, cleanup, requirements and project rules, diff stat and untouched parent checkout |
| 7. Conversation continuity | Cross-host handoff and thread reads, with source identity preserved |

## Verification

Verification entries below refer to the worktree above. Real-machine macOS/Linux and authenticated runtime scenarios are pending.

- The CLI settings integration test sent a synthetic provider and Brave Search key between two isolated homes. It covers the active environment credential, atomic batch rejection, a changed destination revision, scope mismatch, and 0600 file permissions.
- The Linux update smoke started the real host and CLI in a temporary managed package with a synthetic staged release. It verified the headless Automatic default, Dev/Stable selection, idle reservation, cancellation, and an in-place exec on the same PID with the old generation available for recovery. No production account, package, or service was used.
- Root workspace tests passed with a clean `PATH=/usr/bin`. The normal developer PATH exposes two copies of each stock CLI, causing two existing strict doctor fixtures to report warnings.

- Wave 2 was reviewed by Claude Opus 5.5 on the Claude subscription (OX job 1084535274409465), with a direct final reply. Changes after review bind channel selection to the installed generation, let a host recover from malformed preferences, reject conflicting provider scopes before any write, leave environment-shadowed keys unselected, and move update administration off the async listener. Running an explicit desktop channel switch can offer the older installed channel's release to an existing window; reopening should follow the operator's installation.


- The browser acceptance fixture enrolled and authenticated a synthetic device through a private TLS Serve proxy, imported a synthetic Brave Search key, rejected a stale batch, and denied signed remote export. Reading settings after import returned metadata only.
- A nested Wayland compositor rendered the desktop with a disposable host. A separate client-only smoke opened its window and created neither a local host socket nor a host ID. Compositor packages were verified with the Arch Linux ARM package signing key and extracted in a temporary directory.
- Remote image drops upload content from the selected desktop file using the same four-image / 6 MiB combined budget as the phone. Local drops still use local paths. The inbox groups by host and uses catalog request thread IDs, so approvals are answered on the owning host with the existing ask token. A three-catalog fixture gives every host the same project and thread ID and verifies that one disconnection preserves its own rows while the others continue receiving updates.
- Remote projects and worker navigation indexes belong to each host view. On reopening a project the desktop re-registers its path with the host, including after a restart. Desktop pairing can open a folder on a fresh host before the first runtime sign-in.
- Wave 3 was reviewed by Claude Opus 5.5 on the subscription (OX job 3784552356265470). It returned its findings directly. Remote slash commands now use the host-validated native command path. An offline or revoked host can be forgotten locally, and failed enrollment attempts try to revoke the new registration. Terminal status preserves the host's reason for refusal.
- Plain pairing keeps shell access off. I think that distinction is worth keeping because a terminal can operate outside a project. On a headless host, the owner can use `ox host pair --terminal` to include shell access and subscription sign-in in the initial invitation. This saves a second trip to the server. Invitations minted remotely still carry the ordinary device grant.
