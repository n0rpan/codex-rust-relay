# Stock Codex 0.171.0 smoke fixtures

`tests/stock_codex_smoke.rs` serves these shapes from loopback fixture servers
and drives the real installed Codex CLI through `codeox`. A loopback Brave
fixture also answers the relay's direct search requests for the four search
scenarios. The fixtures are generated in Rust from the shapes observed with
stock Codex 0.171.0; this file records what was observed and why each assertion
exists.

## Observed stock shapes

* Codex calls the relay at `POST /v1/responses`; translated vendors receive
  `POST /v1/chat/completions` or `POST /v1/messages`.
* Codex always sends `store: false`, `stream: true`,
  `include: ["reasoning.encrypted_content"]`, `reasoning: {effort, summary}`,
  and its usual `client_metadata` (thread, session, turn and installation ids).
  A request after `-m first/one-model` carries the bare upstream id
  `one-model`: stock Codex resolves the provider-qualified alias before it
  reaches the relay.
* `apply_patch` is advertised to the model as a freeform tool:
  `{"type":"custom","name":"apply_patch","format":{"type":"grammar",...}}`.
  Codex only advertises it when the model catalog entry carries
  `"apply_patch_tool_type": "freeform"`. The relay sets that for translated
  wires and deliberately does not for Responses, so the Responses primary uses
  a plain `exec_command` edit.
* A freeform call streams as `response.output_item.added` with
  `item.type = "custom_tool_call"`, then
  `response.custom_tool_call_input.delta` / `.done`, then
  `response.output_item.done`, then `response.completed`.
* When Codex executes that call it reports an `item.completed` event of type
  `file_change` with `changes[].kind = "update"` and the absolute path.
* Codex returns tool results on the next relay request as
  `function_call_output` / `custom_tool_call_output` items (Responses),
  `role: "tool"` messages (Chat Completions), or `tool_result` content blocks
  (Anthropic Messages). Namespaces are flattened by the relay; `apply_patch`
  itself is a function in every translated upstream request.
* `exec_command` results include `Chunk ID`, `Wall time`, `Process exited`,
  and `Output:` framing. Assertions look for the meaningful output text rather
  than the whole frame.

## Observed standalone web search

The search scenarios declare a `[search]` table, so the relay passes Codex
`supports_standalone_web_search=true` and `features.standalone_web_search=true`
(and `web_search="live"` when the user did not pin a policy). Stock Codex then:

* advertises its `web` namespace with the `run` function. On the Responses wire
  the vendor receives that namespace unchanged; on Chat Completions and
  Anthropic Messages the relay flattens it to the function name `web__run` and
  maps the streamed call back to the namespace.
* posts the call to the provider base URL, i.e. the relay's
  `POST /v1/alpha/search`, with `commands.search_query`, Codex's
  `settings.external_web_access`, and a recent-conversation tail the relay
  ignores.
* receives `{"encrypted_output": null, "output": "...", "results":
  [{"type": "text_result", "ref_id": "turn0search0", "url": ..., "title": ...,
  "snippet": ...}]}` from the relay and reports the search action in `--json`
  as an `item.completed` of `type: "web_search"`.
* replays the search call and its output to the vendor in the next request and
  on a later resume, in that wire's own tool-call shape.

The citation assertions inspect the tool output Codex replays to the vendor.
They require the reference IDs, URL, title, and snippet supplied by Brave.
The CLI's `web_search` progress event can omit its `results` field.

The loopback Brave fixture serves the LLM Context shape and records the path,
query, and `x-subscription-token`. Reaching it requires the `test-brave` feature
in the relay binary, which is a test-only seam: production builds always use
Brave's real HTTPS origin and have no endpoint override, and `[search]` config
has no base-URL key.

The fixture starts the feature build with the dummy key `brave-fixture-key`
and sets `CODEOX_TEST_BRAVE_BASE_URL` to its loopback listener. That override
requires the fixture key. The test client bypasses proxies for loopback.

## Vendor-specific reasoning state

The cross-wire mocks add the state that makes a wire switch unsafe:

* The Anthropic mock requests `thinking: {type: "enabled", budget_tokens:
  1024}` through `anthropic_thinking = "manual"` and returns a signed
  `thinking` block before its tool call. The relay stores it in a reasoning
  item's `encrypted_content` as a versioned, provider-tagged envelope beginning
  `codeox.anthropic.thinking.v1:`. On the same vendor the adapter restores the
  signed block; the fixture asserts the signature is replayed.
* The Responses mock returns its own reasoning item with
  `encrypted_content: "enc-fixture-responses-vendor-one"`. The same-vendor
  resume must replay it.

A second vendor may receive the visible history (prompts, calls, results,
final assistant text) but must not receive the first vendor's reasoning item,
thinking signature, or ciphertext. Each cross-wire case asserts that positive
control first, so an absent marker cannot pass vacuously.

## Cases

