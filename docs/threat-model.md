# OX threat model

This is the baseline for security review of OX. Read it before reporting a
finding. `SECURITY.md` has the rules for reviewers; this file describes the
system those rules apply to.

## What OX is

OX runs stock Codex and Claude Code for one person on machines that person
owns. It is a local relay to model providers, a desktop workbench, a session
host that keeps sessions running, and a phone and multi-host view reached over
the owner's Tailscale network.

OX is a power tool. Its user is a developer who chose to give an agent a shell.
**The user may do anything to their own machine, through OX or through the
agent, and that is the product working.** OX does not protect the user from the
model they chose, from their own instructions, or from the stock clients'
behaviour. Stock Claude Code and Codex in a terminal are the baseline: OX being
as permissive as they are is never a vulnerability.

What OX must not do is act against its user on its own: leak a secret, send
data somewhere the user did not choose, let someone who is not the user in, or
install code that is not ours.

## Who is trusted

| Party | Trust |
| --- | --- |
| The local user, and any process running as that user | Fully trusted. A same-user process can already read the user's files and keys. |
| The model and the agent | Trusted as much as the user's chosen permission mode says. Their actions are the user's actions. |
| A paired device (phone, desktop, worker host) | Trusted as the user, within the grants the user gave that device. |
| The user's Tailscale network | Trusted to deliver the identity of the tailnet user. Not trusted alone: a request also needs a paired device's cookie and signature. |
| The model provider a key was saved for | Trusted with that key and the prompts sent to it. |
| Other local users, other tailnet users, the public internet | Not trusted. |
| GitHub releases of `oxstack/ox` | Trusted as the source of updates, together with the signature check on macOS. |

## What we protect

1. **Provider API keys and subscription sign-ins.** They cost money and give
   access to the user's accounts.
2. **Pairing credentials and device keys.** They are the user's identity
   towards a host.
3. **The update channel.** It installs code.
4. **Prompts, replies, file contents and terminal output.** They leave the
   machine only for the provider the user chose, or a host the user paired.
5. **The release signing keys**, which live in the CI environment
   `apple-release` and nowhere else.

## The boundaries, and what holds them

### Provider keys

- Saved keys are in `ox-api-keys.json` under the Codex home, mode 0600, owned
  by the user, opened without following links. Each key is bound to the
  provider URL it was saved for and is sent only there.
- Keys are taken out of the stock client's environment. Error text that goes
  back to a client has secrets replaced. OX has no telemetry and keeps no log
  of prompts or replies.
- The phone and web views see a key's name and whether it is set, never its
  value. They can replace a key; they cannot read one.
- Known limit: a key supplied through an environment variable is not bound to
  a URL. It is used for whatever URL the provider entry names.

### Copying keys to another host

- Only a paired operator device can start it, with a signed request. The user
  reviews the destination, the key names and what would be replaced, and the
  send is refused if anything changed since the review.
- A destination added from the web view must be an untagged machine of the
  same Tailscale user. A host already paired from the desktop can be chosen
  without that check: the paired operator is trusted to pick the recipient.
- The receiver can use the keys and incur charges. That is stated to the user.

### Pairing and devices

- An invitation is a one-time code, valid for five minutes, bound to the
  Tailscale login that created it. The device proves a P-256 key and gets a
  host-only, HTTP-only cookie. Sensitive requests are signed with a fresh
  single-use nonce.
- Device keys are non-extractable in the browser, in the Secure Enclave or
  Keychain on macOS, and in Secret Service or a 0600 file on Linux.
- Revoking a device removes its record and closes its live sockets and
  terminals. Phone and desktop pairings expire after 30 days unused.

### What is reachable from where

- The relay, the gateway and the host web server listen on loopback only.
  Sockets are 0600 and check that the peer is the same user.
- The host web server is published only through Tailscale Serve. OX refuses
  Tailscale Funnel, so nothing is reachable from the public internet. Requests
  must carry the Serve origin and exactly one Tailscale login.
- The relay uses a random token for each launch. The shared gateway uses one
  static key for all its routes; who can reach it is the tailnet's decision.

### What a paired device can do

- Send prompts, stop, fork and rename sessions, answer approvals, change a
  session's permission mode, and start workers and handoffs.
- Open a terminal on the host only if the user granted that device terminal
  access, which is off by default. Each terminal start is previewed and
  confirmed.
- Change host settings with a signed request.

A paired device doing any of this is the user acting remotely.

### Permission modes

- Modes are read, write, classifier and YOLO, mapped to the stock clients' own
  presets. OX adds no restriction to a session beyond the stock client's.
- A worker or handoff never gets more than the session that started it. Only
  the operator can raise one, and never above its recorded ceiling.
- Starting a worker in YOLO needs the operator's confirmation, a standing
  grant, or a host policy the user set. Guards of this kind are settings.

### Updates and installation

- Downloads come only from GitHub's release hosts, and are checked against the
  published SHA-256.
- On macOS a build signed with our Developer ID accepts only updates and
  installer packages signed by the same team. `OX_ALLOW_UNSIGNED=1` or
  `--allow-unsigned` turns that off; it is the user's choice.
- Known limit: on Linux the only check is the SHA-256, which comes from the
  same place as the download.

### Web Push

- A push carries a thread reference, the session title (at most 80
  characters) and the kind of event, encrypted for the subscription. It never
  carries commands, message text, tool arguments or diffs.
- Push endpoints are limited to Apple's, Google's and Mozilla's services.

### The release pipeline

- The signing certificate and the App Store Connect key are in the
  `apple-release` environment. Only the two signing jobs enter it, and those
  jobs run no third-party code.
- Which branches and tags may enter that environment, and who may create `v*`
  tags, are repository settings.

## Not threats

Do not report these. They are the product, or a decision already made.

- The agent or the user runs a destructive or dangerous command.
- OX does not ask for confirmation where stock Claude Code or Codex would not.
- A session runs in YOLO because the user, or a host policy the user set,
  chose it.
- A process running as the same local user reads OX's files, sockets or keys.
- A paired device, within its grants, controls sessions or changes settings.
- The user copies keys to a host they paired, or turns off the signature rule.
- The shared gateway has one key for everyone who can reach it.
- Prompts and files go to the provider the user configured.
- A provider key in a 0600 file is not encrypted at rest.
- Session titles are visible to the push service in encrypted form.

## Threats

Report these.

- A provider key, a subscription token, a pairing token, a device key or the
  gateway key reaches a log, an error message, a client's environment, the
  phone or web view, a push payload, or any host other than the one it was
  saved for.
- A key bound to one provider URL is sent to another URL, including after a
  settings change made from a paired device.
- Anyone who is not the user's Tailscale login with a paired device reaches a
  session, a terminal or the settings. This includes a request arriving through
  Funnel, with a forged or duplicated identity header, or from another local
  user.
- A revoked or expired device keeps working, or a signed request can be
  replayed.
- A device gets a terminal without the terminal grant, or a worker gets more
  access than the session that started it.
- A worker or an agent answers its own approval or YOLO request, or reads
  another user's or another chain's sessions.
- An update or install accepts code that fails its checksum, or on a signed
  macOS installation code that is not signed by our team, without the user
  having set the override.
- A file holding a secret is created with group or other permissions, or
  through a link someone else controls.
- A workflow change lets a job that runs third-party code, or a pull request
  from outside, read the signing secrets or the auto-queue token.
- Prompts, replies, file contents or terminal output are sent anywhere the
  user did not configure.
