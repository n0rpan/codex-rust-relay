# OX

**Run stock Codex and Claude Code side by side, on your subscriptions or any model API, from your desk or your phone.**

OX does not replace either client. It starts the official `codex` and `claude` CLIs, points them at the billing route you pick, and keeps their sessions running in a small per-user host so you can close the window, follow a turn from your phone, and pick it up again. Plain `codex` and `claude` keep working exactly as before.

## Install and first run

You need macOS 11 or later, or Linux with glibc 2.39 or later (x86-64 or ARM64). One command on both:

```sh
curl -fsSL https://raw.githubusercontent.com/n0rpan/ox/main/install.sh | sh -s -- --desktop
cd ~/Work/my-project && ox desktop .
```

OX Desktop opens on the project folder you pass (launched from the application menu it opens your home directory and offers **Choose project folder…**). On first launch it checks which official CLI is installed and signed in, and starts on that one. OX runs the official `codex` and `claude` CLIs on your computer, also for API models, so install at least one:

| You have | Install the CLI | Then |
| --- | --- | --- |
| ChatGPT plan | `npm install -g @openai/codex@latest` (Codex 0.171.0 or later; the same command upgrades it) | `codex login`, or **Sign in** in the app |
| Claude plan | `curl -fsSL https://claude.ai/install.sh \| bash` (Claude Code 2.1.331 or later; `claude update` upgrades it) | `claude`, then `/login`, or **Sign in** in the app |
| Any model through an API key | One of the two CLIs above | `ox setup` (Anthropic, OpenAI, OpenRouter or a local server), then pick the model in OX |

