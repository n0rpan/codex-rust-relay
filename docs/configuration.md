# Install and configure ox

[Quickstart](../README.md) · [Compatibility](compatibility.md) · [Verification](verification.md)

## Install options

Pick the GitHub Actions artifact that matches `uname -sm`. The runner builds the archive,
checks its SHA-256, extracts it, and runs the copy inside. Sign in to GitHub to download
artifacts while they are retained (30 days). [Download instructions](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/download-workflow-artifacts)

| Machine | Artifact name | Target requirements |
| --- | --- | --- |
| Linux x86-64 | `ox-x86_64-unknown-linux-gnu` | glibc 2.39 or newer |
| Linux ARM64 | `ox-aarch64-unknown-linux-gnu` | glibc 2.39 or newer |
| macOS Apple silicon | `ox-aarch64-apple-darwin` | macOS 11 deployment target |
| macOS Intel | `ox-x86_64-apple-darwin` | macOS 11 deployment target |

To build on your own machine, check out this repository and run the following from its root.
CI uses Rust 1.100.0. Building from source is also an option if the Linux artifact requires a
newer glibc than you have.

```sh
cargo install --locked --path .
ox --help
```

Cargo installs into `${CARGO_HOME:-$HOME/.cargo}/bin` by default. Add the install directory to
`PATH` if the shell cannot find `ox`. Install the stock `codex` or `claude` CLI separately.

## Add a provider

