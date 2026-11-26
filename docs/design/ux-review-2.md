# UX review 2 (screenshot-driven, Gemini 4 Argon via OX)

Status: round 1 PARTIAL (two screens); round 2 below covers most of the not-reached list (see "Not reached this round").

Method: branch build of ox-desktop with the layout probe (`OX_DESK_STATS_SOCKET`), own instances on Hyprland workspace 10, one guarded workspace hop, grim, 50% downscale. Review by `ox codex --model opencode-go/gemini-4-argon exec -i ...`. Screenshots live in the session scratchpad `uxdrive/`, not in git.

## Screen index

| File | Shows |
| --- | --- |
| s01_empty_state_narrow.png | Empty state, project "oscar", Claude subscription route, window ~717 px wide so the sidebar is collapsed behind "Threads". Composer pills "Claude plan · default" and "Default permissions". |
| s02_onboarding_fresh.png | First-run onboarding on a fresh HOME/XDG tree: Codex / Claude Code / API key tabs, "Connect Codex", "Sign in with ChatGPT", "Configure an API provider". |

## Not reached

The probe socket only answers `snapshot`, `reset` and `frame`. It reads controls and their bounds but cannot press them. The app runs natively on Wayland, `xdotool`/`ydotool` are not installed, and the sandbox denied both moving the user's cursor and sending key events to the window with Hyprland's `send_key_state`. So these were not captured: AGENT picker, composer model picker, Claude to Codex switch and back, filter menu, running list, all Settings tabs, Remote access, Start worker sheet, Continue in sheet. A `press` command on the probe socket (in-process, same path as a click) would unblock all of them without touching the real pointer.

Control labels the probe reported (wide layout, own instance, Claude route): command-palette "Commands · Ctrl K", project-picker, billing-route "agent Claude subscription", new-session, thread-filter, settings, remote-access, sidebar-usage-button, model-setting-0 "Claude plan", model-setting-1 "Effort Default", model-setting-2 "Speed Default", access-preset "Default permissions", send-message. Fresh instance: show-threads, command-palette, model-setting-0 "ChatGPT plan" (no agent connected), same others.

## Gemini review (verbatim, with our caveats after)

## Screen 1: `s01_empty_state_narrow.png` (Empty State, Narrow Window)

### What is confusing
* **Terminology collision (`Claude plan · default`):** "Claude plan" sounds like a billing tier (Pro/Max) or Claude Code's "Plan mode" (`/plan`), not an execution route + model. And `default` is meaningless—is the user running Sonnet 4.5, Opus 4.6, or Haiku?
* **Duplicate terminal icons (`>_` in top bar vs. `>_` square in breadcrumb row):** There are two nearly identical terminal glyphs 36px apart vertically with no labels, tooltips, or visual distinction (one is likely global shell/logs or app-level terminal, the other project/session terminal).
* **Duplicate project context (`oscar`):** `oscar` appears in the top-left breadcrumb (`oscar / New session`) and again as a floating subtitle below `"What shall we work on?"`. Neither shows the actual path (`~/oscar` vs `/tmp/oscar`), git branch, or whether `oscar` is clickable to switch projects.
* **`Threads` looks like a top-level tab, not a collapsed sidebar drawer:** At narrow widths, `Threads` has no drawer/sidebar icon, no active-thread badge, and no indication that clicking it toggles a left panel.

### Missing affordances
* **No settings, account, or phone-pairing trigger:** In a narrow window with the sidebar collapsed, Settings, provider connection status, background workers, and QR phone pairing are completely unreachable from the top bar.
* **Weak dropdown affordances in the composer:** `Claude plan · default ▾` and `Default permissions ▾` have no pill background, border, or hover container—just low-contrast muted text with a tiny 3px triangle. They look like static metadata rather than primary interactive controls.
* **No attachment or voice/context affordances:** `@ files` and `/ commands` are mentioned in placeholder text, which disappears the moment the user types a single character. There are no clickable `@` or `/` trigger buttons in the composer footer.
* **No project switcher affordance:** `oscar` in `oscar / New session` has no chevron (`oscar ▾`) or folder icon to signal that you can change the working directory before starting a session.

### Wording
* **`Claude plan · default` → `Claude Code · Sonnet 4.6` (or `Claude Code (Sub) · Default`):** Name the **agent harness** and the **actual model**, not the billing mechanism.
* **`Default permissions` → `Permissions: Ask` (or `Auto-edit` / `Read-only` / `Full auto`):** "Default permissions" tells the user nothing about whether the agent can run bash commands or delete files right now. State the active policy explicitly.
* **`New session` vs. `Threads`:** Pick one noun. Either the drawer is `Threads` and the breadcrumb is `New thread`, or both use `Sessions`.

### Settings clarity & grouping
* **Composer footer conflates three axes into two strings:** Agent harness (`Claude Code` vs `Codex`), Auth/Route (`Subscription` vs `API Key`), and Model (`Sonnet`/`Opus`/`o3`/`gpt-5`) are flattened into `Claude plan · default`. Safety mode (`Default permissions`) sits next to it with equal visual weight.

