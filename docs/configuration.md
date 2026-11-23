# Install and configure ox

[Quickstart](../README.md) · [Compatibility](compatibility.md) · [Verification](verification.md)

## Install options

Download the **binary and its matching `.sha256` file** from the latest stable
release. Use `uname -sm` to choose the platform suffix. Use the filename without
`.tar.gz` for the commands below. [GitHub Releases](https://github.com/n0rpan/codex-rust-relay/releases/latest)

| Machine | Binary filename suffix | Requirements |
| --- | --- | --- |
| macOS Apple silicon (`Darwin arm64`) | `aarch64-apple-darwin` | macOS 11 or newer |
| macOS Intel (`Darwin x86_64`) | `x86_64-apple-darwin` | macOS 11 or newer |
| Linux x86-64 (`Linux x86_64`) | `x86_64-unknown-linux-gnu` | glibc 2.39 or newer |
| Linux ARM64 (`Linux aarch64`) | `aarch64-unknown-linux-gnu` | glibc 2.39 or newer |

**macOS.** In the directory containing both files, set `asset` to your exact
downloaded filename:

```sh
asset='ox-0.1.0-aarch64-apple-darwin'
shasum -a 256 -c "$asset.sha256" &&
  mkdir -p "$HOME/.local/bin" &&
  install -m 755 "$asset" "$HOME/.local/bin/ox"
```

**Linux.** Use the same steps with `sha256sum`:

```sh
asset='ox-0.1.0-x86_64-unknown-linux-gnu'
sha256sum -c "$asset.sha256" &&
  mkdir -p "$HOME/.local/bin" &&
  install -m 755 "$asset" "$HOME/.local/bin/ox"
```

If `ox` is missing on PATH, run `export PATH="$HOME/.local/bin:$PATH"` in Bash or
Zsh and add the same line to `~/.bashrc` or `~/.zshrc` for future terminals.
Run `ox --version`, then `ox setup`. Install the stock `codex` or `claude` CLI
separately if you want to launch it through ox.

### Update an existing installation

```sh
ox update --check
ox update
```

`ox update` verifies the binary against the SHA-256 digest published by GitHub,
then replaces the running executable's file. Use an install directory writable
by your own account. Restart any running `ox serve` or ox-launched sessions to
use the new binary. The updater leaves provider settings and credentials alone.

### Preview builds and source installation

Before the first stable release, or to try an unreleased commit, use a successful
CI run. Sign in to GitHub and download the `ox-<target>` artifact for your
platform, then unzip it and use the same checksum and install steps above.
Older CI artifacts contain only a `.tar.gz` and its checksum. Verify the archive,
extract it, and install the extracted `ox` file. [GitHub Actions builds](https://github.com/n0rpan/codex-rust-relay/actions/workflows/ci.yml)

To build on your own machine, check out this repository and run the following from its root.
CI uses Rust 1.100.0. Build from source if the Linux artifact needs a newer glibc or
if macOS Gatekeeper blocks the CI download. [Apple’s Gatekeeper guidance](https://support.apple.com/en-us/102445)

```sh
cargo install --locked --path .
ox --version
```

Cargo installs into `${CARGO_HOME:-$HOME/.cargo}/bin` by default. Add the install directory to
`PATH` if the shell cannot find `ox`. Install the stock `codex` or `claude` CLI separately.

## Add a provider

Run `ox setup` in a terminal, choose **Add an API key**, and pick the service that
issued it. For OpenCode Go, OpenRouter, Anthropic, OpenAI and Alibaba Cloud
Token Plan in Singapore, ox fills in the URL and API format. Paste your key at
the hidden prompt. If you already exported the provider’s variable, ox uses
that value. The menu returns so you can add more providers or enable Brave.

For **Other or self-hosted**, paste the API URL. A plain `/v1` URL defaults to
OpenAI Responses, and ox looks up `/v1/models` when you launch. Pasting a full
`/chat/completions`, `/messages`, or `/responses` URL selects its format and
removes the endpoint suffix before discovery. If the service cannot list models,
add a [static model entry](#model-names-and-discovery) with the exact model ID.

### Scripted or advanced setup

Choose the API format your **provider** expects, regardless of which CLI you use.
Use a model that supports streaming and tool calls for the coding workflows in the
[compatibility matrix](compatibility.md#verified-client-and-api-combinations).

| Provider API | `--wire` | ox adds this path to the base URL | Example base URL |
| --- | --- | --- | --- |
| OpenAI-compatible Responses | `responses` (default) | `/responses` | `https://openrouter.ai/api/v1` or `https://api.openai.com/v1` |
| Anthropic-compatible Messages | `anthropic_messages` | `/messages` | `https://api.anthropic.com/v1` |
| OpenAI-compatible Chat Completions | `chat_completions` | `/chat/completions` | `http://127.0.0.1:11434/v1` for a local OpenAI-compatible server |

Use the provider's base URL ending at the API prefix, commonly `/v1`. Export the key under
the variable name you pass to `--env-key`. For a loopback server that ignores bearer auth,
point `--env-key` to a variable holding a dummy value such as `local-only`. ox currently
requires a value to enable a route.

On the direct OpenAI endpoint, a key that passes `ox doctor --live` can still receive an
organization access error in Codex. If OpenAI says Codex API access is disabled for the
organization, ask the OpenAI API organization owner to check the setting for that key.
[Codex authentication](https://developers.openai.com/codex/auth)

The API credits benefit for eligible ChatGPT plans covers standard Responses API usage
in the linked API organization. It excludes Codex usage authenticated with API keys.
Use `ox login chatgpt` and `ox codex --chatgpt` to use your ChatGPT plan in Codex.
[OpenAI's API credit terms](https://developers.openai.com/api/docs/guides/chatgpt-api-credits#what-the-credits-cover)

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

`ox setup --help` lists all flags. `ox provider add` accepts the same prompts and flags
for adding more providers. Supplying a provider name and `--base-url` uses the flags
and defaults without prompting. Setup creates `codeox.toml` or appends a provider,
preserving existing settings. To change an existing entry, edit the file and relaunch.
For static models, setup prints their aliases and launch commands. `ox doctor --strict`
also treats warnings as errors.

### OpenCode Go and Alibaba Cloud

For OpenCode Go, load your Console API key as `OPENCODE_API_KEY`, then add a
model from your Go plan. This Chat Completions route works with both ox clients.
[OpenCode Go endpoints and session routing](https://opencode.ai/docs/console/harnesses/#base-urls)

```sh
ox provider add opencode-go \
  --base-url https://opencode.ai/inference/go/openai/v1 \
  --wire chat_completions --env-key OPENCODE_API_KEY \
  --no-discovery --model glm-5.3-flash
ox doctor --live --provider opencode-go
ox codex --model opencode-go/glm-5.3-flash
```

ox derives a stable OpenCode session identifier from the client session header
when one is available. Clients without a session header get a fresh identifier
per request.

Alibaba Cloud Model Studio uses region- and plan-specific URLs. Start from
the base URL in your working account or client configuration. Use Chat
Completions with an OpenAI-compatible URL. [Alibaba Cloud endpoints by region and plan](https://www.alibabacloud.com/help/en/model-studio/base-url)

For example, the Singapore **Token Plan** base URL worked with `qwen3.8-flash`
through both ox frontends. Load the key from that Token Plan as `ALIBABA_API_KEY`.

```sh
ox provider add alibaba \
  --base-url https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1 \
  --wire chat_completions --env-key ALIBABA_API_KEY \
  --no-discovery --model qwen3.8-flash
ox doctor --live --provider alibaba
ox claude --model alibaba/qwen3.8-flash
```

The international **Coding Plan** URL is a separate endpoint. It returned
HTTP 405 with a coding-agent restriction in a live relay check. If you see
this error, confirm the URL against the plan attached to your key.
[Alibaba Cloud Token Plan troubleshooting](https://www.alibabacloud.com/help/en/model-studio/token-plan-faq)

### Check the live connection

```sh
ox doctor --live --provider openrouter
```

This sends one small prompt to the first static model and checks for streamed text.
It can incur API charges. With discovery-only providers or to test another model,
add `--model <upstream-model-id>` using the provider's model ID. The probe uses the
provider's endpoint and credentials. Tool calling, Brave search and a Desktop
client session are separate checks. Plain `ox doctor` stays offline.

### Where settings and keys live

Both clients read `codeox.toml` under `$CODEX_HOME`, which defaults to `~/.codex`. For
Claude Code, that is still the provider registry location. Claude Code's own settings and
session storage use its normal directory. `ox doctor` prints the registry directory.

In `codeox.toml`, `env_key` is the **name** of an environment variable. A key
pasted at the hidden prompt lives in `$CODEX_HOME/ox-api-keys.json` as plaintext
with owner-only permissions (mode 0600). ox uses it only for the configured
destination and keeps it out of the launched client environment. Protect this
file like other credentials and keep it out of version control.

An exported variable takes priority over a saved key. To rotate a saved key,
choose its provider again in `ox setup`, paste the replacement, and restart ox.
If the variable is exported, unset it in that shell before choosing the provider
again. To rotate an environment-managed key, load the new value and restart ox.

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
It takes precedence over the live catalog. Pass an explicit alias on the first Codex
launch if Codex already has a different default model.

Run `ox codex` or `ox claude` with no client arguments from a terminal to choose
from the same list of configured and discovered aliases with usable keys. Filter
by model or provider, or pass `--model <alias>` to select one directly. An API key
configured in ox for Anthropic can power either client.

`ox claude` enables gateway model discovery for its Claude Code process. Open
`/model` to see additional Claude aliases from the configured providers. To opt out
for a launch, run:

```sh
CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=0 ox claude
```

If you launch `claude` directly against a shared `ox serve` daemon, set
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` in the client's environment.

ox lists configured aliases in `GET /v1/models`. Claude Code adds discovered aliases
whose IDs contain `claude` or `anthropic`. Managed picker settings can further limit
the list. For any configured alias, including a GPT or DeepSeek alias, launch with
`ox claude --model <provider/model-id>`. Use qualified aliases because the client
expands built-in names like `sonnet`. [Claude Code model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)

When multiple providers serve the same model, choose the provider-qualified alias
to control which provider receives the request. ox routes each alias to a single
upstream. Price-based routing and cross-provider failover are currently unavailable.

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

After adding an API provider, choose **Enable Brave web search** in `ox setup`,
then choose **Done**. Load `BRAVE_SEARCH_API_KEY` into your shell and restart ox.
The equivalent manual configuration is a top-level table in the same `codeox.toml`.
Remove the table or set `enabled = false` to stop using Brave.

```toml
[search]
env_key = "BRAVE_SEARCH_API_KEY"
max_results = 8
timeout_seconds = 30
```

Ask for a web search and source links. ox sends the query and applicable filters to Brave.
Citations returned to the model become part of that provider's conversation. The Brave key
stays in ox. LLM Context is used when the plan supports it. An entitlement error switches
that client to Brave Web Search for the rest of the launch and prints a notice to stderr.

- **Codex** uses the live standalone search tool. An explicit `disabled`, `cached`, or
  `indexed` policy in a launch flag, the selected profile, or the top level of
  `config.toml` takes precedence. `ox doctor` checks the configured default profile and
  top-level settings. For live search on one launch, use
  `ox codex -c 'web_search="live"' --model <alias>`. Without Brave, hosted search
  is vendor-dependent on Responses-only setups. With any translated vendor and no explicit
  search policy, ox disables hosted search for the launch.
- **Claude Code** uses its existing `WebSearch` tool and permission rules. Its separate
  `WebFetch` tool fetches URLs under Claude Code's network and permission settings. Without
  Brave, Anthropic can still run provider-side search when its endpoint supports it. For a
  translated route without Brave, add `"WebSearch"` to Claude Code's
  `permissions.deny` list to hide the unsupported tool:

```json
{
  "permissions": { "deny": ["WebSearch"] }
}
```

See [search limits](compatibility.md#search-and-fetch) for filters and supported operations.

## Claude Code Auto mode

The [CLI settings example](../examples/claude-code-auto.settings.json) retains Claude
Code's default classifier and asks before `WebFetch`. The commands below pass the
same settings inline, so they work from your own project directory.

Add `claude-sonnet-5` on the current Anthropic route or use the
[provider example](../examples/claude-providers.toml), then launch:

```sh
ox claude --model anthropic/claude-sonnet-5 --permission-mode auto \
  --settings '{"autoMode":{"environment":["$defaults"]},"permissions":{"ask":["WebFetch"]}}'
```

For a translated model, route Claude Code's classifier requests to a configured model that
supports review. This example uses the OpenRouter model from the quickstart and the same
Anthropic classifier route as above.

```sh
CLAUDE_CODE_AUTO_MODE_SERVER=0 CLAUDE_CODE_AUTO_MODE_MODEL=anthropic/claude-sonnet-5 \
  ox claude --model openrouter/openai/gpt-5.4-mini --permission-mode auto \
  --settings '{"autoMode":{"environment":["$defaults"]},"permissions":{"ask":["WebFetch"]}}'
```

The classifier receives action context through the Anthropic route. Check `/status` for
the effective permission mode and server review setting. Claude Code can fall back to
Manual mode if a model is ineligible. If an unfamiliar alias makes the client assume a
larger context window than the endpoint supports, set `CLAUDE_CODE_MAX_CONTEXT_TOKENS`
to the actual limit. [Claude Code permission modes](https://code.claude.com/docs/en/permission-modes#auto-mode)

Claude Desktop 3P users can also configure a stable [ox gateway](shared-gateway.md#claude-desktop-on-3p)
and adapt the [Desktop web-tools overlay](../examples/claude-desktop-web-tools.json).

## Sessions and sign-in

Use `ox setup` to add API providers, enable Brave or open either stock client's
sign-in. The menu returns after each action. Select **Done** when finished. Plain `ox codex`
and `ox claude` use the API registry when it is configured. Choose `:native` in the picker to use a matching subscription
sign-in alongside configured API models. Otherwise, they start
their own signed-in stock client. Bare `ox` prefers Codex and uses Claude Code
if only Claude is signed in. With no API provider, `ox doctor` asks the stock
clients for sign-in status and reports which subscription launches are ready.
`ox help subscriptions` lists the explicit native launch commands.

- **ChatGPT plan in Codex.** Run `ox login chatgpt` if you need to sign in.
  Codex manages its normal credentials. `ox login chatgpt status` checks whether
  a ChatGPT sign-in is present. Native Codex sign-in works without registering
  ox as a third-party integration. Use `ox codex --chatgpt` for that account or
  `ox codex` for API-backed sessions. In the project, use `ox codex resume` to pick
  an API-backed thread. The picker contains threads from both launchers, so select one
  you started through ox. Use a fresh thread and a reviewed summary to move between
  native ChatGPT and API-backed sessions. [Codex authentication](https://developers.openai.com/codex/auth)
- **Codex vendor switches** use the full alias in `/model` between completed turns.
  The new provider receives the portable conversation and tool history. ox keeps opaque
  provider continuation data scoped to its original route. Switch only when you intend
  to send the thread context to the new provider.
- **Claude subscription in Claude Code.** Run `ox login claude` to open Claude Code's
  sign-in, then `ox claude --native` to launch with its own settings. Use a Claude
  environment free of gateway and API overrides for subscription sessions.
  [Claude Code authentication](https://code.claude.com/docs/en/authentication)
- **Claude Code relay sessions** use its normal project history. From the same project use
  `ox claude --resume <session-id> --model <original-alias>`. Start a fresh session
  when changing model or vendor so signed thinking stays with its original route.

The shared gateway authenticates upstream with provider API keys. To spend
included API credits through it, claim them and configure an API key from the
linked OpenAI organization or Claude Console organization. Native subscription
sign-ins stay with Codex and Claude Code. Running an agent runtime on the server
would run the agent and its tools on that host, while the ox gateway serves
model calls for clients that keep their own tools and sessions.
[ChatGPT API credits](https://developers.openai.com/api/docs/guides/chatgpt-api-credits) ·
[Claude Platform credits](https://platform.claude.com/docs/en/about-claude/api-credits-for-subscribers)

OpenAI provides a separate OAuth flow for eligible ChatGPT plan requests from
open-source and locally hosted apps. ox has no integration with that flow today.
Anthropic permits users to sign in to hosted stock Claude Code for their own
sessions and restricts third-party services that route requests through those
subscription credentials, including services built with the Agent SDK.
[Sign in with ChatGPT for open-source apps](https://developers.openai.com/siwc/token-sharing-open-source) ·
[Claude credential rules](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use)

### Upgrade from the original single-provider relay

If there is no `codeox.toml`, `ox codex` follows the old Responses provider settings in
`config.toml`. `ox claude`, multi-vendor routing and Brave need `codeox.toml`. Use
`ox provider add` to create it. ox does not migrate old entries or rewrite the `model`
value in `config.toml`. For a new session, choose the qualified alias explicitly.
To resume a legacy session, keep its original mode and model slug available. A
`model_catalog_json` override prevents legacy startup.
