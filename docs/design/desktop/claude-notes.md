# Stock Claude Code in the workbench

The desktop compatibility floor is Claude Code 2.1.331. The installed Linux binary completed the checks below using disposable profiles, a local Messages provider and synthetic credentials. OX Desktop runs the installed CLI through its matching OX helper. The stream and control messages cross standard input and output directly from Rust. The Agent SDK's subprocess protocol documents the same boundary. [Claude Code headless usage](https://code.claude.com/docs/en/headless) · [Anthropic Python SDK transport](https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/transport/subprocess_cli.py) · [SDK control client](https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/query.py)

## Launch, account and session identity

| Selection | OX command | Evidence checked before sending a user message |
| --- | --- | --- |
| Claude subscription | `ox claude --subscription` plus the stream arguments | OX runs `claude auth status --json` with the same account routing settings and requires a signed-in `claude.ai` account on `firstParty`. The initialized session advertises its subscription and has no active API key source or routing bearer. |
| OX API alias | `ox claude --model <alias>` plus the stream arguments | Stock runs in OX's Claude relay profile and reports `tokenSource: ANTHROPIC_AUTH_TOKEN`, `apiProvider: firstParty`. The bearer is scoped to the local OX relay. OX resolves its upstream provider key. |

A cold profile reported `loggedIn: false` and `authMethod: none`. A profile with only a synthetic API key reported `api_key`. Native sign-in launches the stock `claude auth login --claudeai` in a terminal. Only the stock client handles login. OX honors managed policy and clears conflicting API settings in the child for a subscription launch. Anthropic distinguishes signing in to the unmodified CLI from offering a product's own Claude.ai login or intermediating subscription credentials. [Claude Code authentication](https://code.claude.com/docs/en/authentication) · [Anthropic's authentication and credential-use terms](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use)

Each live session owns one OX process group and one writer for stdin. Its working directory is the canonical project. New sessions use `--session-id UUID`, resumes use `--resume UUID`, and forks use `--resume SOURCE --fork-session --session-id NEW_UUID`. In the loopback check, stock returned the original UUID after a cold resume and the requested new UUID after a fork. The link also checks the reported project and session in the first system init frame.

## Wire contract

The common arguments are `-p --verbose --input-format stream-json --output-format stream-json --include-partial-messages --replay-user-messages --permission-prompt-tool stdio`. Each message occupies one UTF-8 JSON line. The client flushes every control reply, user message and interrupt independently of the display frame clock.

1. OX Desktop sends a `control_request` with a request ID and `request: {"subtype":"initialize","forwardSubagentText":true}`. Stock replies before receiving a prompt. The success payload includes `claude_code_version`, commands, models, account, current permission mode and capabilities. The desktop checks this response against the `[claude]` floor in `desktop/gate.toml`. Missing version information or a different billing route closes the session.
2. A `user` message has a client-generated UUID, session ID, null `parent_tool_use_id`, and `message: {"role":"user","content":[...]}`. Stock emits `system/init`, partial `stream_event` frames, completed `assistant` / `user` envelopes and a terminal `result`. Use native message IDs and content-block indices to reconcile deltas with final blocks.
3. For `request.subtype=can_use_tool`, reply with `{"type":"control_response","response":{"subtype":"success","request_id":"…","response":{"behavior":"allow","updatedInput":{…}}}}`. A denial uses `{"behavior":"deny","message":"…"}` as the inner response. For AskUserQuestion, preserve the requested input and add `answers` under `updatedInput`. Permission request IDs belong to the originating process. Two processes can issue the same ID.
4. `interrupt`, `set_permission_mode` and `set_model` are control-request subtypes. Their success payloads included `still_queued`, `mode` and `model` respectively. A follow-up submitted while a Bash approval was pending reached the next model step of the original turn after denial. Stock produced one final result. A successful enqueue alone cannot establish when the model reads a follow-up.
5. In the stock subagent fixture, nested messages carry a `parent_tool_use_id` for the Agent invocation. Keep these under their parent. Native final/tool records carry the displayable output. Portable transcript items do not own runtime credentials or a second connection.

## Permission modes

A new session nobody chose a mode for starts without `--permission-mode`, so Claude Code applies its own configured default exactly as `claude` does in a terminal (for an eligible account this can be `auto`). The store reads the effective mode from the initialize reply (`current_permission_mode`) and the `init` event, so the composer shows the real mode. **Settings › Accounts › Agents › New Claude sessions** (`claude_start` in the shared Desktop preferences) picks a fixed mode instead; a mode chosen in the composer, a resume's recorded mode and a worker's contract or preset all take precedence. Codex works the same way with `codex_start`: unset, a new thread uses the Codex configuration. The terminal handoff (`terminal_args`) also passes no mode.