### How to switch agent/model easily (solving Complaint #1)
* **Split or structure the picker as a proper popover menu with explicit current state and dismissal:**
  1. **Always include the active item in the list** with a leading checkmark (`✓`), highlighted row background, and a `Current` badge. Never filter out the current selection (`Claude plan`) so the menu only shows `ChatGPT plan`.
  2. **Structure the popover into clear sections:**
     * **Header:** `Agent & Model` + an explicit `✕` close button in the top-right corner.
     * **Section 1 — Agent Route:** Radio rows showing all configured routes:
       * `✓ Claude Code (Claude Subscription)` — *Active*
       * `  Codex (ChatGPT Subscription)`
       * `  API Key (OpenRouter / Anthropic / OpenAI)` — or `+ Connect provider…`
     * **Section 2 — Model:** Explicit model list for the selected route (`Default (Sonnet 4.6)`, `Opus 4.6`, `Haiku 4.5`) so "default" is never opaque.
  3. **Four dismissal paths (mandatory):**
     * Clicking the trigger pill again toggles it closed.
     * Clicking the `✕` button in the popover header.
     * Clicking anywhere outside the popover (backdrop click).
     * Pressing `Esc`.
  4. **Mid-thread switching ("Handoff"):** If a thread already has messages, selecting a different agent in the picker shouldn't silently mutate or hide options—show a clear action: `Switch for next message` or `Hand off thread to Codex →`.

### Discoverability & consistency
* **Zero discoverability for OX's core differentiators:** Nothing on the empty state hints that you can run background workers, hand off between Claude Code and Codex, or pair a phone via QR code. A subtle row of quick-action chips or footer hints below the composer (e.g., `Change folder`, `Spawn worker`, `Pair phone`) would expose the app's power without cluttering the 80s aesthetic.

---

## Screen 2: `s02_onboarding_fresh.png` (First-Run Onboarding)

### What is confusing
* **Tabs (`Codex` | `Claude Code` | `API key`) confuse product category with auth method:** `Codex` and `Claude Code` are **agents**; `API key` is an **authentication method** that can power *either* agent. Putting `API key` as a third sibling tab makes users think API keys are a separate third agent.
* **Redundant/conflicting dual CTAs inside the `Codex` tab:** The tab bar already has an `API key` tab, yet inside the `Codex` tab the body copy says *"— or use your own API key"* and renders a second full-width button: `Configure an API provider`. Clicking `Configure an API provider` vs. clicking the `API key` tab is completely ambiguous.
* **Dead top chrome during onboarding:** The top bar still shows `Threads` and `>_` while nothing is configured. If the user clicks `Threads` on a fresh machine, what happens? Are they trapped in onboarding or can they dismiss it?

### Missing affordances
* **No connection status indicators on the tabs:** If a user connects `Codex` first and wants to also connect `Claude Code`, the tabs have no status dots (`● Connected` / `○ Not set up`). Onboarding looks like a single-choice mutex ("pick one to enter") rather than a multi-provider setup where OX runs both.
* **No CLI binary detection feedback:** Since OX runs the *official* `codex` and `claude` CLIs, the screen doesn't show whether `codex` / `claude` are already installed or already authenticated in `~/.claude` or `~/.codex` on this machine (auto-detecting existing CLI auth would save 90% of users from signing in again).
* **No "Skip / Explore app" or Back/Close action** if this screen is ever re-opened from Settings.

### Wording
* **`Sign in to your ChatGPT account so the official Codex CLI can run here — or use your own API key.`:** Too wordy and splits focus. Change to:
  * **Headline:** `Connect OpenAI Codex`
  * **Body:** `Run the official Codex agent using your ChatGPT Plus/Pro subscription or an OpenAI API key.`
* **`Configure an API provider`:** Vague. If it stays inside the Codex tab, it should read `Use an OpenAI API key instead`.

### Settings clarity & grouping
* **Group by Agent first, or Auth method inside each Agent:**
  * Option A (Best): Two agent cards side-by-side or stacked (`Claude Code` and `Codex`), each with `[Sign in with subscription ↗]` and `[Use API key]`, plus a checkmark badge once connected and a `[Continue to OX →]` button as soon as at least one is ready.
  * Option B: Keep the tabs, remove the `API key` tab, and let each agent tab (`Codex`, `Claude Code`, `Custom / Local Models`) offer both subscription OAuth and API key input inline.

### Discoverability & consistency
* **Left-aligned mascot and tabs inside a narrow centered column feel visually lopsided** compared to Screen 1's centered mascot (`What shall we work on?`). Align the card container cleanly with a subtle border/surface (`#161926`) so the onboarding block reads as a structured setup card rather than floating controls in empty space.

---

## Top 15 Concrete Changes Across OX Desktop (Ranked by Impact)

1. **Composer Agent/Model Picker Popover (`Claude plan · default` menu)**
   * **Change to:** Keep the currently active agent/route visible in the menu at all times with a `✓` checkmark, highlighted row, and `Current` pill. Never hide the active provider. Add four close paths: clicking the trigger pill again, an explicit `✕` button in the popover header, clicking outside the popover, and pressing `Esc`.

2. **Composer Route/Model Pill Label (`Claude plan · default`)**
   * **Change to:** `[Agent Icon] Claude Code · Sonnet 4.6 ▾` (or `Codex · o3 ▾`), with a small sub-badge or tooltip for auth route (`Subscription` vs `API`). Replace the ambiguous word `"plan"` (which collides with Plan Mode) and replace `"default"` with the actual resolved model name.

3. **Agent/Model Popover Structure (Single flat list → 2-level or sectioned menu)**
   * **Change to:** Two explicit sections inside the popover:
     * **AGENT & ROUTE:** `✓ Claude Code (Claude Subscription)`, `Codex (ChatGPT Subscription)`, `API Models…`
     * **MODEL:** Radio list of models for the selected agent (`Sonnet 4.6 (Default)`, `Opus 4.6`, `Haiku 4.5`), plus a footer link `Manage providers & API keys…`.

