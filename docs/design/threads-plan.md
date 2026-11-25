# OX threads: dogfood fixes, session control, bug hunt

Status 2026-11-25. Based on `origin/main` at `2708f55`. Worktree branch `fix/ox-thread-dogfood`.
Companion design plan: [polish-plan.md](polish-plan.md) (41 UI/UX findings, 4 waves).

## How a thread like this runs today

```
OX Desktop / phone ──host protocol──► ox-host (engine.rs, one per user)
                                         │
     per Claude session ─────────────────┼─► ox claude -p --input-format stream-json … (links/claude.rs)
     per project × Codex route ──────────┼─► ox codex … app-server (links/codex.rs)
                                         │
     agent calls MCP ox_workers ─────────┴─► host-owned worker session (#5), lease via host_lifetime.rs
```

- A Claude thread is one stock Claude Code process talking stream-json. OX checks the account and
  version on `initialize`, then every `system/init` frame.
- A Codex scope is one app-server per project and route. Several threads share it.
- Workers are host-owned sessions started via `start_ox_worker`. Each helper is its own process
  group and holds a lease socket to the host. When the lease drops, the host kills the group.

## Fixed in this branch (uncommitted, tests green on main)

| Dogfood symptom | Cause | Fix |
|---|---|---|
| Thread died: "Claude Code opened a different project or session" | Claude Code repeats `init` every turn with the Bash tool's *current* cwd. The thread had run `cd ~/Work/ox/desktop` | Only the first `init` must match the project; later ones only the session id. `links/claude.rs`, regression test in `tests/claude_link.rs` |
| Worker: "the input_image tool output part is not supported by the chat completions adapter" | Chat Completions `tool` messages are text-only; images from `view_image`/screenshots errored | Tool images follow the last tool result as one user message; `[image attached below]` placeholder. `src/adapters/chat_completions.rs` + test |
| Gemini Argon: "The selected API billing route is no longer configured." | Host only allowed static + starred models | Any catalog model may open; `ox --model` validates. Only "no API provider" stops. `ox-host/src/runtime.rs` |
| Agents could not tell favourites | `list_ox_models` unordered | Starred first, `starred: true`, tool description says to prefer them. `src/workers.rs` |

## Running threads like this reliably: rules

1. **Assume stock protocols only for what they promise.** Anything that may change mid-session (cwd,
   model, compaction, resume/fork, subagents) must not be a fatal check after turn 1.
2. **A recoverable error fails a turn, not a session.** An upstream overload, `max_tokens` or a refused
   action reports back and keeps the process/socket alive.
3. **Every model works; stars only sort.** Same for humans (picker) and agents (`list_ox_models`).
4. **Errors reach the place the user is looking**, with the actual fix named.
5. **Everything OX starts, OX can list and end.** No orphan processes.

## Ending Claude and Codex sessions started from OX

Today: Stop only interrupts a turn (`store/compose.rs` `stop` → `interrupt`). Nothing removes a scope
from `engine.running`; a Claude process ends only on terminal handoff or host exit. A Codex
app-server lives per project × route until the host exits. Observed: 55-minute-old worker app-servers
still running.

Plan (wave 2, ~3 h agentic):

1. **`EndSession` host action** (desktop, phone, protocol capability next to `open, send, steer, stop, access`).
   - Claude: drop the session's `Link`. `Process::drop` already sends SIGTERM, then SIGKILL, to the group. Keep
     the thread in the catalog as resumable (`--resume` on next send).
   - Codex: unsubscribe the thread; when a scope has no subscribed threads and no running turn,
     drop the `Live` store, which kills the app-server group.
2. **Workers die with their parent.** Fix the dead lease cascade: export the gateway's own
   lease id as `OX_HOST_PARENT_LEASE` on worker commands (`src/workers/jobs.rs` command build). Nothing
   sets it today, so a crashed parent leaves its workers running (bug-hunt finding, verified).
   Ending a parent ends its workers. "End worker" in the UI maps to the existing `ParentCall::Cancel`.
3. **Cancel while a worker is still launching** must actually abort the launch (today it is a no-op
   returning `starting`, `engine.rs:1621`).
4. **Running panel**: an always-visible "RUNNING · n" list (desktop sidebar + phone) of live
   processes: kind, model, project, age, idle time, with End per row and "End all idle".
   Builds on main's `is:running` filter. Design in polish-plan.md (c).
5. **Idle reaper**: end Claude processes and unused Codex scopes idle > 30 min (setting). The thread
   stays resumable.
6. **CLI escape hatch**: `ox ps` / `ox end <id|--idle|--all>` talking to the host socket, for when the
   UI is gone.

## Bug hunt (16 Haiku hunters on main, 3-skeptic vote; 44 raw → 11 confirmed)

Confirmed (2–3 of 3 skeptics upheld). ★ = spot-checked in code by Claude.