The desktop can install or update Codex for you from its first-run card, and links to the Claude Code guide. If a CLI is missing or signed out, the card also lets you switch to the other one or add an API provider. Details: [Codex](https://developers.openai.com/codex/cli/), [Claude Code](https://code.claude.com/docs/en/quickstart).

### macOS notes

The installer verifies the download's SHA-256 checksum, installs `ox`, `ox-desktop` and `ox-host` in `~/.local/bin`, and puts **OX Desktop** in `~/Applications`. If your shell cannot find `ox`, add the line the installer prints to your shell profile. If Gatekeeper asks, allow the app in **System Settings → Privacy & Security**.

### Linux notes

The same installer adds **OX Desktop** to your application launcher. It runs on Wayland or X11 with a Vulkan driver.

### First session

1. **Connect.** The first-run card has a Codex, Claude Code and API key switch. It checks again by itself while you install or sign in. Later, choose the runtime and billing route in the header. To add an API model, open **Settings → Accounts → Configure provider**, or run `ox setup`. A saved provider becomes the active route.
2. **Send a prompt.** Choose **New session**, type, press Enter. Existing Codex and Claude Code sessions from your terminal appear in the sidebar.
3. **Keep it running.** Run `ox host install` so the session host starts at login. Close the window during a turn; reopen it and the turn is still there.
4. **Check the setup.** `ox host doctor` checks the host, Tailscale Serve, paired devices and the stock client versions, and names the fix for anything that blocks.

## What OX is

- **The relay (`ox`).** One small binary between the stock clients and your model provider. Add a provider key once and both clients get the same models, picker and web search. Any model in either client: a Claude model in Codex, a GPT model in Claude Code, OpenRouter or a self-hosted model in both. [Relay and CLI](docs/relay.md)
- **OX Desktop (`ox-desktop`).** A native Linux and macOS workbench for Codex and Claude Code sessions across projects and billing routes: ChatGPT or Claude subscription, or an OX API alias. [Desktop guide](desktop/README.md)
- **The session host (`ox-host`).** A per-user background process that owns every session, its approvals and its workers. Closing the desktop does not stop a turn. It starts on demand, or at login with `ox host install`. [Host design](docs/design/desktop/host-notes.md)
- **The phone.** A web app served by the host over your private Tailscale network. Pair it with a QR code, then follow turns, answer approvals and questions, start sessions and get notifications. Nothing is exposed to the internet: OX refuses Tailscale Funnel.
- **Workers.** A session can start a background Codex goal, Claude Code workflow or plain session as a separate job, on an API model or a subscription. Workers appear under their parent on both screens. Without the session host, workers start read-only unless the main agent asks for write, and classifier or YOLO workers are refused.
- **Handoffs.** Continue a conversation in another runtime, billing route or model (`/handoff claude:<alias> …` or **Continue in…**). The new session receives the real recent messages, not a summary, and can read the rest of the source.
- **One mode model.** Every permission preset maps to one of four levels: read (Codex Read only, Claude Plan), write (Workspace, Accept edits), classifier (Auto-review, Auto) and YOLO (YOLO, bypass permissions). Workers and handoffs start at their source's level or lower. Agents can lower a child's mode but never raise it. A YOLO child waits for you to accept it on a paired device, unless you stored a grant. [Mode model](docs/design/desktop/host-notes.md#permission-modes-for-workers-and-handoffs)

### Pair a phone

1. Install Tailscale on the computer and the phone and sign in to the same tailnet with MagicDNS and HTTPS enabled.
2. On the computer, open **Settings → Remote access → Enable Tailscale Serve**, then **Pair a device**.
3. On the phone, open the HTTPS address shown, add it to the Home Screen (iPhone: Safari → Share → Add to Home Screen), open it from the icon, tap **Scan pairing QR** and scan the computer's screen.
4. Turn on **Session notifications** for approvals, questions and finished turns.

[Phone setup in detail](desktop/README.md#keep-sessions-running-and-pair-a-phone)

### Terminal only

If you only want the relay in your terminal, leave out `--desktop`:

```sh
curl -fsSL https://raw.githubusercontent.com/n0rpan/ox/main/install.sh | sh
ox setup
ox codex      # or: ox claude
```

[CLI guide](docs/relay.md)

## Upgrade

Rerun the install command you used. For OX Desktop it updates the app, host and CLI together. If the session host is running, the installer restarts it on the new version without cutting work short: running turns finish first (up to 10 minutes), new turns wait a moment for the new host, and the desktop and paired phones reconnect on their own. Sessions, pairings and settings are kept. `ox host restart` does the same by hand; `ox host doctor` shows which version is running.

The terminal-only CLI updates itself with `ox update`. Add `--dev` to the install command (`sh -s -- --dev --desktop`) for the rolling development build of `main`.

To remove the login service, run `ox host uninstall`. It keeps your sessions and pairings. `ox host uninstall --purge` also deletes OX's host state. Neither touches Codex or Claude Code sessions or sign-ins.

## When something is wrong

| What you see | What to do |
| --- | --- |
| Anything in the desktop or phone | `ox host doctor`. Each FAIL line ends with the fix. |
| Provider, key or model errors | `ox doctor`, then the [relay troubleshooting table](docs/relay.md#troubleshooting). |
| "The running OX session host speaks protocol …" | `ox host restart`. The desktop and host versions differ. |
| The phone says the host is restarting for an update | Wait a few seconds and send again. |
| `codex` or `claude` is below OX's floor | Update the stock client; `ox host doctor` prints the command. |
| Remote access fails after a Tailscale change | Turn Remote access off and on in Desktop settings. If doctor reports Funnel, run `tailscale funnel 8443 off`. |

## Documentation

| Topic | Page |
| --- | --- |
| Relay, CLI, speed, release channels | [docs/relay.md](docs/relay.md) |
| Providers, models, search, API workers, install options | [docs/configuration.md](docs/configuration.md) |
| Desktop features, keyboard, building from source | [desktop/README.md](desktop/README.md) |
| Session host, workers, handoffs, modes, remote access | [docs/design/desktop/host-notes.md](docs/design/desktop/host-notes.md) |
| Home-server gateway for other machines | [docs/shared-gateway.md](docs/shared-gateway.md) |
| Supported clients and API boundaries | [docs/compatibility.md](docs/compatibility.md) |
| How releases are checked | [docs/verification.md](docs/verification.md), [release checklist](docs/design/desktop/release-checklist.md), [real-tailnet check](docs/design/desktop/tailnet-check.md) |
