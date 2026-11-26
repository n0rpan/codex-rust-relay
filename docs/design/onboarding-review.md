# Onboarding review (Gemini 4 Argon via ox codex, dogfood)

First-run screenshot: fresh HOME/state, branch build, 2026-11-25. Note: OX found Codex via its known install path even with codex removed from PATH, so the not-installed states were reviewed from onboarding.rs.

### 1. The 10-Second Test: Fails
On first launch (`fresh.png`), a new user sees a pixel ox, **"Connect your account"**, *"Sign in with Codex to use your ChatGPT subscription"*, and a sidebar packed with dead controls (`Running`, `Needs you`, `Finished`, `RUNNING · 0`, `End all idle`, `Checking the session host...`).
- **What is missing:** There is zero mention of **Claude Code**, **local-first execution**, **persistent background sessions**, or **phone pairing**.
- **What it looks like:** A third-party ChatGPT wrapper asking for OpenAI credentials. A Claude Code or local/OpenRouter user launching OX for the first time will think they downloaded an OpenAI-only tool unless they notice the tiny, unlabeled `ChatGPT subscription ▾` text dropdown under the project name.

---

### 2. First-Time Setup Flow & Edge-Case Traps
First-run state is evaluated solely against `self.route.runtime` (which defaults to Codex + ChatGPT subscription), creating severe state traps:

1. **What the user must know/do in order:**
   - Realize `oscar ▾` is the active project folder (launched from `$HOME` if opened via desktop launcher!).
   - Know that `ChatGPT subscription ▾` is actually the **Runtime + Billing Route** selector, and switch it *before* following the center screen prompt if they want Claude Code or an API model.
   - Have a terminal/browser ready because sign-in and CLI installation spawn external processes and require manual `"Check connection"` polling.
2. **Neither CLI installed (`Issue::Missing`):**
   - Because the default route is Codex, the screen says **"Install Codex"** with Zero mention of Claude Code.
   - **Hardcoded bug in `onboarding.rs`:** When installing/updating *either* runtime via `platform::onboarding::repair`, the status message is hardcoded to `"Finish installing Codex in the terminal, then return here."`—even if the user is installing **Claude Code**!
   - Neither `Issue::Missing` nor `Issue::Old` renders the `"Configure an API provider"` button, blocking users from setting up keys first.
3. **Only Claude Code installed (Codex missing):**
   - **Trap:** OX boots into the default Codex route, checks `missing(Codex)`, and blocks the workspace with **"Install Codex"**. Even if `claude` is installed and authenticated, OX does not auto-detect it or offer a one-click *"Use installed Claude Code instead"* button.
4. **Neither CLI installed, or signed out, but an API key/provider exists:**
   - **Both CLIs missing + API key exists:** Still hard-blocked by `Issue::Missing`. The user isn't told that OX *drives* the stock `codex`/`claude` binaries even for API models (it is not a standalone HTTP chat client).
   - **CLI installed + API key exists in env/config:** Default route is still `ChatGPT subscription`, so the user hits `Issue::SignIn` (**"Connect your account"**). Worse, clicking `"Configure an API provider"` opens the provider modal, *not* a route switcher—if the user stays on the subscription route, they remain stuck on the sign-in screen.

---

### 3. Jargon & Copy Problems
- `"Install the stock {name} executable on this computer to open a session."` — *"Stock executable"* is compiler/maintainer jargon. Users say *"official CLI"*.
- `"Sign in with Codex to use your ChatGPT subscription."` — Mentally backwards. You sign in with your OpenAI/ChatGPT account *so Codex can run*.
- `"This project’s saved API route needs an OX provider connection."` — Dense internal architecture jargon (*"saved API route"*, *"OX provider connection"*).
- `"Checking the session host..."` / `"RUNNING · 0"` / `"End all idle"` — Exposed plumbing on an empty first-run screen before a single session exists.
- `oscar ▾` + `ChatGPT subscription ▾` — Unlabeled dropdowns. New users don't know `oscar` means `~/oscar` (working directory where agents can edit files) or that `ChatGPT subscription` switches between Codex and Claude Code.

---