4. **Composer Permissions Pill (`Default permissions ▾`)**
   * **Change to:** Explicit policy label + status icon: `🛡 Ask before running ▾` (with menu options showing the current selection with `✓`: `✓ Ask before running (Default)`, `⚡ Auto-approve edits`, `🔒 Plan / Read-only`, `🚀 Full auto (sandboxed)`).

5. **Composer Footer Pill Styling (Borderless muted text)**
   * **Change to:** Give both footer dropdowns a distinct pill surface (`background: rgba(255,255,255,0.06)`, `border: 1px solid rgba(255,255,255,0.10)`, `border-radius: 6px`, `padding: 4px 8px`) and a visible chevron (`▾`) that rotates `180°` when open so they look clickable and show open/closed state.

6. **Onboarding Tab Architecture (`[Codex] [Claude Code] [API key]` + duplicate `Configure an API provider` button)**
   * **Change to:** Remove the separate `API key` tab OR remove the redundant `Configure an API provider` button. Better: Show a unified **2-card setup checklist** (`Claude Code` and `Codex`) where each card has `[Sign in with Subscription ↗]` and `[Use API Key]`, shows `✓ Connected` in real time, and enables a primary `Start using OX →` button once at least one is connected.

7. **Onboarding Existing-CLI Auto-Detection (`s02_onboarding_fresh.png`)**
   * **Change to:** On first launch, check if `claude` or `codex` CLI is already authenticated on the machine. If found, display a green banner/button: `✓ Existing Claude Code session detected (oscar@…) — Use this account` (1-click setup instead of forcing an external browser OAuth loop).

8. **Top Navigation Bar — Missing Global Utility Controls (Right side of skyline header)**
   * **Change to:** Add utility icon buttons on the right side of the top bar (or immediately left of the skyline illustration): **Background Workers** (`⚡ 0`), **Phone Pairing QR** (`📱`), and **Settings** (`⚙`), so none of these require widening the window or hunting inside the collapsed `Threads` drawer.

9. **Top Bar `Threads` Button (`Threads` text-only link)**
   * **Change to:** `[☰ Threads]` (prepend a sidebar-panel toggle icon) and show an active thread/worker count badge when background threads are running. When the drawer opens over a narrow window, include an explicit `✕` close button, outside-click dismissal, and `Esc` to close.

10. **Duplicate Terminal Icons (`>_` in top bar vs. `>_` button in breadcrumb bar)**
    * **Change to:** Differentiate or merge them. Keep one contextual terminal toggle in the session header labeled `[>_ Terminal]` (with shortcut tooltip `Ctrl+\``), and if the top-bar `>_` is a global CLI/runner log or quick-command bar, give it a distinct icon + text label (`Logs` or `Shell`).

11. **Project Breadcrumb (`oscar / New session`)**
    * **Change to:** `[📁 oscar ▾] / New thread` (with git branch pill e.g. `⎇ main` next to `oscar`). Clicking `oscar ▾` opens the project/folder switcher with a checkmark next to `oscar` and `Open folder…`. Remove the redundant `oscar` subtitle floating under `"What shall we work on?"`.

12. **Mid-Thread Agent Switching / Handoff Clarity**
    * **Change to:** When switching the agent pill inside an *active* thread (e.g., from Claude Code to Codex), show a clear segmented choice in the picker: `Continue here with Codex` vs. `Hand off to new Codex thread →`, while still showing `✓ Claude Code (Current)` at the top of the menu so the user can cancel or stay put.

13. **Composer Action Affordances (`Ask anything · @ files · / commands`)**
    * **Change to:** Add clickable `[+]` / `[@]` / `[/]` icon pills on the left of the composer footer (or right next to the send arrow) so users can browse `@ files` and `/ commands` via mouse without memorizing syntax after the placeholder text disappears.

14. **Terminology Standardization (`Threads` vs. `New session`)**
    * **Change to:** Standardize on **`Thread` / `New thread`** everywhere (or `Session` / `New session` everywhere). Do not mix `Threads` in the top bar with `New session` in the breadcrumb 28px below it.

15. **Empty-State Quick Actions Below Composer (`s01_empty_state_narrow.png`)**
    * **Change to:** Add 3 compact, low-profile action pills below the composer box on the empty state: `[📁 Switch folder]` `[🔀 Background worker]` `[📱 Pair phone (QR)]`. This surfaces OX's key workflows on narrow screens without encroaching on the 80s pixel mascot aesthetic.

## Caveats on the Gemini output

- It only saw two narrow-window screens. Its claim that Settings, Remote access and workers are "unreachable" is a narrow-width artefact: in the wide layout the sidebar footer has Settings, Remote access and Usage buttons. The narrow top bar really does hide them, which is itself a valid finding.
- Model names (Sonnet 4.6, Opus 4.6, o3) are invented; treat as placeholders.
- Items 1, 2, 3, 12 match the user's report (current selection hidden, no dismiss, hard to switch). Items 6 and 7 (merge API key into each agent, detect existing CLI logins) and 14 (thread vs session wording) are the most actionable new ones.


---

# Round 2: probe `press`/`key` run (wide 1448x870, Gemini 4 Argon)

Method: probe-enabled branch build on Hyprland workspace 10 with its own `OX_HOST_STATE_DIR`; driven only by the new in-process `press <id|label>` and `key <keystroke>` probe commands (no OS pointer or keys); 15 guarded workspace hops with grim; 50% downscale; the sidebar thread list was blanked before sending images to the model. Screenshots are in the session scratchpad `uxdrive2/`, not in git.

Probe note: a hidden window gets no frame callbacks, so the probe now lays out all windows right after each `press`/`key` (`Window::draw`, no present). Without that, the control registry lags one hop behind.

## Screen index

