# Security

## Reporting a vulnerability

Open a private security advisory on this repository (Security → Advisories →
Report a vulnerability). Please do not open a public issue for something that
exposes a key or lets someone into another person's sessions.

## Guidelines for security review

These apply to every reviewer, human or automated. The system, its trust
boundaries and the full lists are in [docs/threat-model.md](docs/threat-model.md);
read it first.

**OX is a power tool for a developer on their own machines.** The user chose to
give an agent a shell. If the user or their agent wants to wipe the disk, OX
lets them, exactly as stock Claude Code and Codex in a terminal would. That is
not a finding, at any severity.

### Report

A change is a security finding when the **program itself** does one of these
without the user having chosen it:

- **Leaks a secret.** A provider key, sign-in token, pairing token, device key
  or gateway key reaches a log, an error message, a stock client's
  environment, the phone or web view, a push payload, or a host other than the
  one it belongs to.
- **Sends data somewhere the user did not configure.** Prompts, replies, files
  or terminal output go to a provider or host the user did not pick.
- **Lets someone else in.** A request without the user's Tailscale login, a
  paired device and a valid signature reaches a session, a terminal or the
  settings. A revoked device keeps working. A signed request can be replayed.
- **Gives more access than was granted.** A device opens a terminal without
  the terminal grant. A worker gets more than the session that started it. An
  agent answers its own approval.
- **Installs code that is not ours.** An update or install skips the checksum,
  or the signature rule on a signed macOS installation, without the user's
  override.
- **Weakens a file or socket.** A secret is written with group or other
  permissions, or through a link another user controls. A listener binds
  anything but loopback.
- **Exposes release secrets.** A workflow lets a job that runs third-party
  code, or a pull request from outside, read the signing secrets or the
  auto-queue token.

### Do not report

- A missing confirmation, warning, sandbox or limit on what the user or the
  agent may do. OX adds none by default. A guard, where we want one, is a
  setting the user controls.
- Anything stock Claude Code or Codex also allows.
- YOLO mode, or any permission mode, working as the user set it.
- A process running as the same local user reading OX's files or sockets.
- A paired device using the access it was granted.
- An override the user must set on purpose, such as `OX_ALLOW_UNSIGNED=1`.
- Keys stored unencrypted in a 0600 file owned by the user.
- The shared gateway's single static key.
- The known limits listed in the threat model, unless a change makes one
  worse.

### Before proposing a guard

If a finding's fix is "ask the user first" or "block this", say what
irreversible harm it prevents that the user did not choose. If there is none,
it is not a finding. If there is one, the fix comes with a setting to turn it
off.