Choose the API format your **provider** expects, regardless of which CLI you use.
Use a model that supports streaming and tool calls for the coding workflows in the
[compatibility matrix](compatibility.md#verified-client-and-api-combinations).

| Provider API | `--wire` | ox adds this path to the base URL | Example base URL |
| --- | --- | --- | --- |
| OpenAI-compatible Responses | `responses` (default) | `/responses` | `https://openrouter.ai/api/v1` or `https://api.openai.com/v1` |
| Anthropic-compatible Messages | `anthropic_messages` | `/messages` | `https://api.anthropic.com/v1` |
| OpenAI-compatible Chat Completions | `chat_completions` | `/chat/completions` | `http://127.0.0.1:11434/v1` for a local OpenAI-compatible server |

Use the provider's base URL ending at the API prefix, commonly `/v1`. Export the key under
the variable name you pass to `--env-key`. The direct OpenAI URL needs an eligible API account.
For a loopback server that ignores bearer auth, point `--env-key` to a variable holding a
dummy value such as `local-only`. ox currently requires a value to enable a route.

For example, add Anthropic Messages using a current API version verified with Anthropic:

```sh
# Load ANTHROPIC_API_KEY in this shell first.
ox provider add anthropic \
  --base-url https://api.anthropic.com/v1 \
  --wire anthropic_messages \
  --anthropic-version 2026-10-21 \
  --env-key ANTHROPIC_API_KEY \
  --no-discovery --model claude-haiku-5-5
ox doctor
ox claude --model anthropic/claude-haiku-5-5
```

Use `--anthropic-version 2023-06-01` for an endpoint using the legacy Anthropic API. You can
give it a separate provider name such as `anthropic-legacy` to keep both routes. See the
[example with both API versions](../examples/claude-providers.toml).

`ox provider add --help` lists all flags. Running `ox provider add` without arguments in
a terminal prompts for the fields. The command creates `codeox.toml` or appends a provider,
preserving existing settings. To change an existing entry, edit the file and relaunch.
`ox doctor --strict` also treats warnings as errors.

### Where settings and keys live

Both clients read `codeox.toml` under `$CODEX_HOME`, which defaults to `~/.codex`. For
Claude Code, that is still the provider registry location. Claude Code's own settings and
session storage use its normal directory. `ox doctor` prints the registry directory.

In `codeox.toml`, `env_key` is the **name** of an environment variable. ox reads its
value at launch and keeps configured provider and Brave keys out of the launched client
environment. Keep the values in your shell environment or load them with your secret manager.
After rotating a key, restart ox in a shell with the new value.

Use HTTPS for remote providers and HTTP only for loopback. URLs cannot contain embedded
credentials. `base_url` accepts neither a query string nor a fragment. `models_url` must
share the base URL's origin. `CODEOX_TOKEN` is reserved for the relay's local token.

### Model names and discovery

A model alias defaults to `<provider>/<upstream-model-id>`. For example, provider
`openrouter` with upstream model ID `openai/gpt-5.4-mini` uses
`openrouter/openai/gpt-5.4-mini` in either client.

By default, ox requests `<base_url>/models` once at launch. If that provider does not list
models in the expected shape, the picker may omit them. Set `discovery = false` and add
static entries to make launch independent of that endpoint. Static entries also override
catalog metadata when discovery is enabled.

```toml
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
wire_api = "responses"
env_key = "OPENROUTER_API_KEY"
discovery = false

[[providers.openrouter.models]]
id = "openai/gpt-5.4-mini"
# name = "GPT-5.4 mini"                  # optional display label
# alias = "openrouter/mini"              # optional stable alias
# context_window = 200000                # set to the provider model's actual limit
# input_modalities = ["text"]            # declare supported modalities
# reasoning_efforts = ["low", "medium"]  # declare supported effort values
```

Use lowercase ASCII letters, digits, `_`, and `-` for provider names, starting with a
letter or digit. Keep aliases unique. A provider-qualified alias cannot use another
configured provider's prefix. Keep your alias stable for resume. A known provider prefix
can route to an upstream model ID even when discovery omits it, but the model must exist
for that provider. A bare model ID is accepted only if routing can resolve it unambiguously.

If Codex shows a stale picker, check for `model_catalog_json` in `$CODEX_HOME/config.toml`.
It takes precedence over the live catalog. Claude Code offers a limited set of custom slots
by default. Set `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` to opt in to its gateway catalog,
or use `--model <alias>` directly. Use qualified names to avoid built-in Claude aliases like
`sonnet` being rewritten by the client. [Claude Code model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)

### Protocol-specific options

For Codex on an Anthropic route, `anthropic_thinking` defaults to `off`. For a model that
supports it, select `adaptive` or set `manual` with an `anthropic_thinking_budget_tokens` lower
than `max_output_tokens`. The default output limit for a translated Anthropic request is 8192.
`output_config_effort = true` enables explicit effort forwarding where the provider accepts it.
Both clients omit sampling parameters on Anthropic API `2026-10-21` and later. Codex's
Anthropic adapter also omits them when thinking is enabled. [Anthropic API migration guide](https://platform.claude.com/docs/en/api/migration-2026-10-21)

For Chat Completions, try `max_tokens_field = "max_completion_tokens"` if the provider
expects that field. The default is `max_tokens`. `chat_reasoning_effort = "forward"`
opts in to forwarding effort. `developer_role` defaults to `system` and can be set to
`developer`. Set `include_stream_usage = false` if the provider rejects that stream option.
Check the provider documentation before changing these settings.

## Web search with Brave

Add this top-level table in the same `codeox.toml` and load `BRAVE_SEARCH_API_KEY` into
your shell. Restart ox. Remove the table or set `enabled = false` to stop using Brave.

```toml
[search]
env_key = "BRAVE_SEARCH_API_KEY"
max_results = 8
timeout_seconds = 30
```

Ask for a web search and source links. ox sends the query and applicable filters to Brave.
Citations returned to the model become part of that provider's conversation. The Brave key
stays in ox. LLM Context is used when the plan supports it. An entitlement error switches
that client to Brave Web Search for the rest of the launch.

- **Codex** uses the live standalone search tool. An explicit `disabled`, `cached`, or
  `indexed` policy in a launch flag, the selected profile, or the top level of
  `config.toml` takes precedence. For live search on one launch, use
  `ox codex -c 'web_search="live"'`. Without Brave, hosted search
  is vendor-dependent on Responses-only setups. With any translated vendor and no explicit
  search policy, ox disables hosted search for the launch.
- **Claude Code** uses its existing `WebSearch` tool and permission rules. Its separate
  `WebFetch` tool fetches URLs under Claude Code's network and permission settings. Without
  Brave, Anthropic can still run provider-side search when its endpoint supports it. For a
  translated route without Brave, add `"WebSearch"` to Claude Code's
  `permissions.deny` list to hide the unsupported tool.

See [search limits](compatibility.md#search-and-fetch) for filters and supported operations.

## Claude Code Auto mode

The [CLI settings example](../examples/claude-code-auto.settings.json) retains Claude
Code's default classifier and asks before `WebFetch`. Use the example only where it fits
your existing Claude Code settings. Paths below are relative to the checkout root.

Add `claude-sonnet-5` on the current Anthropic route or use the
[provider example](../examples/claude-providers.toml), then launch:

```sh
ox claude --model anthropic/claude-sonnet-5 --permission-mode auto \
  --settings ./examples/claude-code-auto.settings.json
```

For a translated model, route Claude Code's classifier requests to a configured model that
supports review. This example uses the OpenRouter model from the quickstart and the same
Anthropic classifier route as above.

```sh
CLAUDE_CODE_AUTO_MODE_SERVER=0 CLAUDE_CODE_AUTO_MODE_MODEL=anthropic/claude-sonnet-5 \
  ox claude --model openrouter/openai/gpt-5.4-mini --permission-mode auto \
  --settings ./examples/claude-code-auto.settings.json
```

The classifier receives action context through the Anthropic route. Check `/status` for
the effective permission mode and server review setting. Claude Code can fall back to
Manual mode if a model is ineligible. If an unfamiliar alias makes the client assume a
larger context window than the endpoint supports, set `CLAUDE_CODE_MAX_CONTEXT_TOKENS`
to the actual limit. [Claude Code permission modes](https://code.claude.com/docs/en/permission-modes#auto-mode)

Claude Desktop 3P users can adapt the [Desktop web-tools overlay](../examples/claude-desktop-web-tools.json)
once they have a separate stable inference gateway. See [Desktop scope](compatibility.md#desktop-and-hosted-clients).

## Sessions and sign-in

- **Codex** uses your existing Codex home. Run `codex` for your usual ChatGPT sign-in
  and `ox codex` for API-backed sessions. In the project, use `ox codex resume` to pick
  an API-backed thread. Use a fresh thread and a reviewed summary to move between native
  ChatGPT and API-backed sessions.
- **Codex vendor switches** use the full alias in `/model` between completed turns.
  The new provider receives the portable conversation and tool history. ox keeps opaque
  provider continuation data scoped to its original route. Switch only when you intend
  to send the thread context to the new provider.
- **Claude Code** uses its normal project history. From the same project use
  `ox claude --resume <session-id> --model <original-alias>`. Start a fresh session
  when changing model or vendor so signed thinking stays with its original route.

### Upgrade from the original single-provider relay

If there is no `codeox.toml`, `ox codex` follows the old Responses provider settings in
`config.toml`. `ox claude`, multi-vendor routing and Brave need `codeox.toml`. Use
`ox provider add` to create it. ox does not migrate old entries or rewrite the `model`
value in `config.toml`. For a new session, choose the qualified alias explicitly.
To resume a legacy session, keep its original mode and model slug available. A
`model_catalog_json` override prevents legacy startup.