| File | Shows |
| --- | --- |
| w02_newsession | Empty state, Claude Code route, chips "Claude Code - Opus 5.5 (default)" and "Ask before edits" |
| w03_after_switch_to_codex | After choosing a Codex API model in the sidebar AGENT picker (keys down+enter) |
| w04_agent_picker_codex_tab | AGENT picker, Codex tab |
| w05_agent_picker_claude_tab | AGENT picker, Claude Code tab (list left scrolled, rows clipped top and bottom) |
| w06_composer_model_picker | Picker opened from the composer chip, rows with New session / Hand off |
| w07_back_to_claude | Back on Claude Code (picked by typing "claude subs" + Enter) |
| w08_filter_menu | Thread filter menu: All / Running / Needs you / Finished |
| w10_settings_providers, w11_settings_accounts, w12_settings_apikeys, w13_settings_remote | Settings tabs (inline in the main pane, 9 tabs) |
| w15_narrow_agent_picker | AGENT picker; the window did not narrow (floating resize did not apply), so this is wide again |

## Not reached this round

Hop budget (15) ran out. Not captured: Settings tabs Web search, Starred models, Appearance & input, Sessions, Advanced configuration; Start worker sheet and Continue in... sheet (no palette command; they open from a selected thread's menu / Hand off, and the thread rows, filter menu rows and picker rows are not marked controls, so only keys reach them); a true Running list (the filter menu selection did not land on Running; the sidebar then showed `is:finished` in the search box); a real narrow width. Gap to close next: mark thread rows, menu items and picker rows with `layout_probe::mark` equivalents, and add a probe `resize`.

## Gemini review, caveats first

- Our blanking rectangle covered part of the filter menu in w08, so its "filter menu is clipped behind the list" finding is an artefact. Re-check on a clean capture.
- "Phantom thread, Today 24 to 25": other lanes were creating sessions live; not a switching bug.
- The `is:finished` text in the sidebar search is real: the filter menu writes a query into the search box; there is no visible clear button.
- Real, confirmed in the images: clipped/half rows and a huge empty panel in the AGENT picker (w04, w05, w15); composer picker truncating names to "Dee...", "Glm...", "C..." (w06); "Current" check shown for a Codex model while the Claude Code tab is selected (w05); a row without a New session chip (Glm 5.3 alibaba); "Configured in Codex" permissions chip; Accounts shows "sign-in saved" next to a primary "Sign in" button; "Save changes" at the top of Providers with only Edit rows; OPENROUTER key missing from API keys; no Back/close on Settings except the footer gear.
- The Remote access screenshot showed the user's Tailscale account email to the model; do not reuse that capture elsewhere.

## Gemini review (verbatim, call 1: 10 screens)

### Screen-by-Screen Critique

#### 1. `w02_newsession` (Empty State — Claude Code)
* **Duplicate Agent/Model selectors:** `AGENT Claude Code · Op...` in the top-left sidebar and `Claude Code · Opus 5.5 (default)` inside the composer look like two different controls for the same thing. Users cannot tell if the sidebar sets a global default while the composer sets a per-thread override, or if they are mirrors.
* **Truncation in sidebar:** `Claude Code · Op...` truncates the most important part (the model name) even though there is horizontal room if the `AGENT` label wasn't taking up ~35% of the row width.
* **Redundant project label:** `oscar` appears three times: sidebar `PROJECT oscar`, top breadcrumb `oscar / New session`, and centered below `"What shall we work on?"`. The centered `oscar` under the H1 looks like an unclickable orphan label—either make it a clickable folder/branch pill (`📁 ~/oscar · main ▾`) or remove it.
* **Bottom-left icon bar (`⚙`, `🌐`, `◔`):** Mysterious icon-only buttons with no labels or active state indicator. `🌐` (remote/web?) and `◔` (usage/quotas?) have zero discoverability.
* **Top-right terminal icon (`>_`):** Sits alone with no label or tooltip affordance.

#### 2. `w03_after_switch_to_codex` (Switched to Codex API Model)
* **Silent thread creation bug (`Today 24` → `Today 25`):** Switching the agent in the sidebar on an *empty* new session incremented the thread count from 24 to 25 without the user sending a message. Changing a model before typing should mutate the pending draft session, not spawn a dead thread row.
* **Cryptic permission chip (`Configured in Codex`):** On Claude Code (Screen 1), the second composer chip is `Ask before edits` (clear, actionable). On Codex, it becomes `Configured in Codex`—which tells the user nothing about what permissions are actually active (Read-only? Auto-edit? Full access?) and reads like a disabled state even though it has a dropdown caret (`▾`).
* **Speed chip (`Speed Default ▾`):** "Speed" is vague—in Codex this is Reasoning Effort (`Low / Medium / High / Extra High`) or Fast mode. Calling it "Speed" obscures whether it changes latency or intelligence/reasoning depth.

#### 3. `w04_agent_picker_codex_tab` (Sidebar Agent Picker — Codex Tab)
* **Broken scroll/clipping container:** Look at the bottom of the list: `API MODELS · alibaba` is a section header with nothing underneath it except a 1px clipped top border of a button on the right, followed by `"New session: starts a fresh session on that route."` floating in the middle of a huge empty dark box that extends all the way to the bottom window edge. The scroll viewport is severely miscalculated (likely a fixed small height inside a full-height modal container).
* **Inconsistent row actions (`Glm 5.3 · API · alibaba`):** Every row has a `[New session]` badge except `Glm 5.3 (API · alibaba)`, which has no badge at all. Why? Is it unconfigured, disabled, or a layout bug? There is no explanation.
* **Missing section header:** The top 6 starred rows have no `STARRED` section header (unlike Screen 5 which shows `STARRED`), so they just collide with the helper text `"API models run with this runtime; subscription rows use their own."`
* **Confusing hierarchy between "Run with" tabs and "Subscription plans":** When `Run with: [Codex]` is selected, showing `Claude Code (Claude subscription)` inside the list violates the tab filter. Subscription plans ignore the `Run with` toggle, which forces you to write a sentence of small-print helper text (`"API models run with this runtime; subscription rows use their own."`) to explain a UI contradiction.
* **Redundant `[New session]` buttons on every row:** You are *already* on an empty `New session` screen, and this picker was opened from the sidebar's default `AGENT` dropdown. Putting an explicit bordered `[New session]` button on every single row adds massive visual noise. Clicking a row should just select it.

#### 4. `w05_agent_picker_claude_tab` (Sidebar Agent Picker — Claude Code Tab)
* **Janky scroll state & clipped rows on tab switch:** Switching from `Codex` to `Claude Code` left the list scrolled halfway down! The first row (`Deepseek V4.1 Flash`) is sliced in half horizontally at the top, and under `API MODELS · alibaba` at the bottom, another row (`Deepseek V4.1 Flash 1017`) is sliced in half horizontally.
* **Zero visual feedback on tab switch:** The list of models under `Claude Code` is identical to `Codex`, and `Deepseek V4.1 Flash 1017` still says `Current` even though it is currently running on `Codex`, not `Claude Code`! If the user is on `Codex · Deepseek V4.1 Flash 1017` and switches the tab to `Run with: Claude Code`, that row is *not* `Current` under the Claude Code runtime—it should be selectable to switch runtimes with the same model.

#### 5. `w06_composer_model_picker` (Composer Model Picker)
* **Two different pickers for the exact same data:** Opening the picker from the composer chip renders a smaller popover with *different* layout, *different* widths, and *different* actions (`[New session]` + `Hand off`) than opening it from the sidebar `AGENT` dropdown (Screens 3–4).
* **Severe text truncation:** Because each row tries to cram `[★] [Model Name] [API · provider] [New session] [Hand off]` into ~380px, model names are crushed into unreadable stubs: `Dee...`, `Glm...`, `Gem...`, and `Codex` under Subscription Plans is truncated to `C...`! A model picker where you cannot read the model names fails its primary purpose.
* **"Hand off" shown on an empty session:** Why is `Hand off` offered when the current session is completely empty (`New session` with 0 messages)? There is no context to hand off.
* **No primary click action:** When a row has both `[New session]` and `Hand off`, what happens when you click the model name itself? Does it switch the model in-place for the current session, start a new session, or hand off? In an empty session, clicking a model should simply change the active model in-place.

#### 6. `w07_back_to_claude` (Back to Claude Code)
* **State flow check:** The composer returned cleanly to `Claude Code · Opus 5.5 (default)` and `Ask before edits`, and `Today 25` did not increment to 26 (good), though the earlier spurious increment from Screen 2 remains.

#### 7. `w08_filter_menu` (Sidebar Thread Filter Popover)
* **Z-index / clipping bug:** The filter dropdown menu (`All / Running / Needs you / Finished`) is rendered *behind* or clipped by the thread list container! `"Running"` becomes `"Running"`, `"Needs you"` is cut off on the left to `"eds you"`, and `"Finished"` is cut off to `"ished"`. (Even if the black privacy rectangle was added in post-processing, the popover opens down-and-right across the sidebar boundary without proper elevation/alignment; it should align its right edge to the filter button so it stays inside the sidebar column, or render in a top-level overlay portal).

#### 8. `w10_settings_providers` (Settings — Providers & Models)
* **No way to exit Settings (`Close` / `Back` missing):** When you click `⚙` in the bottom-left, the entire main pane becomes `Settings`. There is no `← Back to session` button, no `✕` close button in the top right, and the `⚙` icon at the bottom-left doesn't even show an active/selected highlight. The only way out is guessing that clicking `+ New session` or a thread in the sidebar closes Settings.
* **Mystery `Save changes` button:** There is a bright primary `[Save changes]` button in the top-right of `Configured providers`, even though no fields are editable on this screen (each row just has an `Edit >` drill-down link). What is `[Save changes]` saving?
* **Duplicate "Add provider" pills:** `OpenCode Go`, `Alibaba Cloud`, `OpenRouter`, `Anthropic`, and `OpenAI` are *already* in `Configured providers`, yet all 5 appear again under `Add provider` at the bottom. Clicking "Add provider" for an already-configured singleton provider is confusing—either hide already-added providers (unless multiple accounts per provider are supported, in which case label it `+ Add another provider / custom`) or show a `+ Add provider` button that opens a template picker.
* **Search filter leaked into sidebar (`is:finished`):** Notice the sidebar search box says `is:finished` while in Settings, which works well as a query syntax, but there is no `✕` clear button inside the search input to clear the filter quickly.

#### 9. `w11_settings_accounts` (Settings — Accounts)
* **Contradictory auth status vs. button CTA:**
  * Under **ChatGPT**, the status text says: `"A native Codex sign-in is saved on this computer."` Immediately below it is a giant primary blue button: `[Sign in with ChatGPT]`. If I am already signed in, why is the primary CTA telling me to sign in?! It should show the signed-in status badge (`✓ Signed in via Codex CLI`), account email/plan if known, and secondary `Re-authenticate` / `Sign out` buttons.
  * Same issue under **Claude Code**: `"A native Claude sign-in is saved on this computer."` paired with `"Sign in with Claude"`.
* **Inconsistent button styling across the 3 cards:** The ChatGPT card has a full-width solid blue button (`[Sign in with ChatGPT]`). The API provider card and Claude Code card have plain borderless centered text links (`Configure provider`, `Sign in with Claude`) that barely look clickable.
* **Redundant "API provider" card:** Having an `API provider -> Configure provider` card inside `Accounts` just bounces the user to `Providers & models` or `API keys`.
* **About / Version card buried in Accounts:** `OX Desktop and ox CLI 0.1.0 / Check for updates / Made by Oscar / Weather data from MET Norway` has nothing to do with user accounts. (Also: why does an AI coding desktop app credit weather data from MET Norway?)

#### 10. `w12_settings_apikeys` (Settings — API Keys)
* **Fragmented 3-tab mental model (`Accounts` vs `Providers & models` vs `API keys`):** To set up an API model, a user has to mentally stitch together three separate tabs: `Accounts` (which mentions API provider), `Providers & models` (which lists OpenAI, Anthropic, Alibaba, OpenCode Go), and `API keys` (which lists `ALIBABA_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENCODE_API_KEY`). Worse, `OpenRouter` is configured in `Providers & models` (Screen 8), but `OPENROUTER_API_KEY` is completely missing from the `API keys` tab! And `BRAVE_SEARCH_API_KEY` is in `API keys`, even though there is a dedicated `Web search` tab right next to it.
* **Ambiguous input state when key is `Saved locally`:** Every card says `Saved locally` in the top-right, but the text input is completely empty showing the placeholder `Enter API key` and a primary `[Save key]` button. Users expect to see a masked placeholder (`••••••••••••••••••••4a9f`) when a key is saved, or a `Replace key` state, so they can tell at a glance that the field isn't empty/broken.
* **Dangerous unstyled `Remove` button:** `Remove` sits right next to `[Save key]` as plain text with no destructive color treatment.

---

### Top Changes Ranked by Impact

1. **Fix the Model Picker layout & merge the Sidebar/Composer pickers into one clean component (`w04`, `w05`, `w06`)**
   * **Exact element:** Composer model popover (`w06`) and Sidebar `AGENT` modal (`w04`, `w05`).
   * **Change to:**
     * Eliminate the sidebar `AGENT` dropdown completely—keep model/runtime selection in **one place**: the composer bar (and make that selection sticky as the user's default for future new sessions).
     * Widening the popover to `480px` and **two-line rows** or clean columns: Left = `[★] Model Name` (never truncated) + muted `provider` pill; Right = nothing on click (clicking the row selects the model in-place).
     * Only show a `[Hand off ↗]` action button when the current thread actually has messages (`messages.length > 0`). Never show `[New session]` badges on every row when the user is already in an empty `New session`.
     * Fix the inner scroll container height in `w04`/`w05` so the list fills the modal down to the footer without clipping rows in half (`API MODELS · alibaba`) and reset `scrollTop = 0` when switching between `Codex` and `Claude Code` tabs.

2. **Collapse `Accounts`, `Providers & models`, and `API keys` into a single `Models & Providers` settings view (`w10`, `w11`, `w12`)**
   * **Exact element:** Settings top tab bar (`Accounts | Remote access | Providers & models | API keys | Web search | Starred models | Appearance & input | Sessions | Advanced configuration` — 9 tabs is way too many).
   * **Change to:** Merge `Accounts`, `Providers & models`, `API keys`, and `Starred models` into two clear tabs:
     1. **`Subscriptions & Runtimes`**: ChatGPT / Codex CLI card and Claude Code CLI card. When a sign-in is detected, change the badge to a green `● Connected` pill and change the button from primary `[Sign in with ChatGPT]` to a subtle secondary `[Reconnect]` button. Move `OX Desktop 0.1.0 / Check for updates` to the bottom of the sidebar or an `About` footer.
     2. **`API Providers & Models`**: Each provider row (`OpenCode Go`, `Alibaba`, `OpenRouter`, `Anthropic`, `OpenAI`) holds its own API Key status (`Key saved ••••` + inline `Update key`), its base URL, and its toggleable/starrable model list in one place. Move `BRAVE_SEARCH_API_KEY` into the `Web search` tab where it belongs. Remove the dead `[Save changes]` button from the provider list header.

3. **Add an explicit exit affordance for Settings and highlight the active sidebar footer icon (`w10`, `w11`, `w12`)**
   * **Exact element:** Settings header (`Settings` H1) and bottom-left icon bar (`⚙`, `🌐`, `◔`).
   * **Change to:** Add an `✕ Esc` close button in the top-right of the Settings pane (or `← Back to session` above `Settings`), and give the active bottom-left icon (`⚙`) a filled background pill + tooltip labels on hover (`Settings`, `Remote Access`, `Usage`). Clicking `⚙` while in Settings should toggle back to the active session.

4. **Separate "Subscription plans" from the "Run with [Codex | Claude Code]" API runtime toggle in the Model Picker (`w04`, `w05`, `w06`)**
   * **Exact element:** `Run with [Codex] [Claude Code]` segmented control and the `SUBSCRIPTION PLANS` section buried below `STARRED` API models.
   * **Change to:** Put **Official Subscriptions** (`Claude Code · Opus 5.5` and `Codex · ChatGPT Subscription`) at the **very top** of the picker above a divider. Below the divider, place the `API Models — Run with: [Codex] [Claude Code]` header and the `STARRED` / provider model lists. Also ensure the `✓ Current` badge matches both `(runtime + model)` so switching the tab from `Codex` to `Claude Code` doesn't falsely show `Current` on a model currently active under Codex.

5. **Replace `Configured in Codex` and `Speed Default` composer chips with transparent, actionable controls (`w03`)**
   * **Exact element:** Bottom composer bar in Codex mode (`Codex · Deepseek V4.1 Flash 1017 ▾` | `Speed Default ▾` | `Configured in Codex ▾`).
   * **Change to:**
     * Replace `Configured in Codex ▾` with the actual effective permission mode read from Codex config (e.g., `Ask before edits ▾` or `Auto-edit (Codex) ▾`) so the user always sees their live safety posture before hitting Enter.
     * Rename `Speed Default ▾` to `Effort: Default ▾` (or `Reasoning: Medium ▾`) so users know it controls reasoning tokens/effort rather than network speed.

6. **Fix the thread filter popover clipping and add a clear button to the search input (`w08`, `w10`)**
   * **Exact element:** Thread filter dropdown (`All / Running / Needs you / Finished`) and `Search threads...` input.
   * **Change to:** Render the filter menu in a top-level portal (`z-index` above the thread list container) and right-align it to the filter icon (`right: 0`) so it stays inside the `260px` sidebar. Add an inline `✕` clear button inside `Search threads...` whenever a filter chip/text like `is:finished` is active.

7. **Stop creating phantom threads when switching models on an empty `New session` (`w02` → `w03`)**
   * **Exact element:** Session creation lifecycle on `New session` (`Today 24` incrementing to `Today 25` on model switch).
   * **Change to:** Keep `New session` ephemeral in memory until the first user prompt is submitted. Switching models/agents on an empty session should only update the draft state.


## Gemini review (verbatim, call 2: Remote access, narrow-attempt picker)

### 1. `w13_settings_remote` — Settings > Remote access

* **Confusing / paradoxical state:** The buttons show `"Enable Tailscale Serve"` right next to `"Disable remote access"`. Is remote access currently on or off? If Tailscale Serve is not enabled, how is remote access active enough to be disabled? Is it running on a LAN/local port without Tailscale Serve, or is the state machine broken? There is zero status indicator (e.g., `● Active`, `○ Stopped`, `● Connected via Tailscale`).
* **Missing core affordances (Where is the connection info and phone pairing?):** For a feature meant for remote access and phone pairing, this screen gives the user nothing actionable:
  * No URL / MagicDNS hostname / Tailscale IP / port shown.
  * No `"Copy URL"` button.
  * No QR code for phone pairing, no list of paired devices/sessions, no auth token or PIN status, and no revoke/disconnect controls.
* **Wording & settings clarity:**
  * `"Tailscale account · [account email redacted]"` sits there as bare text with no explanation of whether the Tailscale daemon is actually running, authenticated, or reachable, nor what node name this machine has on the tailnet.
  * `"Enable Tailscale Serve"` vs. `"Disable remote access"` mixes implementation mechanics (`Tailscale Serve`) with product-level state (`remote access`) on the same action row with identical button styling.
  * `"Refresh"` has no feedback, timestamp (`Last checked just now`), or explanation of what it refreshes.
* **Visual hierarchy & layout:**
  * Three identical secondary-styled buttons in a flat row—a destructive action (`Disable remote access`), a primary setup action (`Enable Tailscale Serve`), and a utility action (`Refresh`) all look exactly the same.
  * **9 tabs in a single horizontal pill bar** (`Accounts`, `Remote access`, `Providers & models`, `API keys`, `Web search`, `Starred models`, `Appearance & input`, `Sessions`, `Advanced configuration`) is bloated and overlaps conceptually (`Accounts` vs `Providers & models` vs `API keys` vs `Starred models`).
  * No explicit `"Close"` or `"Back to session"` affordance in the Settings header.

---

### 2. `w15_narrow_agent_picker` — The AGENT picker

* **Broken spatial anchoring & wasted height:** The trigger is `AGENT` in the top-left sidebar, yet clicking it opens a floating modal centered over the main canvas that stretches vertically all the way to the bottom window border—with **~55% of the panel completely empty dark space**. It obscures the active prompt canvas while detaching from the control that opened it.
* **Confusing "New session" badge when already on a New Session:** The background breadcrumb literally says `oscar / New session` (an empty, unstarted draft). Yet every non-active row in the picker has an amber bordered `"New session"` badge, and the footer says `"New session: starts a fresh session on that route."` If I haven't even sent a message yet, why is picking a model threatening to start a *new* session instead of just changing the model for the current empty draft?
* **Action button vs. row selection ambiguity:** Every row ends with `[New session]` (or `[Current]`) styled like a button/badge. Does clicking the model name select it, or do I have to click `"New session"`? Why does every single row repeat the exact same two words `"New session"` 7 times in bright amber, creating visual noise that outshines the model names and provider labels?
* **How to pick API models & runtimes (Mental model collision):**
  * The picker combines **two orthogonal choices** into one flat list:
    1. **API models + a runtime toggle:** `"Run with [Codex] [Claude Code]"` at the top only applies to the `STARRED` API models section (`"API models run with this runtime; subscription rows use their own."`).
    2. **Subscription plans:** `Codex (ChatGPT subscription)` and `Claude Code · Opus 5.5 (de... (Claude subscription)` at the bottom ignore the `"Run with"` toggle entirely.
  * Because `"Run with"` sits at the very top above everything, it looks like a global filter or a global toggle for the entire modal. Having to read a fine-print footnote (`"API models run with this runtime; subscription rows use their own."`) to understand why the toggle affects the top half of the list and not the bottom half is a classic UX smell.
* **Discoverability of unstarred models & sub-models:**
  * Only `STARRED` models and `SUBSCRIPTION PLANS` are visible. Where are the unstarred API models? Do you have to type blindly into `"Search models"` to find them? There is no `"All models"` / `"Browse providers"` collapse or link to configure models.
  * Under `SUBSCRIPTION PLANS`, how do you change the model *within* a subscription (e.g., switching Claude Code between Opus and Sonnet, or Codex between different reasoning efforts/models)? `Claude Code · Opus 5.5 (de...` is hardcoded into a single truncated row.
* **Truncation & typography inconsistencies:**
  * `Claude Code · Opus 5.5 (de...` truncates the critical suffix (`default`?) right next to `Claude subscription`, even though the modal is wide enough if the metadata weren't crammed into one single-line flex row.
  * Casing is inconsistent: `Glm 5.3` (title-cased acronym) vs `GPT 6.2 Sol` (all-caps acronym) vs `Deepseek V4.1 Flash` (lowercase 's').
  * Duplicate names (`Glm 5.3` twice) force the user to scan right to `API · alibaba` vs `API · opencode-go` to tell them apart.

---

### Top 8 Changes for These Screens

1. **Redesign `Remote access` around a live status card + connection/pairing block:** Show an explicit status badge (`● Off`, `● Local only`, or `● Live on Tailscale`), the active Tailscale URL/hostname with a one-click **Copy URL** button, and a **QR code / Pair phone** card so users can actually connect their phone from this screen.
2. **Untangle `Tailscale Serve` from `Remote access` enablement:** Replace the three ambiguous buttons (`Refresh | Enable Tailscale Serve | Disable remote access`) with a primary toggle/switch for **Remote Access**, and a sub-option or clear mode selector for exposure (`Tailnet via Tailscale Serve` vs. `Local network`), using destructive styling (`Disable`) only when active.
3. **Stop repeating `"New session"` on every row of the Agent Picker—and don't fork an empty draft:** If the current view is already an unstarted `New session`, selecting an agent/model should simply switch the active route in-place. Only show a `"Starts a new session"` warning (or a single footer notice / icon) when switching routes mid-conversation in an already-active thread.
4. **Separate "Subscription Agents" from "API Model + Runtime" visually:** Group the picker into two clear cards or tabs—or move the `"Run with [Codex | Claude Code]"` segmented control *inside* the `API MODELS` section header rather than above the entire modal—so it is visually obvious that the runtime toggle only governs API models and not subscription plans.
5. **Anchor and auto-size the Agent Picker popover:** Anchor the popover to the `AGENT` trigger in the top-left (or size the centered command palette to fit its content with a `max-height`) instead of stretching an empty box down to the bottom edge of the window.
6. **Expose unstarred API models and subscription model variants:** Add an expandable `"All API models (N)"` section (or provider tabs) below `STARRED` so users don't have to guess search queries, and allow picking model/effort variants on Subscription rows (e.g., Sonnet vs. Opus for Claude Code) without leaving the picker.
7. **Fix row truncation and normalize model/provider labels:** Give model rows a clean two-column or two-line layout so `Claude Code · Opus 5.5 (default)` never truncates to `(de...`, normalize casing (`GLM 5.3`, `DeepSeek V4.1 Flash`), and style provider tags (`alibaba`, `opencode-go`, `openai`) as distinct pills so duplicate model names are instantly distinguishable.
8. **Consolidate the 9 Settings tabs into 4–5 logical groups:** Merge `Accounts`, `Providers & models`, `API keys`, and `Starred models` into a single **Models & Providers** (or **Providers & Auth**) section so users don't have to hunt across four separate tabs to set up and star the models that appear in the Agent Picker.


## Top 15 ranked changes (merged, our reading)

1. One model/agent picker: make the sidebar AGENT picker and the composer picker the same component and size; fix the scroll/clip bug (reset scroll on tab switch, no empty panel to the window bottom), widen rows so names never truncate to "Dee...".
2. In an empty New session, picking a route just changes it; drop the per-row "New session" chips and "Hand off" there. Show Hand off only for threads with messages.
3. Put subscription plans first, then API models; move "Run with Codex / Claude Code" inside the API section so it does not look global. Mark Current by runtime plus model.
4. Merge Accounts, Providers & models, API keys and Starred models into two groups (Subscriptions, API providers with their key, base URL and models); move Brave key to Web search; add the missing OpenRouter key.
5. Accounts: when signed in show a Connected badge with Reconnect/Sign out, not a primary "Sign in" button; move About/Version out of Accounts.
6. Settings needs a visible Back/close (and Esc), and the footer gear should show the active state.
7. Remote access: one status line (Off / Local / Live on Tailscale), URL with Copy, a QR pairing block and paired-device list; split enable/disable from "Tailscale Serve" wording; destructive style for Disable.
8. Composer chips on Codex: replace "Configured in Codex" with the effective permission mode; rename "Speed" to what it controls (Effort).
9. Filter menu: keep inside the sidebar (check on clean capture), add a clear button to the search box, show the active filter as a chip rather than raw `is:finished`.
10. Remove "Save changes" from the provider list header unless something on the page is editable.
11. Hide already-configured providers from "Add provider" or relabel it "Add another".
12. Show unstarred models (All models section) and per-subscription model/effort choices in the picker.
13. Normalise model labels (GLM, DeepSeek) and show provider as a pill so duplicate names (Glm 5.3 twice) are distinguishable.
14. Icon-only sidebar footer buttons (settings, remote, usage) need labels or tooltips; the lone top-right terminal icon too.
15. Tell the user what the project label `oscar` under the headline is (clickable folder/branch pill or remove it); sidebar AGENT row truncates the model, give it the width.