`--permission-mode default --allow-dangerously-skip-permissions` initialized in `default`. An explicit control request to switch to `bypassPermissions` yielded `mode: bypassPermissions`. Switching back yielded `mode: default`. An explicitly requested bypass launch uses stock `--dangerously-skip-permissions`. Advance the visible selection on confirmation. Native policy or platform restrictions can reject a switch. The picker also exposes `plan`, `acceptEdits` and `auto`. On stock 2.1.331 the Auto selection returned `mode: auto`. With `permissions.disableAutoMode` set to `"disable"`, stock rejected that request and the picker retained its previous mode. Auto uses a classifier to review actions. Availability depends on the active model, provider and policy. [Claude Code permission modes](https://code.claude.com/docs/en/permission-modes)

## Catalog and verification

Request `claude_profiles` through the OX desktop settings port to discover the two stock session roots without creating either one. Native sessions use the stock config directory. API sessions use the OX relay profile. Stock JSONL history under each profile's project directory supplies catalog and transcript records for closed processes. In the API fixture the top-level persisted `requestedModel` was the OX alias `fixture/demo` while `message.model` was the upstream name `demo`. Require the saved route binding or a verified alias before charging a resumed API session.

Run `cargo test --locked --manifest-path desktop/Cargo.toml -p ox-desk-core` for decoder and transport cases. The stock session harness exercises the installed OX helper and Claude Code with isolated project directories and local endpoints. Invoke `python3 desktop/tests/claude_stock_session.py /absolute/path/to/ox /absolute/path/to/claude`.

## Saved conversations and billing identity

Stock Claude Code 2.1.331 created session JSONL under each selected profile’s `projects/<encoded-cwd>/` directory. Names replace each non-ASCII-alphanumeric UTF-16 code unit with a dash. Inputs over 200 encoded units use the first 200 and a hash suffix. Distinct directories can encode to the same name. OX checks the canonical cwd and session ID in the conversation before listing or resuming it. The read-only exception in `desktop/ci/migration.toml` covers these session files. Only stock Claude performs resumes, forks and transcript writes.

On an actual OX API run selecting `fixture/demo`, the assistant message recorded the upstream model `demo`. The enclosing JSONL record preserved `requestedModel: "fixture/demo"`. The latter can identify a configured alias. An explicit OX billing binding is stored separately, keyed by project, canonical relay profile and session UUID. Two desktop processes cannot reassign the same binding. If both profiles contain the same UUID, the combined catalog returns an ambiguous origin and requires an explicit profile before reading history.

A stock fork created a new UUID and copied the original messages under that identity. A stock Agent call wrote a separate `<session UUID>/subagents/agent-<agent ID>.jsonl`. The parent tool result provided the agent ID and tool-use ID. OX replays the child's verified sidechain as nested events under that tool. It does not follow the tool's external output-file path.

The loopback stock suite checks both directions of billing isolation. A private HTTPS CONNECT proxy accepts only the fixture’s native Anthropic host and validates the synthetic OAuth bearer. The API endpoint checks a distinct vendor credential and model. A Console key without a saved subscription is refused. Two projects request writes concurrently. Approving one and declining the other produce only the first project's marker. Interrupt, full process restart, native fork and a nested stock subagent are also exercised. Run `python3 desktop/tests/claude_stock_session.py /path/to/ox /path/to/claude` with Python 3 and OpenSSL.

## Linux release measurements

On a private headless Sway compositor, the release workbench took 184.24 ms at p90 to open the first window across ten launches. The GUI drew zero frames and consumed zero CPU ticks during 60.01 seconds idle with Claude attached. Four simultaneous sessions each streamed 200 KB at 200 deltas per second while a typed draft remained visible. Frame draw time was 4.08 ms at p95 and the Stop click reached the child’s stdin in 0.217 ms. The process, settings, state and log scans found zero occurrences of the upstream credential canary. The measurement uses the release OX helper and a deterministic stream-json child. The stock suite above covers actual Claude Code behavior.

An independent Xvfb run measured 161.17 ms cold p90, 3.57 ms draw p95 and 0.129 ms Stop latency. Its 60-second idle sample used 0.767% GUI CPU, exceeding the 0.5% budget. A follow-up X11 idle trace counted zero frames, notifications, drains and process starts, with roughly 63 voluntary GUI wakeups per second. The private Wayland check is the CI reference for native Linux performance.

Build `ox-desk-probe` and its fixture binaries in release mode. Run the gate from the repository root.

```sh
dbus-run-session -- desktop/ci/wayland-smoke.sh --run \
    desktop/target/release/ox-desk-probe gate --wayland --scenarios claude \
    --app /path/to/packaged/ox-desktop --ox /path/to/packaged/ox \
    --out /tmp/claude-gate.json
```