| # | Sev | Where | Bug | Fix |
|---|---|---|---|---|
| 1 | high ★ | `ox-host-web/src/bridge.rs:232` | Any refused phone action (e.g. invalid worker form) `break`s and drops the whole phone socket; the user sees "Disconnected before confirmation" instead of the reason | Reply `Server::Error{id,message}` and keep the socket open; break only on auth/protocol violations |
| 2 | high ★ | `src/adapters/anthropic.rs:1856` | `max_tokens` cut inside a `tool_use` block → invalid-JSON error fails the stream instead of `response.incomplete` | Defer argument validation to `message_stop`; map `max_tokens` to incomplete |
| 3 | med ★ | `src/workers/jobs.rs:551` | One `is_error` result (overload, max turns) kills a Claude worker permanently | Fail the turn, keep the process steerable |
| 4 | med ★ | `src/adapters/chat_completions.rs:1199` | Providers that omit tool-call `id` → `call_id: ""`; parallel calls collide | Synthesize a stable `call_<index>` id |
| 5 | med ★ | `src/main.rs:796` | One discovered model id with a space/quote aborts `ox codex`/`ox claude` for every provider | Skip invalid discovered ids with a note; hard-fail only static config |
| 6 | med | `store/catalog.rs:118` | Workers running in a subdirectory vanish on list refresh and are not rediscovered (another un-nesting cause) | Keep rows still in `child_threads`; re-run child discovery |
| 7 | med | `ox-host-web/assets/app.js:1198` | Whitespace-only worker task/handoff request passes `required`, then drops the socket | Trim + inline message; host side fixed by #1 |
| 8 | med | `store/replica.rs:399` | Any unrelated host error cancels auto-select of the session being opened; user stays on blank draft | Track the opening request id |
| 9 | med | `ox-host/src/engine.rs:1065` | Terminal handoff lost if the requesting client reconnects; thread stuck in terminal mode | Retarget `live.terminal` on replay/disconnect |
| 10 | med | `ox-host/src/engine.rs:1621` | Cancel during worker launch is a silent no-op | See session plan step 3 |
| 11 | med | `store/notifications.rs:95` | Settings-mismatch path leaves a stale `active_turn`; thread can look running forever | Reconcile `active_turn` from snapshot in `add_history` (1 skeptic disputed reachability; confirm with a test) |
| 12 | med ★ | `src/host_lifetime.rs:43` | Parent lease cascade never fires (`OX_HOST_PARENT_LEASE` never set) | Session plan step 2 (contested 1/3 by Haiku, verified by Claude) |
| 13 | med | `src/provider.rs:489` | Env key with trailing whitespace hides a valid saved key, error says "set X" although it is set | Trim / fall through to saved key; distinct message (contested 1/3) |

Low-severity, unverified (16): fix opportunistically while in the file. Full list with scenarios in
`bug-hunt-findings.json`; highlights: unknown `finish_reason` fails a turn after the answer streamed
(`chat_completions.rs:1350`), no idle bound after missing `[DONE]` (`:1005`), `stop_sequences`
forwarded as an invalid Responses field (`claude_request.rs:190`), one malformed historical
`function_call` arguments string poisons every later Anthropic request (`anthropic.rs:452`), the
title-inference process group is not killed on success (`titles/inference.rs:155`), and a Codex worker is
SIGKILLed right after `turn/completed` with no flush (`jobs.rs:725`).

## Decisions for Oscar

1. Write-mode Claude workers: Bash removed (honest: "Accept edits" never approves Bash, so it always failed). Downside: they cannot run tests/builds. Option: allow a narrow `Bash(cargo *)` etc.
2. YOLO confirmation: desktop sheets now need an explicit "I understand" tick; phone keeps main's convention (choosing YOLO = confirmation). Pick one rule for both.
3. Idle reaper default 30 min also ends sessions open in a client (they resume on select). Keep, lengthen, or skip selected ones?

## Waves (agentic wall-clock)

| Wave | Scope | Size |
|---|---|---|
| 0 | Land this branch (4 dogfood fixes) | done, needs PR + dogfood build |
| 1 | Bugs 1–5, 7, 12 (socket errors, truncation, worker survives errors, call ids, discovery, lease cascade) + polish wave 1 | ~3 h |
| 2 | Session control (EndSession, cancel-launching, Running panel, idle reaper, `ox ps/end`) + bugs 6, 8–11 + polish wave 2 (model picker) | ~6 h |
| 3 | Polish waves 3–4 (worker/handoff sheets, type/spacing/kit, icons, motion, pairing) + lows | ~9 h |

Hardware/human check after wave 1 and wave 2. Rerun the original dogfood: Claude thread in OX → Gemini
worker → `cd` → screenshots → End session; check that no process survives.

## Progress log (hand-off point — keep current)

How to continue (any agent or a human):
- Worktree `~/Work/ox-wt-dogfood`, branch `fix/ox-thread-dogfood`, one PR for all waves. Rebase on
  `origin/main` before starting a wave; never work in `~/Work/ox` (live agents use it).
