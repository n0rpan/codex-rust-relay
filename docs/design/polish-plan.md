# OX polish plan: desktop and phone

Status: proposal, 2026-11-25, revision 2. Verified against `main` at `2708f55` plus the uncommitted favorites change in this worktree.
Scope: `desktop/ox-desktop` (GPUI) and `desktop/ox-host-web/assets` (phone web app).
Brief: keep the 80s arcade theme, make it look designed and finished, and keep it fun.

Read from source only. Nothing here was checked against a running build. The nesting diagnosis in 3.4 is a hypothesis to confirm on a current build before fixing.

## 1. Diagnosis

The arcade identity exists but only in a few spots: the pixel ox, the marquee, the horizon, the 7×7 cartridge icons, the flame and the key-cap bevel on the phone's primary button. Everything around them is default component styling. Five things make it feel unfinished:

1. **Two different products.** Desktop defaults to lime on graphite (`appearance.rs:30-40`, accent `#c5ec88` on `#1b1e22`). Phone is periwinkle on indigo (`style.css:3-12`, accent `#a6b9ff` on `#191a29`). Same sprites, different brand.
2. **No scale.** Desktop mixes `text_xs/sm/lg/xl/2xl` with ad hoc 12, 13, 14 and 20 px, and row heights of 22, 24, 26, 30, 31, 32, 37, 39 and 44 px. The phone stylesheets use about 30 distinct font sizes and 18 distinct radii (0 to 1.1rem).
3. **State has no visual language.** Desktop has no success colour, approvals look like any other panel, errors are printed as muted small text (`host.rs:1177`, `transcript.rs:609`), and Stop is a ghost button.
4. **Two component kits.** `kit::button(p, id, label, primary, enabled, click)` and the preferences-only `button(id, label, primary, p, click)` (`preferences/forms.rs`) differ in size and behaviour, so Settings looks like another app.
5. **New features arrived as text.** Workers and handoffs (main #5, #6) are powerful, but on desktop they are typed commands with a usage string, and their status is rows of muted 12 px text with secondary buttons. On the phone they are bare stacked forms.

The fix is a small token system applied everywhere, plus a handful of flow repairs. No new framework is needed.

## 2. Design direction: "OX Arcade" as a system

Principle: **the cabinet is pixel, the screen is clean.** Chrome, status and identity get the arcade treatment. Transcript, code, diffs and forms stay calm and highly legible.

### 2.1 Colour tokens

One semantic set, used by both products. Add the four that are missing today (`success`, `raised`, `line_strong`, `accent_dim`).

| Token | Role | House dark (new) | House light (new) | Desktop today | Phone today |
|---|---|---|---|---|---|
| `bg` | page | `#191a29` | `#f5f5fa` | `#1b1e22` / `#f5f6f2` | `#191a29` / `#f5f5fa` |
| `nav` | sidebar, header | `#12131f` | `#ffffff` | `#15181b` / `#e8eae3` | `#12131f` / `#fff` |
| `panel` | cards, composer | `#222538` | `#eceef8` | `#25292e` / `#ffffff` | same as new |
| `raised` | buttons, hover | `#2d3149` | `#e3e6f4` | none (`hover` `#363c42`) | same as new |
| `line` | hairlines | `#3d415a` | `#a9b0c7` | `#363c42` / `#c4c7bc` | same as new |
| `line_strong` | bevel, focus track | `#494c6c` | `#6875a6` | none | literal `#494c6c`, `#6875a6` |
| `fg` | text | `#f0eff8` | `#191d32` | `#eaece7` / `#22261e` | same as new |
| `muted` | secondary text | `#b4b7ce` | `#47516d` | `#a4aaa8` / `#535b4c` | same as new |
| `accent` | selection, primary | `#a6b9ff` | `#465d9c` | `#c5ec88` / `#526d37` | same as new |
| `accent_dim` | key-cap bevel | `#6b81c8` | `#2f4274` | computed inline | literal `#6b81c8` |
| `warning` | needs you | `#f3bc80` | `#935119` | `#e0af68` / `#915600` | `--amber` |
| `success` | running, done | `#92d9bc` | `#216747` | **missing** | `--green` |
| `error` | failed, destructive | `#f3a598` | `#a82f35` | same as new | **missing** (`#ffd7cc` literal) |

Decisions:

- **Adopt the phone's indigo and periwinkle palette as the house palette** on desktop too. It is the more distinctive of the two and already ships a light variant (`settings.css:118-130`). The lime graphite defaults retire.
- **Theme source becomes** `System (Omarchy)` / `OX Arcade dark` / `OX Arcade light` (today: System / Light / Dark in `preferences/behavior.rs`).
- **Omarchy interplay.** Keep reading `colors.toml`, but map into the full token set and guard it:
  - `success` from `green` or `color2`. Today there is none.
  - `error` falls back to `fg` when the theme has no red (`appearance.rs:120`), which makes errors invisible. Fall back to the house `error` instead. Same for `warning`, which falls back to `accent` (`:117-119`).
  - `line` is taken from the theme's `muted`/`color8` (`:107-109`), which is often a mid-grey text colour and too loud as a border. Clamp to at most 30% of the way from `bg` to `fg`.
  - Add a contrast guard: if `muted` on `bg` or `on_accent` on `accent` is under 4.5:1, nudge toward `fg` until it passes. `lightness()` already exists for this.
  - Sprites keep using tokens, so the ox, marquee and cartridges recolour with any Omarchy theme. That already works; the plan only makes the derived palette trustworthy.
- Replace the roughly 35 literal hex values in `style.css`/`settings.css` (`#353b55`, `#303650`, `#454052`, `#343044`, …) with tokens or `color-mix()` of tokens.
- Colour means state, always: amber means needs you, red means failed or destructive, green means running or done, accent means selected or primary. YOLO keeps its flame and uses `warning`. Nothing else gets those colours.

### 2.2 Type

Two families. Body: the system sans on both (the phone names "Inter" at `style.css:15` but does not ship it; drop the name or bundle it). Display and data: bundle **Departure Mono** (believed SIL OFL, about 40 kB; confirm the licence before bundling), a pixel-grid monospace that stays legible at 11 to 13 px. Use it for labels, status, counters, key hints and code captions only. It never sets body text or transcript prose.

| Step | Size / line (desktop at base 16) | Phone | Use | Replaces |
|---|---|---|---|---|
| `label` | 11 / 16, pixel mono, caps, +0.08em | 12 / 16 | section labels, status chips, day rules | `small()` 12 px, `.status` 0.72rem, `.role` 0.7rem, `#network` 11 px |
| `meta` | 12 / 16 | 13 / 18 | secondary row text, timestamps, banners | `text_xs`, 0.77 to 0.82rem, `.handoff-marker` 0.85em |
| `ui` | 14 / 20 | 15 / 22 | buttons, list titles, menus | `text_sm`, 13 and 14 px, 0.85 to 0.9rem |
| `body` | 16 / 24 (user base) | 16 / 24 | transcript, forms | theme base, 1rem |
| `title` | 20 / 28, semibold | 22 / 28 | thread title, sheet title | `text_lg`, `text_xl`, 20 px, 1.25 to 1.6rem |
| `display` | 28 / 32, semibold, -0.02em | `clamp(26px, 6vw, 32px)` | page headings, onboarding | `text_2xl`, `clamp(1.6rem, 5vw, 2.15rem)` |

Other rules: code mono is `base - 1`, not `base - 3` (`appearance.rs:192` makes code 13 px against 16 px prose). The three literal `font_family("monospace")` calls in `ask_sheet.rs:94,99,138` use the theme mono. Phone `h1` letter-spacing of `-0.045em` (`style.css:155`) relaxes to `-0.02em`.

### 2.3 Spacing, size and shape

- **4 px grid.** Allowed steps: 4, 8, 12, 16, 24, 32. Remove one-offs (`mb(px(7.))`, `pt(px(42.))`, `pb(px(14.))`, `0.46rem`, `0.76rem`).
- **Control heights.** Desktop: 24 (chips), 32 (buttons, inputs), 40 (list rows). Phone: 44 minimum, 48 for primary. The sidebar row goes from 37/39 to 40.
- **Radius.** 2 px on controls, 0 on list rows and panels, 4 px on overlays and sheets. No pills. This removes `rounded_lg`/`rounded_md` on desktop and the phone's 0.4 to 1.1rem values, including the second `#composer` rule (`style.css:973-983`) that turns the composer into a rounded chat pill with a drop shadow and contradicts the first (`:420-428`).
- **Signature: the key cap.** The phone primary button's `inset 0 -3px` bevel (`style.css:63`) is the best detail in the product. Make it the primary button everywhere: 2 px bottom edge in `accent_dim`; pressed moves the label down 1 px. Desktop does this for Send only (`compose.rs:388-400`).
- **Elevation is line and tone, not shadow.** Remove the blurred shadows on the phone (`style.css:781,983`). Overlays get a 1 px `line_strong` border and a 2 px hard offset shadow in `nav`.
- **Section rule.** The day header (13 px accent dash, label, hairline; `host.rs:690-697`) becomes the one section-header pattern for sidebar groups, Settings sections and the phone's `.project` headings.

### 2.4 Motion

| Kind | Spec | Where |
|---|---|---|
| Sprite loop | 8 fps stepped (already 125 ms, `activity.rs:68`) | activity sprite, ox loader, flame |
| State change | 120 ms, `steps(3)`, opacity or 4 px offset | sheets, menus, toasts, row insert |
| Press | 60 ms, 1 px down | all key-cap buttons |
| Attention | 1 Hz two-frame blink, stops after 10 s | "Needs you" chip only |
| Reward | one 4-frame sparkle on the cartridge | turn or worker finished while unfocused |

No easing curves, springs or parallax. One looping animation per pane at most. Everything honours the existing switches (`Palette.motion`, `OXA_REDUCED_MOTION`, `data-phone-reduced-motion`, `prefers-reduced-motion`).

### 2.5 Iconography

Extend the 7×7 cartridge grid (`cartridges.png`, `activity::cartridge`) into one pixel icon set, rendered at 14 px (desktop) and 14 or 21 px (phone). It replaces the text glyphs in use today: `×`, `↑`, `↓`, `▾`, `•••`, `＋`, `★`, `↳`, `←`, `‹`.

Needed set (18): codex, claude, api, worker (exist); handoff, star-filled, star-empty, stop, eject, play/send, chevron, close, plus, menu, settings, warning, check, flame. One source bitmap generates the phone sprite sheet and the desktop `paint_quad` table so they cannot drift.

### 2.6 Sound and haptics

Off by default, one toggle in Appearance & input. Three events only: needs you, finished, failed. Phone: `navigator.vibrate(10)` on send and on an approval decision where supported (not iOS Safari). Desktop chiptune blips are wave 4, optional, and the first thing to cut.

### 2.7 Restraint rules

1. Pixel type is for labels of four words or fewer. Never for sentences.
2. No scanlines, glow, CRT curvature or gradients over content. The one gradient today (`.ask`, `style.css:344`) becomes flat `panel` plus the amber rule.
3. One primary button per view. Destructive actions are never primary.
4. Sprites appear in fixed slots only: sidebar header, activity line, list-row cartridge, empty states, approval header.
5. Every status has text as well as colour and sprite.
6. Any Omarchy theme must pass the contrast guard, or OX falls back to the house value for that token.
7. A feature the user can start must have a control. A typed command is a shortcut, never the only way.

## 3. Usability findings

Size: S is under 1 h agentic, M is 1 to 3 h, L is over 3 h or crosses the host protocol.

### 3.1 Start a thread, pick model and route

| # | Where | Problem | Fix | Size |
|---|---|---|---|---|
| 1 | `views/host.rs:1029-1035` | "by Oscar" in italic 14 px monospace sits beside the marquee and reads as a placeholder. | Move the credit to Settings → About. | S |
| 2 | `views/host.rs:851-887` | The route picker is a ghost button with a typed "▾" opening one flat menu of both runtimes times the **whole** model catalog (`Runtime::routes(&models)`, `:866`), with "★  " as a text prefix. With every catalog model now usable this menu is unbounded and has no search. | See finding 8: a short menu (subscriptions, starred aliases) ending in "All models…", which opens the searchable picker. | M |
| 3 | `views/host.rs:1177` (set at `:451`, `:591`, `:607`) | Errors such as "Project unavailable" print as muted 12 px text under the Settings button: easy to miss, not dismissable, not error-coloured. | One banner/toast component: `error` rule, message, optional action, dismiss. Reuse for the command notice at `workbench.rs:757-780`. | S |
| 4 | `views/host.rs:1083-1135` | Opening a saved thread whose alias is unknown swaps in a primary button "Choose the billing alias to resume" above the list, with no link to the thread it concerns. | Row-level state: a "Needs a model" chip on that thread with the picker anchored to it. | M |
| 5 | `views/route_chip.rs:210-247`, `compose.rs:241-289` | Model, Effort, Speed and Permissions are ghost text buttons of varying width that wrap unpredictably under 760 px. | One 24 px chip component (pixel label plus value), fixed order, collapsing into one "Session" chip when narrow. | M |
| 6 | `views/workbench.rs:702-714` | The header shows `project   /   route` with typed triple spaces and a 5 px square, and no session state. | Breadcrumb in `meta` with a cartridge icon and a status chip (Running, Idle, Needs you, Ended). | S |
| 7 | `views/host.rs:132-138` | The search placeholder is now "Search threads… (is:running, is:waiting, is:finished)". It truncates in a narrow sidebar and hides a useful feature behind syntax. | Keep the tokens for keyboard users; add the same four filter chips the phone has (`index.html:91-96`: All, Active, Requests, Finished) under the field, with counts. | S |

### 3.2 Favorites and model choice (owner finding a)

Decided and implemented in the uncommitted diff: **any catalog model can be used; stars only order lists**. The host no longer refuses unstarred API routes (`desktop/ox-host/src/runtime.rs:188-195`), and `list_ox_models` returns the whole catalog with starred models first and a `starred` flag (`src/workers.rs:699-716`). What remains is the UI: every picker should show starred models first and let the user search and pick anything.

| # | Where | Problem | Fix | Size |
|---|---|---|---|---|
| 8 | all model surfaces | Each surface picks models differently. The composer picker already does it right: searchable, starred first, then alphabetical (`route_chip.rs:30-72`). The sidebar route menu is a flat unsearchable dump (`host.rs:866-876`); the alias-resume menu lists raw aliases (`host.rs:1094-1100`); the phone uses a native select for "Runtime and billing" (`index.html:128-129`) and for handoff routes (`:236-237`), and a free-text field with a datalist for worker models (`:204-206`). | **One model picker, used everywhere.** Search field focused on open. Section "STARRED" in star order, then "ALL MODELS" grouped by provider. Each row: star toggle (tap to star or unstar in place), alias, provider. Subscription routes pinned above both. Short menus show starred only and end with "All models…". Phone gets the same list as a bottom sheet. | M |
| 9 | `preferences.rs:884-1010` | Favorites is two lists and a text field: starred rows with "↑", "↓", "Remove"; an "Add alias" field that doubles as the filter; up to 50 unstarred models as "＋ alias" buttons (`:994`) with no sign that more exist; nothing takes effect until "Save favorites" (`:892`). The page still reads as a gate ("add to use"). | Retitle to "Starred models" with one line: "Starred models are listed first for you and for agents. Any model can be used." Use the picker list from finding 8 inline. Reorder starred rows by drag (keep arrow keys). Autosave with an inline "Saved" tick. Show "50 of N, refine search" when capped; an unknown alias offers "Add custom alias". | M |
| 10 | `route_chip.rs:67`, `host.rs:873` | The star is a text character prefixed to the label, so starred names do not align with unstarred ones. | Star icon in a fixed leading column (filled or empty). | S |
| 11 | copy | README and Settings text still describe favorites as what makes a model available in Desktop. | Update copy to the ordering-only rule. | S |

### 3.3 Approvals and permission asks

| # | Where | Problem | Fix | Size |
|---|---|---|---|---|
| 12 | `views/ask_sheet.rs:66-104`, `kit/mod.rs:92-104` | The most important moment in the product looks like a settings panel: "1 request" in muted small text, a plain title, an unstyled monospace block, no warning colour, no icon. | Approval card: 3 px `warning` left rule, Waiting cartridge, `label` "NEEDS YOU · 1 OF 3", `title`, breadcrumb in `meta`, command or diff in an inset code block with copy. Matches the phone's `.ask` (`style.css:342-348`). | M |
| 13 | `views/ask_sheet.rs:118-133` | Decisions are equal-width buttons labelled "1   Allow" with typed spaces. Broad grants look the same as one-off ones. | Key-hint chips (`[1]`) at the trailing edge. Keep allow and deny equal weight (stated intent at `app.js:604-605`), but put session-wide grants below a hairline with a one-line consequence. | S |
| 14 | `views/ask_sheet.rs:71-78` | Under 825 px the inline sheet is capped at 35% of the window, so long diffs scroll in a small box. | Grow to 60% with a collapse handle; detail collapsed by default ("Show 42 lines"). | S |
| 15 | phone `style.css:369-371` | Decision buttons sit inline with small margins, so Allow and Deny are adjacent 44 px targets about 7 px apart. | Stack full-width, 8 px gap, 48 px tall, deny-type choices last. | S |
| 16 | phone `index.html:255-257` | Asks render after the transcript; scrolling up to read a diff moves the decision out of view. Only the composer is sticky. | A compact "NEEDS YOU" bar in the sticky stack above the composer; tap scrolls to the ask. | M |
| 17 | `compose.rs:241-289`; phone `index.html:168-176`, `controls.js:130-140` | Permission mode is a dropdown on desktop and a select plus "Apply mode" on the phone. New on main: "Also YOLO for its workers and handoffs" is a checkable menu item on desktop (`compose.rs:273`) and a checkbox on the phone. The grant's status and Revoke live in a muted text row above the transcript (`transcript.rs:621-641`; phone `app.js:1073-1090`). | Same chip and menu on both. YOLO and its grant form one clearly marked group in `warning` with a one-line consequence each. While a grant is active, the permission chip itself shows "YOLO + workers" with the flame, and Revoke is in its menu, not in a text row. | M |
| 18 | `compose.rs:31-36,52-56`; phone `app.js:256-257` | A worker that asks for YOLO can end up "waiting for you to accept YOLO", announced by a transient notice on the phone. On desktop the only confirmation path found is typing `--mode yolo`. Where the operator accepts a waiting worker is not visible in either UI. | Treat it as an approval: the worker row shows "Needs you", and the standard approval card (finding 12) offers "Allow YOLO for this worker", "Run with <ceiling> instead", "Cancel worker". | M |

### 3.4 Workers, sub-agents and handoffs (owner finding b)

Main #5 made workers host-owned sessions: the separate worker view is gone and a worker now uses the normal transcript and composer. That removes two findings from revision 1 (plain worker transcript, Refresh/Stop/Steer row).

**b. A nested worker jumps to the top level once clicked.** Main #5 already fixed one cause: a worker on a different route than its parent is now attached through an id lookup within the project (`views/host/discovery.rs:477-495`; phone `app.js:473-476`). **Re-test on a current build first.** If it still reproduces, the remaining suspect is the merge at `discovery.rs:458-461`: when a "higher quality" copy of the same row arrives, the stored parent is replaced by that copy's `parent_thread_id`, and live rows win on quality (`:454`). A live copy without a parent would drop the row to `roots` (`:497`).

| # | Where | Problem | Fix | Size |
|---|---|---|---|---|
| 19 | `views/host/discovery.rs:458-461` | Parent link can be downgraded from `Some` to `None` on merge. | Never downgrade: keep the existing parent when the better row has none. Add a regression test: open a child, re-index, assert depth 1. | S |
| 20 | `views/host.rs:733-777`, `kit/mod.rs:143-205`; phone `app.js:498-506`, `style.css:221-226` | Nesting is a "↳" text prefix plus 12 px of padding. No collapse, no child count, and the parent gives no hint when the selected row is its child. Handoff targets look the same as workers. | Real tree rows: disclosure chevron and count on the parent ("3 workers, 1 running"), a 1 px connector line, an "ancestor" tint on the parent while a child is selected, aggregated status when collapsed. Worker and handoff get distinct icons. Selecting a child never re-sorts the list. | M |
| 21 | `views/workbench.rs:676-714`, `compose.rs:134-141` | On desktop the only sign that the open session is a worker is the composer placeholder. The phone shows "worker 41 · workspace write ceiling" in its context line (`app.js:836-842`); desktop shows nothing about job id, ceiling or parent. | Header breadcrumb "Parent title ▸ Worker 41" with a worker chip and a ceiling chip. Clicking the parent crumb selects it. | S |
| 22 | `compose.rs:26-66`, `ox-desk-core/src/origin.rs:138-146`, `host/palette.rs:309-316,577-585` | On desktop a worker is started by typing `/worker codex:<model> [--mode <mode>] [--goal] <task>`, and a handoff by `/handoff …`. The palette's "Continue in…" only pre-types `/handoff ` and puts the syntax in its hint. Parse errors land in the header status line (`launch_error`, `workbench.rs:580`). | A "Start worker" and a "Continue in…" sheet on desktop, opened from the thread menu, the palette and a composer "+" chip. Fields: runtime, model (picker from finding 8), mode chips, permission (inherit, read, write, classifier, YOLO), task. The typed commands stay as shortcuts and errors show inline under the composer. | M |
| 23 | phone `index.html:197-254`, `app.js:1124-1210` | The phone has both forms, but as raw stacked label/select/textarea blocks injected above the transcript, opened from the ••• menu. Worker model is free text; "Leave empty for a Claude subscription worker" is a placeholder doing the job of an option. | The same sheet design as a bottom sheet: model picker, segmented mode control, explicit "Subscription" option, the provider notice (`#handoff-provider`) as a `warning` line. | M |
| 24 | `views/transcript.rs:542-641`; phone `app.js:1032-1090`, `style.css:1069-1089` | Handoff state is a stack of muted 12 px lines with secondary buttons: "Continued from…", "Handed off to X · <session id>" with "Open", "Handing off to X…", and "Handoff to X failed: …" in the same muted style. Raw session ids are shown. | One "link card" pattern: handoff icon, title of the other session (not its id), route chip, "Open" as a quiet link. In-progress shows the ox loader; failure uses the error banner (finding 3) with "Retry". The carried-context fold (`transcript.rs:343`) becomes a standard collapsible card. | M |
| 25 | `views/workbench.rs:24-46` | The parent's activity line no longer reflects host-owned workers (the job check was removed with #5); only native agent operations show the Worker scene. | Parent activity line shows "2 workers running" with the worker sprite when children are live, linking to the first one. | S |

### 3.5 Stopping and ending sessions (owner finding c)

Still open on main. **Stop interrupts the current turn and nothing else**: `Action::Stop` (`ox-desk-core/src/actions.rs`) maps to a turn stop or Claude's `interrupt()`, and the phone sends `{ stop: id }` for workers too (`app.js:906-909`). Session capabilities are `open, send, steer, stop, access` (`ox-host/src/engine.rs:2061`); none ends a session. Cancelling a worker exists only as an agent tool (`cancel_ox_worker`, `src/workers.rs:908`; `ParentCall::Cancel`, `engine.rs:1670-1688`); the operator has no button for it. The thread menu offers only Archive (`workbench/thread_menu.rs:98-163`). Stop itself is a muted ghost button that appears only mid-turn (`compose.rs:375-385`; phone `style.css:448-451`).

What main added that helps: `is:running` / `is:waiting` / `is:finished` search tokens on desktop and a Finished filter on the phone. They answer "what is running" for those who know the syntax; nothing answers "how do I end it".

Design: three verbs, the same words everywhere.

| Verb | Meaning | Control | Confirm |
|---|---|---|---|
| **Stop** | Interrupt the current turn. Session stays alive. | Square stop icon, secondary key cap, fixed slot right of the composer; disabled when idle rather than hidden. | Never |
| **End session** | Stop the turn if any, then close the runtime process. History is kept; the thread can be resumed. | Eject icon. Thread actions menu, Running panel row, palette, phone ••• menu. | Only with running workers or a pending approval: "End session and 2 workers?" |
| **Cancel worker** | The existing host cancel, offered to the operator. Files it wrote remain. | Eject icon on worker rows and in the worker header. | Never |

**Running panel ("Now playing").** A collapsible section pinned at the top of the sidebar whenever anything is alive, titled with the pixel label `RUNNING · 3`. It is the `is:running` + `is:waiting` result, always visible.

- One row per live process: cartridge, title, state chip, elapsed time. States: `Turn running` (green, the bars from `kit/mod.rs:186-190`), `Needs you` (amber), `Idle, process open` (muted), `Worker running`.
- Workers and handoff targets sit under their parent using the tree row from finding 20.
- Row hover (desktop) or swipe (phone) reveals **Stop** and **End**. The header has **End all idle**.
- The sidebar footer beside Settings (`host.rs:1161-1175`) shows a compact count when the panel is collapsed.
- Phone: the **Active** filter (`index.html:93`) becomes this panel; the conversation ••• menu (`index.html:136-144`) gains "End session".
- Palette (`host/palette.rs:293-316`): "Show running", "End this session", "End all idle sessions".
- An ended thread shows an `Ended` chip and a "Resume" primary.

| # | Where | Work | Size |
|---|---|---|---|
| 26 | `ox-desk-core/src/actions.rs`, `host_protocol.rs`, `ox-host/src/engine.rs`, `lease.rs` | New host-targeted `EndSession(id)` action and `end` capability; host closes the Codex thread or Claude process, cascades to owned workers and handoff attempts, updates the ledger. Snapshot exposes "process alive" separately from "turn active". Operator-facing `CancelWorker` reusing `ParentCall::Cancel`. | L |
| 27 | `views/host.rs`, `kit/mod.rs` | Running panel, footer count, row actions. | M |
| 28 | `views/compose.rs:375-385`, phone `style.css:448-451`, `app.js:906-909` | One stop control, fixed position, never hidden. | S |
| 29 | `workbench/thread_menu.rs:98-163`, phone `controls.js:379` | "End session" in thread actions; replace the phone's `window.confirm("Archive this session?")` with the in-app confirm sheet. | S |
| 30 | `ox host status` CLI | List live sessions and workers so the terminal agrees with the UI. | S |

### 3.6 Settings

| # | Where | Problem | Fix | Size |
|---|---|---|---|---|
| 31 | `preferences/forms.rs` versus `kit/mod.rs:12-38` | Two button implementations with different argument order and metrics. | Delete the preferences one; all controls come from `kit`. | M |
| 32 | `preferences.rs:1059-1066` and pages | Pages mix save models: Favorites and providers need an explicit save, behaviour applies live. | Autosave where the revision check allows, with a consistent inline "Saved" or "Changed on disk, review" row. Explicit Save only for credentials and the Advanced editors. | M |
| 33 | `views/host.rs:1161-1175` | Settings is a full-width button that turns into "Back to thread". | Footer bar: settings icon, running count, host status dot. Settings gets a back chevron in its own header. | S |
| 34 | `preferences/behavior.rs:134-136` | Text size offers 14, 16 and 19; 19 is off the scale. | 14, 16, 18, 20. | S |

### 3.7 Phone pairing and replies

| # | Where | Problem | Fix | Size |
|---|---|---|---|---|
| 35 | `index.html:27-64` | The pair screen shows everything at once: a secondary "Scan pairing QR", a paste field, a name field and a primary "Pair device". The obvious first action is the least prominent. | Three steps with a pixel step counter: 1 Scan (primary; "Paste a link instead" as a quiet link), 2 Name this device (prefilled), 3 Paired, with an offer to enable notifications. Ox sprite in the empty state. | M |
| 36 | `index.html:77-83` | "Session notifications" is a permanent disclosure between the heading and search. | One-time banner until decided, then it lives in Settings. | S |
| 37 | `style.css:973-1000` versus `:420-459` | The composer is styled twice; the later rule wins with a rounded shadowed pill and removes the send arrow. | One definition in the arcade language: square, 2 px top bevel, key-cap Send with the play icon. | S |
| 38 | `style.css:129-146` | Connection status is 11 px mono with a 6 px dot. | `label` step (12 px), 8 px dot, tap explains "Reconnecting" or "Offline". | S |
| 39 | `index.html:9`, `style.css:2` | `theme-color` is fixed at `#171825` and `color-scheme` at dark, so the light theme leaves a dark status bar. | Set both from the active theme in `settings.js`. | S |
| 40 | `index.html:136-144` | The conversation ••• menu now holds four unrelated items (two terminals, Start worker, Continue in…) and will gain End session. | Group with section rules: Session (Start worker, Continue in…, End session) and Terminal. | S |
| 41 | `app.js:892`, `index.html:275-276` | Send becomes "Steer" mid-turn, which is right, but Stop appears and disappears beside it and shifts the layout. | Fixed two-slot action row; see finding 28. | S |

## 4. Waves

Times are agentic wall-clock (Fable plans and reviews, Haiku implements); human-equivalent in brackets. Each wave ends with one batched hands-on check on the laptop and a phone, about 30 to 45 minutes of owner time.

**Wave 1: fix what hurts, lay the tokens (about 3 h agentic; roughly 4 human-days)**

- Re-test nesting on a current build; if it reproduces, finding 19 with a regression test.
- Tokens: section 2.1 in `appearance.rs` (new tokens, house palette, Omarchy fallbacks, contrast guard) and as CSS variables; remove literal hex from both stylesheets.
- Stop control in a fixed slot (28), error banner (3), "by Oscar" (1), duplicate phone composer (37), theme-color (39), text sizes (34), star column and copy (10, 11), filter chips (7).
- Approval card on desktop (12, 13) and stacked decisions on phone (15).

Outcome: both products share one palette, approvals and Stop look intentional, and the favorites wording matches the new rule.

**Wave 2: one model picker, and seeing and ending what runs (about 6 h agentic; roughly 7 human-days)**

- The shared model picker and Starred models page (8, 9), replacing the route menu (2).
- Protocol and host: `EndSession`, `end` capability, operator cancel, process-alive state (26). Do this before the UI that depends on it.
- Running panel on desktop and phone (27), thread-menu, palette and phone menu entries (29, 40), CLI listing (30).
- Tree rows with disclosure, counts and ancestor state (20); worker header (21); parent activity line (25).

**Wave 3: workers and handoffs as designed features, plus the system pass (about 6 h agentic; roughly 8 human-days)**

- Start worker and Continue in… sheets on desktop and phone (22, 23); handoff link cards (24); YOLO grant in the permission chip and the waiting-worker approval (17, 18).
- Type scale and Departure Mono (2.2), spacing, heights and radius (2.3), key-cap buttons everywhere.
- Single kit: retire the preferences button (31), chip component (5, 6), section-rule pattern, settings save model and footer (32, 33), alias-resume row state (4).

**Wave 4: delight (about 3 h agentic; roughly 4 human-days)**

- Pixel icon set from one source bitmap (2.5); motion spec (2.4) including the finished sparkle and the needs-you blink.
- Phone pairing in three steps (35), notification banner (36), sticky needs-you bar (16), ask sheet height (14).
- Empty states with the ox: no sessions, no matches, nothing running, paired successfully.
- Optional and last: haptics and the three sound blips (2.6).

## 5. Acceptance checks

Checks a person runs, not gates:

1. Put the desktop and the phone side by side on the same thread. Same colours, same status words, same icons.
2. Switch through five Omarchy themes including one light and one low-contrast. Nothing becomes unreadable; errors are always visibly errors.
3. Start a worker from a session, click it, click the parent, click it again. It never leaves its parent.
4. From a cold look at the window, answer in five seconds: what is running, and how do I end it?
5. Open any model picker. Starred models are on top; typing three letters finds an unstarred model and it starts a session without a trip to Settings.
6. Start a worker and a handoff on desktop without typing a slash command.
7. Answer an approval on the phone one-handed without mis-tapping.
8. Turn on reduced motion. Nothing moves; nothing is lost.