| Primary wire | Edit tool | Secondary wire | Resume after relay restart | Switch to second vendor |
| --- | --- | --- | --- | --- |
| Responses | `exec_command` write | Responses | yes | yes, second Responses vendor |
| Chat Completions | native `apply_patch` | Chat Completions | yes | yes, second Chat vendor |
| Anthropic Messages (`2023-06-01`) | native `apply_patch` | Anthropic Messages | yes | yes, second Messages vendor |
| Anthropic Messages (`2026-10-21`) | native `apply_patch` | Anthropic Messages | yes | yes, second Messages vendor |
| Anthropic Messages (`2023-06-01`) + thinking | native `apply_patch` | Chat Completions | yes | yes, foreign thinking dropped |
| Anthropic Messages (`2023-06-01`) + thinking | native `apply_patch` | Responses | yes | yes, foreign thinking dropped |
| Responses + native reasoning | `exec_command` write | Anthropic Messages | yes | yes, foreign ciphertext dropped |
| Chat Completions | native `apply_patch` | Anthropic Messages | yes | yes |

Search cases (one vendor, `--features test-brave`), each one coding task plus
`web.run`/Brave citations and a relay restart:

| Wire | Edit tool | Resume | Brave request | Citation result |
| --- | --- | --- | --- | --- |
| Responses | `exec_command` write | yes | `/res/v1/llm/context` | `turn0search0` |
| Chat Completions | native `apply_patch` | yes | `/res/v1/llm/context` | `turn0search0` |
| Anthropic Messages (`2023-06-01`) | native `apply_patch` | yes | `/res/v1/llm/context` | `turn0search0` |
| Anthropic Messages (`2026-10-21`) | native `apply_patch` | yes | `/res/v1/llm/context` | `turn0search0` |

Every switching case asserts:

* the upstream model is the vendor's own id, not the Codex alias;
* the vendor's own key is used (`Authorization: Bearer` or `x-api-key`, never
  both for Anthropic);
* the configured `anthropic-version` reaches the vendor;
* Codex actually changed `fixture.txt` and the check command printed
  `CODEOX_CHECK_PASSED`;
* the resume request on the first vendor and the switch request on the second
  vendor replay the original prompt, the three tool calls, their outputs, and
  the assistant's final message, in that vendor's own wire shape;
* the switch request completes with the expected assistant text;
* when the first vendor produced native reasoning state, it is replayed to
  that vendor and absent from the second vendor's request.

Every search case additionally asserts:

* the relay advertised Codex's standalone web tool in that wire's shape
  (`namespace web/run` or the flattened `web__run` function);
* stock Codex reported one `web_search` item whose query is the fixture query
  and whose `results` contain Brave's `turn0search0` URL, title, and snippet;
* exactly one Brave request reached the loopback fixture, at
  `/res/v1/llm/context`, with the fixture query and subscription token;
* the vendor's next request replayed the search call and the citation output;
* the restart/resume request still contained the search call, its citation
  output, and the earlier read/edit/test history.

## Previously failing case, now covered

`stock_codex_anthropic_to_responses_explicit_switch` used to fail because the
Responses pass-through forwarded the Anthropic thinking envelope
(`codeox.anthropic.thinking.v1:`) verbatim. The case now passes:
`translate::sanitize_replayed_history` removes a reasoning item whose
`encrypted_content` is a provider-tagged Anthropic envelope whenever the target
wire is Responses, before the non-translated branch serializes the payload. The
translated adapters already dropped or refused foreign reasoning, and the
same-vendor positive control still proves the Anthropic vendor receives its own
signed thinking back.

## Explicit `-m` on resume

`codex exec resume --json -m second/two-model <thread> <prompt>` does switch
vendors on an existing thread. Stock 0.171.0 preserves the full history,
resolves the provider-qualified alias to the second vendor's bare upstream
model id, and injects a developer `<model_switch>` message immediately before
the new user prompt. The switch request replays the earlier `Resume the
fixture.` exchange and the original coding turn.

## Commands

Offline (safe when stock Codex is absent; the integration cases stay ignored):

```sh
cargo test --test stock_codex_smoke
```

Real stock CLI, opt in explicitly:

```sh
CODEOX_TEST_CODEX_BIN=/home/$USER/.local/share/mise/installs/codex/0.171.0/bin/codex \
  cargo test --test stock_codex_smoke -- --ignored --nocapture
```

The four search cases need the relay's test-only Brave seam; without the
feature they are not compiled:

```sh
CODEOX_TEST_CODEX_BIN=/home/$USER/.local/share/mise/installs/codex/0.171.0/bin/codex \
  cargo test --features test-brave --test stock_codex_smoke \
  -- --ignored --nocapture --test-threads=1
```

One cross-wire case only:

```sh
CODEOX_TEST_CODEX_BIN=/home/$USER/.local/share/mise/installs/codex/0.171.0/bin/codex \
  cargo test --test stock_codex_smoke \
  stock_codex_anthropic_to_chat_completions_explicit_switch -- --ignored --nocapture
```

Invoking the ignored cases without `CODEOX_TEST_CODEX_BIN` fails with a message
that names the variable. Only loopback sockets are used, with all proxy
variables pointing at a closed loopback port.

## Pending gaps

* **Responses `apply_patch`.** The current relay catalog intentionally omits
  it for Responses vendors. If that changes, the Responses scenarios can move
  from `exec_command` editing to native `apply_patch`.
* **Final-message annotations.** `codex exec --json` does not expose the
  streaming `url_citation` annotations on `agent_message`. The fixtures assert
  the citations in the replayed tool output.
* **Anthropic sampling drop.** Stock Codex does not send `temperature` /
  `top_p` in these runs, so the `2026-10-21` case asserts the version header and
  the absence of those fields, while the actual drop behaviour is covered by
  the relay's own adapter tests.