- Build/test with shared caches: root `CARGO_TARGET_DIR=~/Work/ox/target cargo test -q`;
  desktop `cd desktop && CARGO_TARGET_DIR=~/Work/ox/desktop/target cargo test -q -p ox-desk-core -p ox-host`
  (add `-p ox-desktop -p ox-host-web --features ox-host-web/server` when those change), plus `cargo clippy --all-targets` and `cargo fmt --all` in both.
- Per wave: implement → self-review the diff (correctness against the finding's scenario, a regression
  test per bug) → fix review findings → tests green → one commit per wave → update this log.
- Bug details and scenarios: `bug-hunt-findings.json` (match on file + title). UI items: `polish-plan.md` §4 numbers.

| Date | Wave | State | Commit | Notes |
|---|---|---|---|---|
| 2026-11-25 | 0 dogfood fixes | done | 767c454 | rebased on c8ff6a1 (#8 CI hardening) |
| 2026-11-25 | 1 bugs 1–5, 7, 12 + polish wave 1 | done | dd93b86 | Reviewed by Claude: phone deny styling moved from :last-of-type to a `deny` class by label. Follow-ups: phone settings.css still has literal light colours; phone Stop should be disabled rather than hidden in app.js (~906); desktop filter chips have no counts yet; truncated Anthropic tool item gets no output_item.done (by design, never enters history); ox-host-web tests need `--features server`. Human check pending: approvals card, Stop slot, phone stacked decisions, light theme status bar. |
| 2026-11-25 | 2 session control + bugs 6, 8–11 + polish wave 2 | done | 56c1f05 | EndSession + `end` cap, ListProcesses/ProcessView, idle reaper (`OX_HOST_IDLE_MINUTES`, default 30, 0=off), parent ends workers, cancel-while-launching, held terminal handoff, await_new by request id, catalog keeps subdir workers, active_turn reconcile, shared ModelPicker (Codex/Claude toggle), desktop RUNNING panel + End in thread menu/palette, phone Running view + grouped ••• menu + in-page confirm + Stop disabled-not-hidden, `ox ps`/`ox end` (src/ps.rs, now the single host socket path impl), tree rows/fold/counts, worker header, parent '"N workers running"'. Review fixes: quiet Running panel without host; deduped socket path; browser.py menu-order assertion. All root+desktop tests, clippy, phone check.sh (headless Chromium) pass. Gaps: no engine test for ending a Codex thread with ready transcript; no "Ended — select to resume" chip (desktop/phone); Favorites page inline picker/drag/autosave; tree connector lines/icons; reaper may end a session open in a client (resumes on select). |
| 2026-11-25 | 3a sheets, phone polish, low bugs | done | f3d004c | Desktop: Start worker + Continue in… sheets (views/worker_sheet.rs, handoff_sheet.rs; YOLO needs an explicit "I understand" tick), handoff link cards, YOLO grant in the permission chip. Phone: bottom-sheet worker/handoff forms with starred-first model select, handoff cards, sticky needs-you bar, ask max-height, 3-step pairing, dismissable notification banner (sw cache v7). Root lows fixed: tool_choice without tools, unknown finish_reason, historical bad args wrapped, stop_sequences dropped, ambiguous bare --model at launch, env key whitespace (+ ox doctor wording), Codex worker 2 s flush, write-mode Claude workers no longer offered Bash. Desktop-core lows fixed: title inference group kill + bounded wait, refused acts not cached, YOLO project default survives a failed read, scope-less subscribers get catalog-only deltas, refused subscribe answered with error, new-chat images stay with text. Review: flaky title test hardened against ETXTBSY. All tests, clippy, phone check.sh pass. |
| 2026-11-25 | 3b gaps + desktop system pass | done | 3a0193e | Stream end bounded 5 s after finish_reason (src/upstream.rs), desktop "Worker waits for YOLO" ask card (no duplicate notice), Codex end-session store test, "Ended" marker desktop+phone (in-memory per client). Desktop: six-step type scale (Step, scales with text-size pref; Label=Meta=12 px), kit grid constants (24/32/40, radius 2, 4 px spacing), kit::chip and kit::input, preferences button retired, sidebar footer with RUNNING count + host dot, starred models autosave. Review fix: star edits during a save are kept and re-saved. All tests, clippy, phone check.sh pass. |
| | 4 after PR | todo | — | Human check on laptop+phone (approval card, Running panel/End, sheets, light theme, no orphan processes after End, type scale at 14/18/20). Leftovers: Departure Mono (verify OFL, then bundle as LABEL_FONT), icons + motion (polish 2.4, 2.5), empty states, alias-resume row (4), narrow-width Session chip and header status chip (5, 6), Providers/Search autosave decision (32), Settings back chevron (33), remaining rounded_*/spacing one-offs, `ox snapshot` off the host actor thread, scope-less subscribe snapshot still full state, persistent "ended" marker. |