### 4. The README as Onboarding
- **Architecture before action:** `README.md` opens with a 7-bullet architecture taxonomy (*relay, desktop, session host, phone, workers, handoffs, one mode model*) before showing the install command.
- **Unexplained strict version floors:** Demands `Codex 0.171.0` and `Claude Code 2.1.331` upfront without giving the one-line install/upgrade commands, nor reassuring users that OX Desktop can install/update them.
- **Home-directory hazard:** The README shows `cd ~/Work/my-project && ox desktop .`, while also stating the installer adds **OX Desktop** to the OS app launcher. Launching from the OS app launcher opens `$HOME` (`oscar ▾`) with no in-app onboarding prompt to pick a project folder.

---

### 5. Top 10 Concrete Changes (Ordered by Impact)

1. **Auto-select an installed/authenticated CLI on first launch (`Workbench` init + `first_run_issue`):**
   - *Change:* On fresh launch with no saved state, probe `codex` and `claude`. If `codex` is missing/logged-out but `claude` is installed, auto-switch the initial route to `Claude subscription` instead of blocking on `"Install Codex"`.
2. **Add an inline Runtime & Route Pill Switcher to the Onboarding Card (`onboarding.rs` top of `panel`):**
   - *Change:* Above the title in `onboarding()`, render a 3-way segmented control: `[ Codex ]  [ Claude Code ]  [ API Key ]` so users discover both engines and API routing immediately without hunting the top-left sidebar.
3. **Replace `"Connect your account"` header & copy (`onboarding.rs` `Issue::SignIn`):**
   - *From:* Title `"Connect your account"`, Note `"Sign in with {name} to use your {} subscription."`
   - *To:* Title `"Ready to run {name} locally"`, Note `"Authenticate the official {name} CLI with your {} account, or route it through your own API key."`
4. **Fix the hardcoded `"Codex"` bug in CLI repair (`onboarding.rs:82`):**
   - *From:* `"Finish installing Codex in the terminal, then return here."`
   - *To:* `format!("Finish installing {name} in the terminal, then return here.")` (and change element IDs `"install-codex"` / `"codex-guide"` to `"install-runtime"` / `"runtime-guide"`).
5. **Explain *why* the CLI is needed & allow switching when missing (`onboarding.rs` `Issue::Missing`):**
   - *From:* `"Install the stock {name} executable on this computer to open a session."`
   - *To:* `format!("OX runs the official {name} CLI locally on your machine (including for custom API models). Install {name} to continue, or switch runtimes above.")` If the *other* CLI is already installed, render a secondary button: `format!("Switch to installed {}", other_runtime.name())`.
6. **Warn & offer a folder picker when launched in `$HOME` (`onboarding.rs` + sidebar header):**
   - *Change:* Give the sidebar selectors micro-labels (`PROJECT` above `oscar ▾`, `AGENT & BILLING` above `ChatGPT subscription ▾`). If `self.project == home_dir`, show an onboarding banner: `"Project folder is your home directory (~)."` with button `"Choose project folder…"`.
7. **Auto-switch to the API route after saving a provider (`ConfigureProvider` handler + `Issue::Provider`):**
   - *From:* `"This project’s saved API route needs an OX provider connection."`
   - *To:* `"Add an API key (Anthropic, OpenAI, OpenRouter, or local) to use this route."` When the user clicks `"Configure an API provider"` from `Issue::SignIn` and saves a provider, automatically switch the active route from `Subscription` to that new API alias.
8. **Auto-poll CLI installation and login instead of manual `"Check connection"` (`onboarding.rs` `awaiting_login`):**
   - *Change:* When `awaiting_login` is true, poll `platform::onboarding::missing(runtime)` and connection state every 1.5s in the background so the onboarding card transitions to ready automatically the moment terminal login/install finishes. Keep `"Check connection"` only as a fallback.
9. **Hide dead session controls during first-run onboarding (Sidebar in `fresh.png`):**
   - *Change:* When total threads == 0 and `first_run_issue().is_some()`, hide the `Running / Needs you / Finished` filter tabs and the `RUNNING · 0 / End all idle` footer card. Replace `"Checking the session host..."` with `"Local host: starting…"` and hide it once connected.
10. **Restructure `README.md` top fold for 30-second quickstart:**
    - *Change:* Move `## Install and first run` immediately below the 2-sentence intro paragraph (above `## What OX is`). Add a 3-row table right under the install snippet showing exact commands for `Codex + ChatGPT`, `Claude Code + Claude`, and `Any model via API key (ox setup)`.
