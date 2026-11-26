# OX Desktop

## Install or update the dev release

The rolling dev release is published after a successful build of `main`. Once its assets include `ox-desktop-linux-*` and `ox-desktop-macos-*`, the installer can choose the right package for Linux x86-64 or ARM64, Intel macOS or Apple Silicon. [Dev release and assets](https://github.com/n0rpan/ox/releases/tag/dev)

```sh
curl -fsSL https://raw.githubusercontent.com/n0rpan/ox/main/install.sh | sh -s -- --dev --desktop
ox desktop .
```

Run the launch command from the project you want to open, or pass a directory such as `ox desktop ~/Work/my-project`. The installer verifies the archive checksum, installs `ox`, `ox-desktop` and `ox-host` under `~/.local/bin` by default, and adds **OX Desktop** to your Linux launcher or macOS **~/Applications**. Follow the installer's PATH hint if your shell cannot find `ox`.

For an update, run the same installer command again. `ox update --dev` updates the standalone CLI. The desktop installer updates the whole package and keeps existing settings and native histories. If the session host is running, the installer restarts it on the new version: running turns finish first (for up to 10 minutes), new turns wait for the new host, and connected desktops and phones reconnect on their own. `ox host restart` does the same by hand. Check installed versions with `ox --version`, `ox-desktop --version` and `ox-host --version`, and the running host with `ox host doctor`.

## First run

On a fresh install, OX Desktop looks for the official `codex` and `claude` CLIs and a local sign-in for each (it only checks that the files exist). It starts on the route that is ready: Claude Code if only Claude is signed in, Codex if only Codex is, your default (Codex) if both are, or the first saved API model if neither is. If something is missing, the card in the middle shows the next step: a Codex, Claude Code or API key switch, then **Install**, **Update** or **Sign in**, plus **Configure an API provider** and, when the other CLI is installed, **Use installed … instead**. OX runs the official CLIs on your computer, also for API models. While you install or sign in, the card checks every couple of seconds and opens the workspace by itself. **Check connection** is the fallback. Saving a provider switches the window to its first new model. When the project is your home directory, **Choose project folder…** opens the folder picker.

## Open your first session

Install the stock CLI for the routes you want to use. The supported floors are Codex 0.171.0 and Claude Code 2.1.331. Open an existing thread from the sidebar, or choose **New session** and send a prompt with Enter.

Open a project and choose a runtime and billing route in the header. Codex can use your ChatGPT subscription, Claude Code can use your Claude subscription, and either runtime can use an OX API alias. Each alias uses its configured provider account. The sidebar lists saved Codex and Claude conversations across projects and billing routes. Open **Commands → Refresh sessions** to pick up conversations created in a terminal while Desktop is open. Switching projects keeps running turns and approvals attached to their originating project. Select a session to resume its thread in the original profile.

In **Settings → Accounts**, sign in through stock Codex or Claude Code, or choose **Configure provider**. Pick a service or **Custom**, enter its endpoint and protocol, and use the **Models** section for static model IDs and capability overrides. **Save provider** opens **API keys**, where you can enter, rotate or remove a destination-bound key. Configure Brave under **Web search**. Exported keys take precedence. **Starred models** sets only the order: starred models appear first, and any model can be used. If Codex selects a legacy custom provider, its tab lets you edit the model and relay connection in place. Changes apply to new client launches. For a new Claude subscription profile, choose **Sign in with Claude** and complete the stock client’s sign-in. Return to the workbench to connect.

Use **Test saved connection** to list coding models at one provider’s endpoint. **Advanced configuration** opens text editors for the provider registry, favorites and the selected legacy connection. Private and unrecognized registry fields stay in the original file. A changed-on-disk notice keeps your unsaved text or native form edits available for review and retry.

## Keep sessions running and pair a phone

Desktop starts the packaged OX session host on first use. Close and reopen the window to reconnect to an active turn or pending request. Run `ox host status` to see whether the host is running, the local socket and state paths, and whether it starts at login. Run `ox host install` to start it at login through systemd on Linux or launchd on macOS. Use `ox host uninstall` to remove that login service; sessions, pairings and Codex and Claude Code data stay. `ox host uninstall --purge` also deletes OX's host state. `ox host doctor` checks the host, Tailscale Serve, paired devices and the stock clients, and names the fix for anything blocking.

1. Connect the laptop and phone to Tailscale. In **Settings → Remote access** on the laptop, select **Enable Tailscale Serve**. If Serve asks for administrator access, choose **Run in OX terminal**, check the command and run it. Type your password at the terminal prompt. OX checks the resulting mapping before it offers a pairing QR.
2. Open the private HTTPS Serve address on your phone. On iPhone, use Safari’s **Share → Add to Home Screen**, enable **Open as Web App** if shown, then open OX from its icon. The Home Screen app has its own device key. If you already paired in Safari, create a fresh pairing QR on the laptop. [Apple’s Home Screen instructions](https://support.apple.com/guide/iphone/iphea86e5236/ios)
3. Choose **Pair a device** on the laptop. On the phone tap **Scan pairing QR**, allow the camera, scan the laptop screen and name the device. You can also paste the pairing link. Each installation has its own entry in Remote access.
4. Open **Session notifications** and tap **Enable notifications → Allow**. Alerts cover approvals, questions, finished turns and failures. Tap an alert to open its thread. On iPhone this requires the Home Screen app and iOS 16.4 or later. If permission was denied, open iPhone **Settings → Notifications**, select the web app’s name and turn on **Allow Notifications**. [WebKit’s Web Push guide](https://webkit.org/blog/13878/web-push-for-web-apps-on-ios-and-ipados/)

The phone and desktop show live sessions across projects. The phone groups consecutive tool calls into one collapsed row. Tap the count, then a specific call, to read its command and output. Assistant replies render headings, lists, tables and code inline. Start a thread on either device, then open its row on the other to follow the same reply, answer an approval, steer a worker or press Stop. The session settings use the runtime’s confirmed mode, including YOLO. Your selection on one screen stays independent of what you open on the other. Device revocation and disabling remote access take effect on open connections. [Tailscale Serve](https://tailscale.com/kb/1312/serve)

On the phone, **Sessions → ••• → Settings** opens the host’s **Accounts**, **Providers & models**, **Credentials**, **Web search**, **Favorites**, and **Advanced configuration**. Each save checks the revision you edited. A conflict leaves your changes in the form. Credential fields accept a replacement key and show whether a key is configured. **Phone appearance & input** and **Desktop appearance & input** let you choose which device to change. In **New session**, use **Browse laptop folders** to start in another project. Run `ox help settings` for the same map from the CLI.

**Terminal access is per device.** On the laptop, open **Settings → Remote access**, find the paired phone and select **Allow terminal access**. On the phone, choose **Sessions → ••• → Host terminal** or open **••• → Project terminal** in a session. Review the program and folder, then tap **Run**. Commands are visible as you type. When the terminal switches off echo for a password prompt, OX masks the field. **Show** and **Hide** change the display on your phone for that prompt. Input goes directly to the terminal and stays out of agent transcripts. Closing the terminal, revoking the phone, or choosing **Disable terminal access** on the laptop closes that terminal session. Existing and newly paired devices start with terminal access off.

To use the laptop terminal from the desktop, open the command palette and choose **Open project terminal**. A suggested shell command can also offer **Review in terminal**. Check the command and directory, then explicitly start it. A native approval handoff first declines the original tool action so it runs only once. Input and output belong to the terminal window.

## Appearance and controls

**Settings → Appearance & input** controls the theme source, text size, animation and Enter behavior. With **Enter** selected, Shift+Enter inserts a line break. Drag the sidebar or review divider to resize it. OX restores the window size, pane widths, open projects and selected thread on restart.

On Omarchy, OX reads the active desktop palette and follows theme changes while the window is open. A running pixel ox marks history loads. During a turn, the activity sprite changes as the agent thinks, writes, uses tools or runs a worker. Omarchy’s disabled-animations toggle or `OXA_REDUCED_MOTION=1` keeps the sprites still. Click a tool row or thinking summary to see its details. A code card shows the language and offers Copy.

The permissions picker in the composer applies to the selected Codex thread. Choose **Read only**, **Workspace**, **Auto-review**, or **YOLO**. YOLO grants full filesystem and network access and runs commands without approval. A burning ox marks that mode after Codex accepts it. Switch back in the same picker.

Subagents and OX workers appear beneath the session that started them. Click a child to see its activity, send an instruction, or stop it. Click the parent to return to the conversation. Saved OX jobs from a previous process show their recorded activity. Live OX controls reconnect through the session host.

## Keyboard and composer

Press **Enter** to send a message or **Shift+Enter** for a new line. Change this under **Settings → Appearance & input**. Select the token count below the composer to inspect the session’s context window, token breakdown and reported account limits.

Open the command palette with **Ctrl+K** on Linux or **Cmd+K** on macOS. Search for a thread, recent project, billing route or Codex command. Results include their project and route. Use the arrow keys and Enter to open a result, or Escape to return to your draft. [Keyboard reference](../docs/desktop/keys.md)

Type `@` and part of a filename to search the current project with Codex. Use the arrow keys to choose a path and Tab to insert it. Paste a PNG, JPEG, GIF or WebP from the clipboard, or drag an image file onto the composer. The attachment stays with the draft until the turn or steering instruction is accepted. If a model reports text-only input, choose another model or remove the image before sending.

The composer, command palette, and ordinary Settings text fields support native input-method composition. The credential control accepts directly typed or pasted printable ASCII API tokens. It masks the token and keeps it out of copy, cut and undo history.

## Claude Code sessions

Select **Claude subscription** or **Claude Code · OX API** to start a conversation. The workbench uses stock Claude Code 2.1.331 or newer. Each open Claude session has its own process. Send during a turn to steer it, use **Stop** to interrupt it, and use the request badge to answer a background approval. Expand a parent session to read its subagent activity. **Thread actions → Fork thread** opens an independent session on the same billing route.

The permission menu offers Claude’s Default, Auto, Plan, Accept edits, and **YOLO · bypass permissions** modes. Default follows your permission rules and asks when approval is needed. Auto lets Claude review eligible actions. If your configuration disallows Auto, the picker shows the reason and keeps the previous mode. Choose YOLO in the composer to skip tool approvals for that Claude session. The flame lights when Claude confirms bypass mode and stays static with reduced motion. A new draft, a resumed session and a fork each start in Claude’s default permission mode.

Type `/` for commands available from the installed client. Use `/terminal` to continue an idle session in the stock terminal, on the same route and in the same directory. OX reads stock’s session journals to display history. It stores API alias associations in its own desktop state directory. When an older journal has no recorded alias, select its original alias to resume.

## Codex commands

Type `/` in the composer to see commands for the installed Codex and the selected route. Use `/goal <objective>` to set a goal, `/goal` to view it, and `/goal pause`, `/goal resume`, or `/goal clear` to manage it. Use `/speed` to see service tiers offered by the active model. Selecting a tier applies it to subsequent turns in that thread. The menu also offers session actions such as `/new`, `/resume`, and `/compact`.

Use `/terminal` to open the stock CLI’s complete live command menu. For a command the desktop does not implement, **Copy command and open stock Codex** opens the same project, billing route and thread, then copies the command. Paste it into the native composer for Codex to interpret. Close that CLI session before reconnecting in the desktop.

## Work with a conversation

Native session names appear in the sidebar. For a new conversation without a native name, Desktop requests a short title from the same model through its current billing route. The opening message supplies the title prompt and the extra request uses that route’s allowance. To name an older unnamed conversation, open it and choose **Thread actions → Generate title**. Existing sessions are indexed without model calls.

Use **Thread actions** to rename, fork, archive, or search a conversation. A fork opens as a separate thread in the same project and billing route. Archive asks for confirmation. The changes pane groups patches by file and shows the plan for the selected turn. Use its copy actions to take a single patch or the full diff.

Right-click a message to copy its Markdown source or an individual code block. In **Find in transcript**, enter a phrase and move between matching rows with **Next** and **Previous**. The **Copy** menu also works from a search result. When the loaded history contains no match, **Next** continues into earlier pages. **Thread actions** also lets you expand or fold details and toggle thinking summaries.

## Local state

Codex owns the thread history, approval requests and ChatGPT session. The session host reads and updates them through the stock app-server. It connects each project and billing route to its own server so tool calls run in the selected directory. The OX companion handles provider configuration, saved API keys and OX workers.

OX stores its desktop state under `$XDG_STATE_HOME/ox/desktop` when that variable names an absolute directory. Defaults are `~/.local/state/ox/desktop` on Linux and `~/Library/Application Support/ox/desktop` on macOS. Workspace, layout and preferences use separate JSON files. Saved provider keys stay in OX’s credential store.

## Run a packaged build

Download the artifact for your OS and architecture from the [OX Desktop workflow](https://github.com/n0rpan/ox/actions/workflows/desktop.yml). Each archive includes the matching desktop app, session host and CLI. Use the artifact from the branch and commit you intend to test.

- On macOS, extract the ZIP and open **OX Desktop.app**. These builds are ad-hoc signed. If Gatekeeper requests confirmation, use System Settings → Privacy & Security → Open Anyway for the downloaded app. Terminal may request Automation permission for native sign-in or a terminal handoff.
- On Linux, extract the tarball and run `./ox-desktop/ox-desktop`. Keep all three executables in the extracted directory. Linux builds target glibc 2.39 or later and use Wayland or X11 with a Vulkan-capable driver. The tarball contains its shared library requirements in `shared-libraries.txt`.

Install stock Codex or Claude Code for each route you intend to use. A desktop launch searches your PATH, user-local binaries and the standard Homebrew locations. If you use a different installation directory, start the desktop executable from a shell with the CLI on PATH.

To install a checked workflow artifact locally, download and unzip the GitHub artifact for your platform. From this checkout, point the installer at the directory containing the native archive and its sibling `.sha256` file.

```sh
OX_INSTALL_BASE_URL="file://$PWD/desktop/dist" sh install.sh --desktop
ox desktop ~/Work/my-project
```

## Build from source

Install Rust 1.100 or later. On macOS, install Xcode and the Metal toolchain. On Linux, install the native development libraries listed in the [desktop workflow](../.github/workflows/desktop.yml). From the repository root:

```sh
desktop/package.sh
```

The host-native archive appears in `desktop/dist`. To run a development build, build the app, host and gateway in the same target directory:

```sh
cargo build --locked --bin ox --target-dir desktop/target
cargo build --locked --manifest-path desktop/Cargo.toml -p ox-host -p ox-desktop
./desktop/target/debug/ox desktop .
```

Run desktop tests with `cargo test --workspace --locked --manifest-path desktop/Cargo.toml`.

The desktop workflow attaches the matching performance probe alongside each application package. Use the probe from the same commit to reproduce the [performance measurements](../docs/design/desktop/gate-report.md). The [stock Codex checks](../docs/design/desktop/codex-notes.md) cover billing-route isolation, worker directories, approvals and session recovery.

To run the stock-client integration tests, point the fixture at your built OX and installed Codex binaries. For a mise installation, use `mise which codex` to resolve the binary outside the test home. The fixture runs in temporary homes and sends inference requests to local test servers.

```sh
OX_TEST_OX_BIN="$PWD/desktop/target/debug/ox" CODEOX_TEST_CODEX_BIN="$(command -v codex)" \
  cargo test --locked --manifest-path desktop/Cargo.toml views::workbench::tests::stock_session -- --ignored --exact
```

On Linux, `keyboard_window.py` starts a private Sway compositor, D-Bus session and fcitx5 Pinyin input method. It exercises the packaged GUI against stock Codex and a loopback provider. Install Sway, wtype, wl-clipboard, fcitx5 with Pinyin, D-Bus, busctl and the Vulkan driver used by GPUI.

```sh
python3 desktop/tests/keyboard_window.py desktop/target/debug/ox-desktop "$(mise which codex)"
```

Append `--gtk-drop` to drag a PNG from a GTK 3 window into the composer. Install the Python GObject bindings for GTK 3 and build `ox-desk-probe` beside the GUI. If the probe lives elsewhere, use `--gtk-drop /path/to/ox-desk-probe`.
