# Design critique: OX vs T3 Code and DeepSeek Harness

Source: Gemini 4 Argon via `ox codex --model opencode-go/gemini-4-argon exec -i <screenshots>` (dogfood), 2026-11-25, on the branch build. Claude-measured misalignments added at the end. Stop/Send decision: one fixed icon button (↑ idle, ■ running) instead of hiding Stop.

The owner is right. OX Desktop looks like a database schema rendered directly to HTML: every state variable gets its own bordered box, every action gets a heavy button, and nothing shares a baseline or a margin. T3 Code and DeepSeek Harness feel calm because **80% of their chrome has no borders at all**—they rely on alignment, spacing, and text contrast to do the work that OX is trying to do with 1px strokes and filled rectangles.

Here are 14 specific, implementable changes to fix it.

### 1. Kill the boxed-everything disease in the sidebar top stack
In OX, the banner, project selector, `New session` button, `Search threads...` input, and the three filter tabs (`Running`, `Needs you`, `Finished`) are stacked like bricks with 4–6px gaps and borders on nearly all of them.
* **Change:** Remove the borders and backgrounds from the filter tabs (`Running`, `Needs you`, `Finished`) and the project/provider rows. Give the sidebar a single **12px outer horizontal padding** so the left and right edges of `New session`, `Search`, and the thread list share one strict vertical keyline (right now the banner, dividers, buttons, and list items all start and end at different X coordinates).

### 2. Move the composer into the center on the empty state (copy T3 and DeepSeek)
OX pins a huge empty-state hero in the middle (`What shall we work on?` + 3 buttons) *and* leaves a full-width, docked bottom composer (`Ask Claude Code...`) spanning ~1600px across the bottom of the screen. The user has two competing places to start a session, 400px apart.
* **Change:** On the empty state (`New session`), move the composer box directly under `What shall we work on?`, centered horizontally at **`max-width: 680px`**. Only dock the composer to the bottom once a thread has messages—and even then, constrain its width to **`760px` centered** inside the main pane instead of stretching edge-to-edge with 12px margins.

