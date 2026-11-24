# Stock Codex runtime checks

The current compatibility floor is stock Codex 0.171.0, also the standalone OX CI floor. At the start of the migration the latest stable was 0.172.0. Both binaries generated their experimental schemas locally and passed the method and field checks in `contract_protocol.rs`. Both passed the existing desktop loopback cases for native billing, API billing, wrong authentication, legacy providers, approval, denial, interruption, subagents and OX workers. The desktop workflow checks the floor and current release against the same contract.

## Project and billing isolation

A stock app-server launched by OX in project A accepted a thread started with the absolute path of B. Codex reported B as the thread's directory. A real OX Codex worker launched by that thread executed a relative read and write. It copied A's unique input into a marker **inside A**. Launching a separate app-server from B made the same worker copy B's input inside B. The fixture observed this on 0.171.0 and 0.172.0.

Key long-lived connections by canonical project directory and billing route. An existing connection can serve multiple threads in that project and route. A thread's reported cwd alone cannot establish the worker boundary. Route changes must preserve native account checks and the relay's selected model. The fixture checks the native session's account ID and bearer token at its private native endpoint and the worker's distinct provider credentials at its API endpoint.

To reproduce the filesystem check, run the stock session test from the desktop README with `OX_DESKTOP_TEST_CASE=worker-cwd`. Both directories, accounts, credentials and inference endpoints are temporary fixtures.

## Catalog and subscription observations

Two independent stock app-servers opened the same synthetic profile. A substring of the assigned title matched via `thread/list` with `sortKey=updated_at` and requested limits of 1 and 5,000 (the fixture contained one matching thread). The state-database-only and ordinary searches respectively took 4.34 / 6.71 ms on 0.171.0 and 1.62 / 4.50 ms on 0.172.0. These small-catalog measurements establish the request contract. Large-list scrolling is a separate gate.

While one client renamed and ran a resumed thread, a second client that had only listed it received no thread-specific notification before its next listing completed. Subscribe active threads explicitly. Refresh the catalog upon reconnect and when discovering activity from another client.

Both versions returned a login ID and browser authorization URL from `account/login/start` through OX, and accepted cancellation. With an empty home, both versions of the subscription launch exited before initialization. The desktop therefore opens stock terminal sign-in and checks the connection when its window regains focus. On a private Wayland seat, returning early kept sign-in available. After the fixture supplied a synthetic login, returning again connected on both versions. For a live subscription connection, an account-change event prompts the desktop to verify the identity with `account/read`. Real-account browser consent and billing have not been exercised by these loopback tests.

## Concurrent desktop approvals

The Linux window fixture opens Alpha and Beta through the project picker. One stock app-server per project uses the same OX API alias with a loopback provider. Both turns request permission to write a relative file. The fixture approves Alpha from its background request badge and checks Alpha's file exists while Beta's is still absent. It starts and steers an Alpha subagent while Beta is still waiting, then grants Beta’s request with the keyboard and checks Beta's file and final transcript. Returning to the child verifies its follow-up reached the provider and that Stop interrupts the second approval. The run passed locally on Linux ARM64 with Codex 0.172.0. The Linux package jobs repeat it using their packaged desktop and matching OX executable.

Run under Xvfb with xdotool, ImageMagick and Tesseract installed.

```sh
xvfb-run -a -s '-screen 0 1280x900x24' python3 desktop/tests/concurrent_window.py \
  /path/to/packaged/ox-desktop /path/to/stock/codex
```

### Discovering unopened billing routes

Stock Codex 0.171.0 and 0.172.0 each listed two native threads and one API thread in the mixed-profile fixture. Both lists used the project directory and an empty model-provider filter. The native process could see only the native sessions. A fresh relay process listed the saved API session with its `fixture/demo` alias and resumed it with that alias and the `codeox` provider. A native profile’s default model can also be copied into new relay configuration. The desktop explicitly sends the selected API alias in `thread/start` and verifies the response before sending any prompt.

The workbench opens one listing connection per available Codex home for the current project, then combines the results by thread ID. Opening another API alias creates its isolated execution connection on demand. An unavailable provider stays visible. If a relay row has no model and OX has no previously verified route for its ID, choosing an alias opens a new draft. The unverified thread remains in the catalog. A known alias permits a resume after the runtime confirms the route.

## Native runtime boundary

The workbench owns the thread list, transcript rendering, drafts, pending user input, and worker views. A native runtime supplies session identity, history, stream events, and the actions currently available on a thread or worker. Keep its wire names, request matching, authentication, permission semantics, and terminal resume arguments inside its integration. A capability can differ between two sessions using the same executable. For example, the Codex decoder converts an agent message’s `phase` into the shared transcript’s update flag. The renderer receives the author label from the active runtime.

Codex is the reference runtime for this sprint. Its stock protocol and the OX billing route remain authoritative. OX worker controls use their originating parent connection. The shared worker view receives the displayed runtime, messages, and available actions from that connection. Claude Code now maps its stream events and session capabilities to those views. Its stock protocol, billing checks, and session-scoped process model are documented in [Claude Code runtime checks](claude-notes.md).

### Workbench parity with stock Codex

The stock session fixture uses the same Codex launch adapter and store as the visible workbench. On 0.171.0 and 0.172.0 it passed all nine cases on Linux ARM64, including native and legacy API resume, refusal of an API key on the subscription route, the four permission presets, decline and interrupt, native subagents, OX jobs, and worker file operations across two directories. The subscription and API endpoints in this fixture are local.

A child can reach its first approval before it appears in the paged catalog. For each child named by a spawn event, the adapter can request its metadata immediately. The workbench checks the reported parent, project directory and provider before inserting it, and rechecks the identity and effective route when opening the child. On a cold subscription launch that closes before sending a frame, the workbench asks the OX helper for its typed account status and offers native sign-in or provider setup.

### Composer input and model capabilities

Both Codex 0.171.0 and 0.172.0 declare `fuzzyFileSearch` and the `image` and `localImage` input variants on `turn/start` and `turn/steer`. The Linux keyboard fixture selects a file from native Codex search, pastes an image from the Wayland clipboard, and checks the image content in the upstream provider request. An OX fixture alias with `input_modalities = ["text", "image"]` forwards it. Stock Codex replaces images with an omission notice for a text-only alias, so the composer uses the selected model’s reported modalities to preserve the draft and display that limitation.
