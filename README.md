# ox

A local Rust gateway that connects stock coding clients to multiple model providers.

`ox codex` and `ox claude` start the selected client with a local authenticated relay. Both
commands read the same provider registry from `$CODEX_HOME/codeox.toml`. Model aliases identify
the vendor, which supplies its own base URL and credential reference.

The local interface speaks Responses for Codex and Messages for Claude Code. The shared gateway
can route to OpenAI Responses, Anthropic Messages, and OpenAI Chat Completions vendors.

Client-specific launch settings and local request handling are separate from provider discovery,
alias routing, and upstream credentials. A new client can bring its own launch settings and
protocol adapter while reusing the registry and Brave search.

## Setup

Install with Cargo and put the clients you use, `codex` or `claude`, on your `PATH`.

```sh
cargo install --locked --path .
```

Run setup in the same `$CODEX_HOME` the relay uses. Set the credential in the named environment
variable before running `doctor` or launching the relay.

```sh
ox provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --env-key OPENROUTER_API_KEY
ox doctor
ox codex -C /path/to/project
```

`provider add` writes one `[providers.<key>]` table and never accepts or prints a credential value;
`doctor` is offline and reports each provider's wire, credential reference and launch conflicts.
Both setup commands return before a client starts. You can try the setup in a scratch home.

```sh
scratch_codex_home=$(mktemp -d)
CODEX_HOME="$scratch_codex_home" ox provider add acme \
  --base-url https://api.example.com/v1 --no-discovery --model acme-coder \
  --env-key ACME_API_KEY
ACME_API_KEY=fixture-only CODEX_HOME="$scratch_codex_home" ox doctor
```

Run `codex` directly for native ChatGPT sessions. `ox codex` uses the same home and session
history with launch-scoped provider settings. Run `claude` directly for native Claude sessions.
To use the gateway from Claude Code, start it in your project directory.

```sh
ox claude --model openrouter/vendor-model
```

`ox provider add` updates only the shared `codeox.toml` provider configuration.