### 3. Delete the duplicate primary CTA (`New session` appears twice)
You have a bright blue filled `New session` button in the top-left sidebar and an identical bright blue filled `New session` button in the center empty state—even though the user is *already on* the new session screen.
* **Change:** Delete the center row of three buttons (`New session`, `Start worker`, `Open project...`). Focusing the centered composer *is* starting a new session. If `Start worker` is a distinct mode, make it a mode toggle chip inside the composer (like DeepSeek's `Standard mode ▾` above the input).

### 4. Unify control heights to a strict 28px / 32px / 36px scale
Look at the heights in Image 1: the `>_` button in the header is ~28px, `New session` is ~34px, `Search threads...` is ~32px, `Running / Needs you / Finished` are ~30px, `End all idle` is ~30px, `Settings` is ~28px, the model pills are ~24px, and `Stop / Send` are ~32px. Nothing matches.
* **Change:** Standardize on three heights across the entire app:
  * **32px** for primary buttons and inputs (`New session`, `Search threads...`, `Send`).
  * **28px** for secondary/header icon buttons (`>_`, filter tabs).
  * **24px** for inline metadata pills inside the composer (`Model`, `Permissions`).

### 5. Strip the borders off the composer's bottom bar (`Model`, `Permissions`, `Stop`, `Send`)
The bottom row of the composer has four separate bordered rectangles inside a bordered parent rectangle. In T3 and DeepSeek, controls inside the composer are borderless ghost triggers (`6.2 Sol High`, `Workspace Write ▾`) plus a single circular/square submit icon button (`28×28px`).
* **Change:**
  * Turn `Model Claude Code · Claude plan · default` and `Permissions Next thread · Default` into **borderless ghost buttons** (`background: transparent`, hover `rgba(255,255,255,0.05)`, `height: 24px`, `padding: 0 8px`). Drop the prefix words `"Model"` and `"Permissions"` and add a small chevron `▾`—the values (`Claude plan · default` and `Default permissions`) are self-explanatory.
  * Hide the `Stop` button completely when no turn is running (showing a disabled `Stop` button next to a disabled `Send` button is pure backend-dev UI).
  * Replace the text `Send` button with a **`28×28px` icon button** (`↑`) at the bottom-right corner of the composer, matching T3 and DeepSeek.

### 6. Fix the center-aligned `oscar ▾` / `Claude subscription ▾` header block
Center-aligning two dropdowns of different widths in a left-aligned sidebar breaks the vertical scanline completely. It also duplicates the breadcrumb (`■ oscar / Claude subscription`) at the top-left of the main pane.
* **Change:** Delete the top-left breadcrumb in the main pane (`■ oscar / Claude subscription`)—or keep it in the main pane header (like T3's `No project / New thread`) and remove `Claude subscription ▾` from the sidebar. In the sidebar, left-align the workspace selector (`oscar ▾`) on the exact **12px left keyline** with a `28px` row height, matching DeepSeek's `Default workspace`.

### 7. Collapse the double-footer in the sidebar (`RUNNING · 1` appears twice in 80px)
Look at the bottom-left of Image 1:
1. A section header `RUNNING · 1` with a bordered `End all idle` button.
2. A two-line running item (`Codex opencode-go/gemin...` / `oscar · up 2m · idle 2m`) with a floating `End` text button.
3. A bottom bar with a bordered `Settings` button and *another* `RUNNING · 1 🟩` indicator.
* **Change:**
  * Delete the bottom `RUNNING · 1 🟩` label completely.
  * Replace the bordered `Settings` button with a borderless icon+label row (`⚙ Settings`, `height: 32px`, left-aligned to the 12px keyline, like DeepSeek's `••• More` or T3's bottom icon rail).
  * Move active running processes into the `Running` filter tab or pin them at the top of the thread list under a clean section label (`Running  1`), with `End` appearing **only on row hover**.

### 8. De-clutter the thread list and align its baselines
Every thread row in OX has a loud colored pixel icon (`16×16px`) on the left, truncated text, and a bright blue `Ended` status badge on some rows (`Ox thread access i... Ended`)—even though the others below it are also ended. Furthermore, the `— Today` header sits on a different left offset than the thread icons.
* **Change:**
  * Align the left edge of `Today` with the left edge of the row items (**12px from sidebar edge**), drop the `—` dash, and set `Today` to `11px`, medium weight, muted gray (`#6B7280`), with `16px` top margin and `4px` bottom margin.
  * Remove the `Ended` text badge completely. Finished is the default resting state of a history list; only badge exceptions (`Running` pulse dot or `Needs you` amber dot).
  * Reduce thread row height to **`32px`** with **`8px` horizontal padding**, `6px` border-radius on hover, and mute the pixel agent icons to **60% opacity** until hovered (like T3's subtle monochrome `CB` / `WK` badges).

### 9. Replace the 3 boxed filter buttons with a single segmented control or inline filter
`Running`, `Needs you`, and `Finished` look like three separate action buttons directly under `Search threads...`. Because they share the same border weight and fill as `Search` and `End all idle`, the eye reads five stacked form controls at the top of the sidebar.
* **Change:** Either collapse filters into a filter icon inside the Search row (like T3's `Search` + filter icon in the top-left), or style them as a single **28px-tall borderless tab strip** (`All`, `Running`, `Needs you`) with a subtle bottom underline or pill highlight on the active tab only—zero borders on inactive tabs.

### 10. Tone down the blue fill on `New session`
The bright periwinkle fill (`#6E9EFF`) on the full-width `New session` button screams for attention and fights with the OX arcade banner right above it. Compare with DeepSeek (`New Session` is a subtle elevated dark surface `#26282E` with a 1px border) and T3 (a simple compose icon in the top bar).
* **Change:** Make the sidebar `New session` button a dark translucent surface (`rgba(255,255,255,0.06)` background, `1px solid rgba(255,255,255,0.08)` border, `height: 32px`, `border-radius: 6px`, left-aligned `+ New session` or centered with `8px` icon gap). Reserve the signature periwinkle accent for the OX pixel mascot, focus rings, and the active composer submit button.

### 11. Fix the composer box geometry and padding rhythm
OX's composer has an awkward, cavernous empty gap between the placeholder (`Ask Claude Code about this project...`) and the bottom control row, and sits inside a secondary outer tray/border at the bottom of the window.
* **Change:** Remove the outer tray box around the composer. Give the composer card a single **`1px solid rgba(255,255,255,0.10)`** border, **`12px` border-radius** (or `8px` if keeping a crisper arcade edge), **`12px 16px` internal padding**, a minimum text area height of **`48px`**, and **`12px` gap** above the bottom metadata row.

### 12. Establish a 3-step typography and color hierarchy (right now everything is the same gray)
In T3 and DeepSeek, there are three clear text tiers: bright primary (`#ECEFF4`), mid secondary (`#9CA3AF`), and quiet tertiary (`#646A73`). In OX, almost all labels (`oscar`, `Claude subscription`, `Running`, `Today`, thread titles, placeholder, `Model`, `Settings`) sit in the same mid-contrast slate gray (`~#94A3B8–#CBD5E1`) at `13–14px`.
* **Change:**
  * **Hero title (`What shall we work on?`):** `22px` (currently ~18px and too timid), `#F3F4F6`, `font-weight: 500`, `16px` below the pixel mascot.
  * **Sidebar thread titles:** `13px`, `#D1D5DB` (`#FFFFFF` when active), single-line ellipsis.
  * **Section headers (`Today`, `Running`) & metadata (`up 2m · idle 2m`):** `11px`, `#6B7280`, tabular numerals (`font-variant-numeric: tabular-nums`).

### 13. Stop putting borders inside borders (the 1px stroke audit)
Count the nested boxes at the top-left of OX: the window border -> the sidebar border -> the arcade header box -> the `OX` logo box + the `>_` button box -> a horizontal `<hr>` divider -> `New session` box -> `Search` box -> 3 filter boxes. That is **9 bordered rectangles in a 240×180px corner**.
* **Change:** Enforce a hard rule: **no horizontal `<hr>` dividers inside the sidebar**, and **never place a bordered control inside a bordered container** unless it is an input field. Use **`16px` vertical spacing** to separate the header, the primary action/search block, and the thread list.

### 14. Concentrate the 80s arcade identity into 3 signature moments instead of the whole UI chrome
Right now the arcade theme leaks into boxy 90s-style button borders, heavy pixel badges on every row, and a clunky boxed banner at the top-left. That's what makes it feel amateurish rather than intentional.
* **Change:** Keep the modern UI shell as quiet and borderless as T3/DeepSeek, and put 100% of the 80s arcade soul into three high-craft places:
  1. **The sidebar header scene:** Remove the rectangular border around the moon/landscape banner so the pixel art bleeds edge-to-edge across the top of the sidebar (`height: 48px`), with the `OX` pixel mark at `12px` left and a borderless `>_` terminal button at `12px` right.
  2. **The center empty-state mascot:** Keep the blue pixel ox/dog above `What shall we work on?` (it's genuinely charming, just like DeepSeek's whale), and let it animate subtly on idle/thinking.
  3. **The 8px×8px pixel glyph icons** for thread providers—kept small (`12–14px`), monochrome or muted in the sidebar list, and full-color only on the active thread or while actively streaming.

## Measured misalignments (full-res crops)
- Filter chip row ~8 px wider than search and New session on both sides.
- Centred project/route block next to a left rule; everything else left-aligned.
- 'Today' header, thread rows and controls on three different indents.
- Footer mixes bordered 'End all idle', borderless 'End', bordered 'Settings'; RUNNING count shown twice.
- '>_' header box not on the OX logo baseline or height.