Claude Code keeps its own session history and approvals. Run `ox claude --resume <session-id>`
to resume an API-backed Claude Code session. Use `--model <alias>` to select any configured
model. Claude Code's built-in picker has limited slots for custom model IDs.
Set `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` to opt in to Claude Code listing the
shared catalog from the relay's `/v1/models`.
See [Claude Code model configuration](https://code.claude.com/docs/en/model-config).

Start a new Claude Code session when changing vendor or model. A resumed native Anthropic
session can contain signed thinking scoped to the previous model and API route.

For Claude Code, ox uses a private temporary settings file to keep inference pointed at
loopback and preserve unrelated client settings. The file is removed on normal exit.
An organization policy that controls routing or credentials can prevent the gateway launch.

## Two modes

* **Multi-vendor mode** — `$CODEX_HOME/codeox.toml` exists. Every provider in that file is relayed,
  and models are addressed by provider-qualified aliases.
* **Legacy Codex mode** — no `codeox.toml`. The original single-provider behaviour is preserved: the
  provider named by `model_provider` in `config.toml` is relayed, and its upstream model IDs are used
  unchanged as Codex model slugs.

Create `$CODEX_HOME/codeox.toml` to switch modes; delete it to go back to legacy behaviour. An
existing `codeox.toml` with no usable providers is a startup error rather than a silent fallback.

## `codeox.toml`

```toml
[providers.openrouter]
name = "OpenRouter"                                  # optional display name
base_url = "https://openrouter.ai/api/v1"            # required
env_key = "OPENROUTER_API_KEY"                       # optional: env var holding the bearer key
# wire_api = "responses"                             # responses | anthropic_messages | chat_completions
# models_url = "https://openrouter.ai/api/v1/models" # optional; must share base_url's origin
# discovery = true                                   # optional; false = static models only

[providers.deepinfra]
name = "DeepInfra"
base_url = "https://api.deepinfra.com/v1/openai"
env_key = "DEEPINFRA_API_KEY"

[[providers.deepinfra.models]]
id = "meta-llama/Meta-Llama-3.1-70B-Instruct"
name = "Llama 3.1 70B Instruct"
context_window = 131072
input_modalities = ["text"]                          # "text" and "image" are supported
reasoning_efforts = ["low", "medium", "high"]
# alias = "deepinfra/llama-3.1-70b"                  # optional stable alias override
```

Provider names must start with a lowercase ASCII letter or digit and contain only lowercase
letters, digits, `_` or `-`. URLs must be HTTPS, or HTTP on a
loopback host; they cannot embed credentials, and `base_url` cannot carry a query string or fragment.
`models_url` must have the same origin as `base_url`, so a vendor key is never sent cross-origin.
`env_key` cannot be `CODEOX_TOKEN`, which carries the relay's one-time loopback token.

### Aliases, model routing and resume

Each model is exposed to Codex as a stable alias:

* default: `<provider>/<upstream-model-id>`, e.g. `openrouter/anthropic/claude-sonnet-4`
* or the explicit `alias` from a `[[providers.<name>.models]]` entry

Requests are routed by alias, and the `model` field sent upstream is rewritten to the vendor's own
model ID. Two vendors can expose the same upstream model ID without colliding — that is exactly what
the provider prefix is for. If two entries ever produce the same final alias, startup fails with an
error naming both providers instead of guessing.
An explicit alias may use its own provider's prefix. Prefixes that name another configured
provider are reserved for that provider's routes.

Aliases are what Codex persists in sessions, so they are deterministic and survive restarts as long
as the provider key, the upstream model ID (or explicit alias), and the launch provider ID stay the
same. Requests for `<known-provider>/<anything>` are routed by prefix even when model discovery did
not list that model, so a resumed session keeps working while a vendor's `/models` endpoint is
unreachable. A bare upstream ID also works when exactly one vendor claims it; if several do, the
relay returns an error listing the qualified aliases.

The relay does not rewrite `model` in `config.toml`. Choose an alias with `/model` in Codex, or pass
`ox codex --model <alias>`.

### Discovery, static models and capability overrides

With `discovery = true` (the default), ox asks each vendor for `GET <models_url>` at launch
(20 s timeout, 8 MiB response bound) and turns the result into Codex catalog entries. Vendors fail
independently: one vendor's error is logged (as an HTTP status, never a response body) and the rest of
the catalog is still served.

`[[providers.<name>.models]]` entries serve two purposes:

* **Static models** — required when discovery is off or unavailable. They keep aliases in the catalog
  (and therefore resumable sessions) even while a vendor cannot list its models.
* **Capability overrides** — an entry whose `id` matches a discovered model overrides `name`,
  `context_window`, `input_modalities`, `reasoning_efforts` and `alias`.

Models that are not returned by discovery or listed statically do not appear in the `/model` picker,
but they still route by provider prefix as described above.

### Credentials

Each vendor's key is read once at launch from the environment variable named by its `env_key`. Keys
are:

* used only for that vendor's origin;
* kept out of every Codex child environment (every configured `env_key` is removed before both the
  bundled-catalog probe and the real `codex` launch);
* never written to Codex config, logs, or error messages.

A vendor without a key stays in the catalog when it has static models, but its requests fail with a
safe `502` naming the missing variable instead of borrowing another vendor's credentials.

## Translated vendors (Anthropic Messages, Chat Completions)

`wire_api = "anthropic_messages"` translates to `POST <base_url>/messages`; `wire_api =
"chat_completions"` translates to `POST <base_url>/chat/completions`. Both endpoints are derived
from the configured `base_url` (put `/v1` in it if the vendor needs it) and stay on that origin.

```toml
[providers.claude]
name = "Claude"
base_url = "https://api.anthropic.com/v1"
wire_api = "anthropic_messages"
env_key = "ANTHROPIC_API_KEY"
discovery = false

# Optional Anthropic options
# anthropic_version = "2023-06-01"     # default; also tested with "2026-10-21"
# anthropic_thinking = "off"           # off (default) | manual | adaptive
# anthropic_thinking_budget_tokens = 4096
# output_config_effort = false         # opt in to output_config.effort
# max_output_tokens = 8192             # relay-supplied max_tokens

[[providers.claude.models]]
id = "claude-sonnet-4-5"
name = "Claude Sonnet 4.5"
context_window = 200000
reasoning_efforts = ["low", "medium", "high"]

[providers.ollama]
base_url = "http://127.0.0.1:11434/v1"
wire_api = "chat_completions"
discovery = false

# Optional Chat Completions options
# max_tokens_field = "max_tokens"      # or "max_completion_tokens"
# chat_reasoning_effort = "drop"       # or "forward"
# developer_role = "system"            # or "developer"
# include_stream_usage = true

[[providers.ollama.models]]
id = "qwen3-coder"
```

Authentication is wire-specific: Anthropic vendors get `x-api-key` plus `anthropic-version`; the
Responses and Chat Completions wires get `Authorization: Bearer`. Codex's own `Authorization`
header never reaches an upstream.

### Anthropic API versions and thinking

The version header defaults to `2023-06-01` and is configurable per vendor. `2026-10-21` (and later
dates) removes the sampling parameters, so `temperature`, `top_p` and `top_k` are dropped before the
request is sent; `2023-06-01` forwards them.

`output_config.effort` is opt-in (`output_config_effort = true`) because the 2023 API version does
not define it. Alternatively:

* `anthropic_thinking = "manual"` sends `thinking: {type: "enabled", budget_tokens: N}` and clears
  sampling parameters; `N` must be smaller than `max_output_tokens`.
* `anthropic_thinking = "adaptive"` sends `thinking: {type: "adaptive"}` and clears sampling
  parameters.

With thinking enabled, signed `thinking`/`redacted_thinking` blocks are carried to Codex inside the
reasoning item's opaque `encrypted_content` envelope and replayed verbatim while a tool call is
outstanding. Do not strip or reorder reasoning items mid-turn. With the default `off` setting no
signed blocks exist and that replay path is unused.

### Tool surface

Codex 0.171 emits four kinds of tools. Translated vendors see a predictable, reversible mapping:

| Codex tool | Sent to the translated vendor | Translated back |
| --- | --- | --- |
| `function` | unchanged | unchanged |
| `namespace` (`multi_agent_v1`, `web`) | flattened `<namespace>__<name>` functions | `function_call` with `namespace` + bare `name` |
| `custom` freeform (`apply_patch`) | function with one required string `input` | `custom_tool_call` with `input` |
| hosted `web_search` | dropped (the launch sets `web_search="disabled"` unless live standalone search is configured) | n/a |

Without live search, when any configured vendor uses a translated wire, ox launches Codex with
`-c web_search="disabled"`, because hosted search runs provider-side and cannot be executed by a
translated vendor. With live search enabled, Codex's `web.run` namespace is flattened to `web__run`
for the vendor and translated back to `namespace: "web"`, `name: "run"`, so the relay executes the
search itself without a second service. Ordinary client tools, approvals and sandbox settings are
untouched. Tool calls in replayed history (`custom_tool_call`, `custom_tool_call_output`, namespaced
`function_call`) are normalized the same way. `additional_tools` items from code-mode models are
rejected with an error, never silently dropped.

## Live web search (opt-in)

By default web search is untouched: Responses vendors keep Codex's hosted search, and translated
vendors keep it disabled. To let Codex 0.171.0's standalone `web.run` tool run through ox,
opt in with a top-level `[search]` table and a Brave Search subscription token in the environment:

```toml
[search]
env_key = "BRAVE_SEARCH_API_KEY"   # default; Oxa-compatible
# max_results = 8                   # 1..=20 results requested per query
# timeout_seconds = 30              # 1..=120 seconds, whole request
```

```sh
export BRAVE_SEARCH_API_KEY=...     # never stored in codeox.toml
ox codex
```

`[search]` is the opt-in. It enables three launch-scoped overrides only when the credential
reference resolves to a usable key:

* `model_providers.codeox.supports_standalone_web_search=true` (the provider flag Codex 0.171.0
  publishes for its standalone extension);
* `features.standalone_web_search=true`;
* `web_search="live"` when, and only when, the user has not pinned a policy.

The token is read once, used only for `https://api.search.brave.com`, and stripped from both Codex
child invocations. The relay mounts `POST /v1/alpha/search` behind the same loopback bearer token,
buffers at most 1 MiB for it, and forwards only Codex's explicit `commands.search_query` and
`settings` to Brave. Codex's `input` conversation tail is never read or forwarded.

Policy is never widened silently:

* an explicit `web_search = "disabled"`, `"cached"` or `"indexed"` in `config.toml`, the selected
  profile, or a forwarded `-c web_search=...` keeps the standalone flags off and is left untouched
  (`ox` prints a warning when `[search]` is also configured);
* only live policy is served. The bridge rejects a request whose
  `settings.external_web_access` is false, `"cached"` or `"indexed"` with `403`;
* if `BRAVE_SEARCH_API_KEY` is missing or invalid, nothing is advertised and no other provider's
  credential is borrowed.

Limits and fail-closed behaviour:

* one request may contain at most four `search_query` entries, eight domains per filter group, and
  1 MiB of request body;
* rendered output is bounded to 12 KiB per query (48 KiB per request), matching the direct client;
* only exact Brave recency buckets (1, 7, 30, 365 days) are forwarded; any other value is rejected;
* only `search_query` runs. `image_query`, `open`, `click`, `find`, `screenshot`, `finance`,
  `weather`, `sports`, `time`, literature search and unknown operations return an explicit tool
  error instead of being ignored;
* `allowed_domains` and per-query `domains` become `site:` groups, and returned URLs must match; a
  `blocked_domains` filter fails closed;
* if the Brave plan lacks LLM Context, the same pooled client falls back to Brave web search once
  and says so on stderr.

### Claude Code web search

With the same `[search]` table, `ox claude` answers stock Claude Code 2.1.330's dedicated
`WebSearch` follow-up from Brave. Claude Code's tool permissions control whether that follow-up
runs. The relay sends Brave the search query and any domain or country filters, then returns
bounded excerpts and citation links. The relay handles this request directly without an MCP
server or a summarization model.

If you run a translated provider without Brave, use the Claude Code permission rule
`"deny": ["WebSearch"]` to remove the tool. Native Anthropic routes can use the vendor's
own search when ox has no Brave configuration.

### Claude Code provider and permission profiles

Merge [claude-providers.toml](examples/claude-providers.toml) into `$CODEX_HOME/codeox.toml` and
set `ANTHROPIC_API_KEY` and `BRAVE_SEARCH_API_KEY` in the launch environment. Run `ox doctor`
to check the configuration. Both Anthropic entries in the example intentionally use the same key.
Select the API version by model alias:

```sh
ox claude --model anthropic/claude-sonnet-5
ox claude --model anthropic-legacy/claude-haiku-5-5
ox claude --model anthropic/claude-sonnet-5 --permission-mode auto \
  --settings ./examples/claude-code-auto.settings.json
```

The [Auto mode profile](examples/claude-code-auto.settings.json) keeps Claude Code's default
classifier rules and requires approval for its `WebFetch` tool. Claude Code executes fetches using
its own network and permission settings. The relay handles the separate `WebSearch` call.

Native Anthropic routes preserve the Auto mode beta header, request safeguards, and streamed
verdicts. Availability depends on the model and account. For a translated route, use Claude
Code's classifier with a configured model that supports it:

```sh
CLAUDE_CODE_AUTO_MODE_SERVER=0 CLAUDE_CODE_AUTO_MODE_MODEL=anthropic/claude-sonnet-5 \
  ox claude --model openrouter/openai/gpt-5.4-mini --permission-mode auto \
  --settings ./examples/claude-code-auto.settings.json
```

This example assumes you have added the OpenRouter provider and model. Claude Code sends
classifier context to the configured Anthropic route. Use `/status` to check server review
and the effective permission mode. See the [Claude Code permission modes guide](https://code.claude.com/docs/en/permission-modes#auto-mode).

Claude Code may choose Manual mode for models that it does not consider eligible for Auto
mode. On a model alias that Claude Code cannot recognize, set
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` to your upstream's context limit if the client assumes a
larger window. See the [Claude Code gateway compatibility guide](https://code.claude.com/docs/en/llm-gateway-protocol).

### Claude Desktop web tools

[claude-desktop-web-tools.json](examples/claude-desktop-web-tools.json) is a web-tools overlay for
a Claude Desktop 3P installation with its own inference connection. It uses Desktop's bundled
Brave connector. Replace the example fetch host and point `headersHelper` at an executable that
supplies the Brave key as an `X-Subscription-Token` header in a JSON object. The example allows
search in Chat and Cowork. Code sessions apply Claude Code's tool permission rules. Web Fetch asks
for approval, and the host allowlist controls Chat and Cowork fetches and shell egress where the
Code sandbox is active. See the [Desktop web tools
guide](https://claude.com/docs/third-party/claude-desktop/web-tools).

Desktop 3P needs a stable inference endpoint and credential. The `ox claude` relay uses a
launch-scoped loopback address and credential tied to its CLI child. Desktop inference and its
provider-side Web Search protocol are outside the verified ox client matrix. See the [Desktop
gateway contract](https://claude.com/docs/third-party/claude-desktop/gateway).

## Moving from native ChatGPT to API-backed sessions

Switch providers across the native and API-backed modes by starting a new thread with a summary
you have reviewed.

1. Ask the current thread for a handoff covering the goal, changed files, checks already run and
   next step. Review it and remove credentials or any details you want to keep with the current
   provider.
2. Start a new API-backed thread with `ox codex -C /path/to/project` and select an alias in `/model`.
   For the reverse handoff, start a new thread with `codex`.
3. Paste the reviewed summary as the first message.

Both modes store thread history in the shared Codex home. Use `resume` with the same launcher
that created the thread. An explicit thread ID can select history from that shared home.

### Streaming and errors

Translated SSE is framed incrementally with a per-event size bound. A premature end, idle timeout
or protocol error produces a terminal `response.failed`. The Responses path forwards SSE bytes
directly and surfaces an upstream read failure as a downstream transport error. Dropping the
client connection cancels the upstream request on both paths.

Translated upstream failures are reduced to the HTTP status plus a redacted `error.message` (never
the raw body or headers), so a vendor key echoed by an upstream cannot leak back to Codex. Inbound
request limits still apply before translation.

### Known limitations

* Adapter slices reject hosted/server tools, image and file inputs, structured outputs,
  `item_reference`, `previous_response_id`, `conversation`, `store: true`, `background` and
  non-streaming requests.
* Anthropic `parallel_tool_calls: false` has no equivalent; Claude may still emit several
  `tool_use` blocks, which arrive as several `function_call` items.
* `pause_turn` has no Responses equivalent and becomes `response.failed` with code `pause_turn`.
* Chat Completions drops `reasoning.effort` unless `chat_reasoning_effort = "forward"`, and drops
  reasoning items because they cannot be represented statelessly.
* Remote compaction is not available for translated vendors: Codex 0.171 reports
  `remote_compaction_v2` as removed, and `POST /v1/responses/compact` returns `400` so local
  compaction is used.
* Model discovery still uses the Responses-shaped `/models` parser. Set `discovery = false` and list
  models statically for vendors whose catalogue does not match that shape.
* Live web search is available only in multi-vendor mode (`codeox.toml`); legacy mode has no
  `[search]` table. It serves Brave text results only: `image_query`, page `open`/`click`/`find`,
  screenshots and the other standalone operations return explicit tool errors rather than being
  emulated. `ox doctor` currently reports providers.

## Security and limits

* The relay listens on an ephemeral `127.0.0.1` port only and requires the random 256-bit bearer
  token it passes to Codex (`CODEOX_TOKEN`). Token comparison is constant-time.
* Upstream redirects are never followed (`Policy::none()`), so a vendor cannot bounce a request — or
  its credentials — to another origin.
* Hop-by-hop and inbound `Authorization`/`Host` headers are stripped before forwarding; the vendor's
  key is attached only to that vendor's requests. Re-encoded request bodies get fresh framing headers.
* Request bodies for routing are bounded at 32 MiB. Requests over the limit return `413`.
  Malformed bodies, unknown aliases and ambiguous bare IDs return `400` before contacting upstream.
* Translated streams accept up to 1 MiB per SSE event and 32 MiB of upstream data per response.
  The 300-second inference read timeout measures the gap between reads, so active streams can
  continue beyond five minutes.
* `POST /v1/alpha/search` is authenticated by the same loopback bearer token, is bounded at 1 MiB,
  and is only mounted when `[search]` and a usable `BRAVE_SEARCH_API_KEY` exist. Its outbound
  queries go only to Brave's production origin; non-production origins are accepted only by the
  test harness.
* Successful Responses streams pass through unchanged. Responses HTTP failures retain their
  status, a redacted message and safe error identifiers. Translated HTTP failures are reduced
  to a status and redacted message. Both paths preserve numeric `Retry-After` on `429`.

## Backwards compatibility and omissions

Legacy mode is intentionally the previous relay behaviour, not a superset of the new one:

* Legacy mode has no `codeox.toml` features: no provider-qualified aliases, no static model entries,
  no per-vendor capability overrides, and no independent discovery across vendors.
* Legacy model slugs stay unprefixed upstream IDs. Multi-vendor aliases are prefixed. Sessions resumed
  across a mode switch need a matching slug.
* There is no automatic migration from a `config.toml` provider table into `codeox.toml`, and no
  mapping of the existing top-level `model` value to an alias.
* `model_catalog_json` in `config.toml` still makes Codex ignore discovery. Legacy mode refuses to
  start with it set; multi-vendor mode warns and continues, since the relay cannot unset that key for the
  child.
* Discovery runs once per launch; there is no background refresh. Add static entries if you need
  aliases pinned across vendor outages.
* Environment-variable stripping is exact and case-sensitive, matching the platform environment
  model. Vendor keys referenced only through `env_key` are not forwarded, but keys inherited by other
  means (for example inside a wrapper script) are outside the relay's control.
* Request bodies that are routed are parsed and re-serialized, so JSON object key order can change
  (semantics do not).

## Wire compatibility

`wire_api` accepts exactly `"responses"`, `"anthropic_messages"` and `"chat_completions"`. Aliases
such as `"anthropic"` or `"chat"` are rejected so nothing silently degrades to the wrong protocol.

Stock Codex CLI 0.171.0 completed the same search, read, edit and test task with loopback vendor
and Brave fixtures. The search results included citation URLs and survived restarting the relay
and resuming the session. Separate fixtures switched vendors within a session.

| Upstream wire | Codex 0.171.0 | Claude Code 2.1.330 |
| --- | --- | --- |
| OpenAI Responses | Coding, citations, resume, switch | Coding, citations, resume |
| OpenAI Chat Completions | Coding, citations, resume, switch | Coding, citations, resume |
| Anthropic Messages `2023-06-01` | Coding, citations, resume, switch | Coding, citations, resume |
| Anthropic Messages `2026-10-21` | Coding, citations, resume, switch | Coding, citations, resume |

All four cross-protocol switch fixtures preserved the visible conversation and tool history.
The same-vendor fixtures retained opaque continuation state, including Claude's signed thinking.
The cross-vendor fixtures excluded that state from the new vendor's request.

```sh
CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 \
  cargo test --locked --offline --features test-brave --test stock_codex_smoke -- \
    --ignored --test-threads=1

OX_TEST_CLAUDE_BIN=/path/to/claude-2.1.330 \
  cargo test --locked --offline --features test-brave --test stock_claude_smoke -- \
    --ignored --test-threads=1
```

The standalone search contract was verified with stock Codex 0.171.0 and loopback mocks.
With `supports_standalone_web_search=true`, `features.standalone_web_search=true` and
live policy, stock Codex emitted the `web.run` namespace, called `POST /v1/alpha/search`, returned
the search output and its `turn0searchN` citation ids as a `function_call_output`, and completed the
turn. The same test asserts that the provider flag arrives through the published launch override and
that the mocked Brave request contains only the query. Run it explicitly:

```sh
CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 \
  cargo test --locked --offline --bin ox stock_codex_emits_web_run -- --ignored --nocapture
```

Translated vendors replay `web.run` as `web__run` and back (tested with mocked Chat Completions and
Anthropic Messages vendors); native Responses citations stream through unchanged (tested with a
mocked SSE stream).

The fixtures use loopback vendors and dummy credentials. A separate loopback test verifies
that stock Codex uses its ChatGPT credentials for native inference and the vendor credential
through ox from the same Codex home.

On the Responses and Chat Completions wires, the Claude fixtures also use stock `ToolSearch`
to discover the `WebSearch` schema before calling it.

### Live checks

Using real API credentials with disposable projects, stock Codex completed the coding task
through OpenRouter Responses (`openai/gpt-5.4-mini`), Alibaba Chat Completions
(`deepseek-v4.1-flash-1017`), and Anthropic Messages (`claude-haiku-5-5`) on both API versions.
The OpenRouter run also verified relay restart, resume, and Brave citations. Claude Code
returned live Brave citations on the legacy Anthropic route and completed read and edit
checks on OpenRouter and Alibaba. The client reported Auto mode and completed the read and edit
check on current Anthropic with `claude-sonnet-5`, and on OpenRouter with Claude Code's
classifier configured to use that Anthropic route. A transport fixture verifies that native
Anthropic SSE fields, including `safeguard_results`, survive the relay unchanged.

The direct OpenAI account returned `codex_byok_restricted_by_api_credit_promo`. Native ChatGPT
credential isolation was verified with loopback fixtures.

### CI binaries

The CI workflow uploads a release archive and SHA-256 checksum for each of Linux x86-64 and
ARM64 (glibc 2.39) and macOS Intel and Apple silicon (macOS 11 deployment target). Each native
runner builds, packages, and runs the archived `ox` binary. Download your platform's artifact
from the workflow run.

## Release benchmark

On Linux, build the release binary and benchmark a fixed copy.

```sh
cargo build --release --locked --offline
ox_release_copy=$(mktemp -d)
cp target/release/ox "$ox_release_copy/ox"
CODEOX_BENCH_BIN="$ox_release_copy/ox" \
  cargo test --release --locked --offline --test relay_benchmark -- --ignored --nocapture
OX_BENCH_CLIENT=claude CODEOX_BENCH_BIN="$ox_release_copy/ox" \
  cargo test --release --locked --offline --test relay_benchmark -- --ignored --nocapture
```

The harness records the client, host, binary digest, fixture sizes and results for each protocol. It
measures 60 seconds of warmed idle RSS and CPU, 40 paired direct and relayed first-event times
against the same loopback mock, and peak RSS with eight concurrent streams. Process metrics cover
only the relay. The latency test excludes Codex, remote networks and model generation.

### Measured release

On 2026-11-23, the installed ox 0.1.0 release passed all six client and protocol combinations on
a Qualcomm Oryon host with 12 logical CPUs running Linux 7.3.0-77-omarchy (aarch64). The executable had
SHA-256 `941b1e72ac55a470dd942b1b11d523004a41343e470be2e326af549b29226d4a`.

| Client | Upstream wire | Added p95 first event | Warm idle RSS | Peak RSS, eight streams |
| --- | --- | ---: | ---: | ---: |
| Codex | Responses | 0.608 ms | 5.301 MiB | 5.598 MiB |
| Codex | Chat Completions | 0.899 ms | 7.324 MiB | 7.324 MiB |
| Codex | Anthropic Messages | 0.746 ms | 5.434 MiB | 7.527 MiB |
| Claude Code | Responses | 1.119 ms | 5.461 MiB | 6.043 MiB |
| Claude Code | Chat Completions | 1.267 ms | 7.406 MiB | 7.406 MiB |
| Claude Code | Anthropic Messages | 0.726 ms | 5.355 MiB | 5.680 MiB |

Idle CPU recorded zero ticks in each 60-second window. Codex and Claude Code request fixtures
were 2,248 and 2,469 bytes. The direct Chat Completions and Anthropic Messages request fixtures
were 1,974 and 1,692 bytes. The Responses, Chat Completions and Anthropic Messages SSE fixtures
were 3,184, 1,695 and 1,433 bytes. The mock held eight simultaneous streams for 1,500 ms.
[Benchmark fixtures](tests/fixtures/benchmark)

## Troubleshooting

* `could not read the bundled Codex model catalog` — `codex` is missing from `PATH`.
* `provider "x" is unavailable; set X_KEY ...` — the vendor's `env_key` is not set in the
  environment ox was started from.
* `unknown model ...` — the alias is not in the catalog and does not start with a configured
  provider key; run `/model`.
* `model ... is provided by multiple providers` — use a provider-qualified alias.
* `[search] is configured but BRAVE_SEARCH_API_KEY is not set` — export the variable named by
  `search.env_key`; web search stays off rather than falling back to another key.
* `search is configured, but Codex's web_search policy is pinned to "cached"` — set
  `web_search = "live"` to enable live search.
