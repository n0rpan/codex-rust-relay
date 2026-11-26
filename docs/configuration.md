# Install and configure ox

[Quickstart](../README.md) · [Compatibility](compatibility.md) · [Verification](verification.md)

## Install options

```sh
curl -fsSL https://raw.githubusercontent.com/n0rpan/ox/main/install.sh | sh
```

The installer works on macOS and Linux. It picks the build for your machine,
downloads it with its `.sha256` file, verifies the checksum, and installs
`~/.local/bin/ox` with an atomic rename. A running ox keeps its old file until
it exits, so the same command also upgrades. If the checksum does not match,
nothing is installed. When it finishes it prints the installed version, the
line to add to your shell profile if the directory is not on `PATH`, and the
next step, `ox setup`. Install the stock `codex` or `claude` CLI separately.

| Machine | Platform name (`<target>`) | Requirements |
| --- | --- | --- |
| macOS Apple silicon (`Darwin arm64`) | `aarch64-apple-darwin` | macOS 11 or newer |
| macOS Intel (`Darwin x86_64`) | `x86_64-apple-darwin` | macOS 11 or newer |
| Linux x86-64 (`Linux x86_64`) | `x86_64-unknown-linux-gnu` | glibc 2.39 or newer |
| Linux ARM64 (`Linux aarch64`) | `aarch64-unknown-linux-gnu` | glibc 2.39 or newer |

The installer needs `curl` or `wget`, `tar`, and `sha256sum` or `shasum`. On
Linux it checks the glibc version first. With an older glibc or musl it stops
and prints the [source build](#build-from-source) command.

| Installer option | Effect |
| --- | --- |
| `--stable` | Install the latest stable release. This is the default. |
| `--dev` | Install the dev pre-release. Pass it as `… \| sh -s -- --dev`. |
| `OX_CHANNEL=stable` or `OX_CHANNEL=dev` | Same as the flags. A flag wins over the variable. |
| `OX_INSTALL_DIR=/absolute/path` | Install directory. The default is `$HOME/.local/bin`. |
| `OX_INSTALL_BASE_URL=URL` | Download `ox-<target>.tar.gz` and its `.sha256` from this URL instead of GitHub, for a mirror or an offline copy. |
| `OX_INSTALL_SKIP_GLIBC_CHECK=1` | Skip the Linux glibc check. |

### Release channels

| Channel | Source | Version | Fixed download URL |
| --- | --- | --- | --- |
| stable | A pushed tag `vX.Y.Z` | `X.Y.Z` | `https://github.com/n0rpan/ox/releases/latest/download/ox-<target>.tar.gz` |
| dev | Every commit on `main` | `X.Y.(Z+1)-dev.N` | `https://github.com/n0rpan/ox/releases/download/dev/ox-<target>.tar.gz` |

**No stable release has been tagged yet.** Until one is, the dev pre-release is
the only download. Run without a channel, the installer says
`No stable ox release is published yet. Installing the dev build instead.` and
continues. With an explicit `--stable` it stops and asks you to run it again
with `--dev`.

The dev channel is a single GitHub pre-release tagged `dev`. CI replaces it
after every commit on `main` passes the stock-client checks and the four
platform builds, so the tag always points at the newest such commit. Its
version is the highest stable version with the patch number raised by one,
then `-dev.` and the CI run number. With no stable release that is
`0.0.1-dev.N`. A dev version always sorts below the stable release it leads
up to: `0.0.1-dev.2` < `0.0.1-dev.10` < `0.0.1`.

`ox --version` prints the version built into the binary. A release binary
reports the version CI resolved. A source build reports the version in
`Cargo.toml` and counts as a stable build.

### Update an existing installation

```sh
ox update --check
ox update
```

A build follows its own channel. A stable build looks at the latest stable
release, and a dev build looks at the dev pre-release.

| Command | Effect |
| --- | --- |
| `ox update --check` | Report whether a newer release exists. Installs nothing. |
| `ox update` | Install the newer release on this build's channel. |
| `ox update --dev` | Switch to the dev pre-release. |
| `ox update --stable` | Switch back to the latest stable release. |

`--check` combines with either channel flag. A channel switch installs the
other channel's current build even when its version number is lower, which is
how `ox update --stable` returns a dev build to stable. From a stable or source
build before the first stable release, `ox update` reports
`no stable ox release has been published yet` and points to `ox update --dev`.

`ox update` verifies the binary against the SHA-256 digest published by GitHub,
then replaces the running executable's file. Use an install directory writable
by your own account. Restart any running `ox serve` or ox-launched sessions to
use the new binary. The updater leaves provider settings and credentials alone.
Running the installer again has the same effect as `ox update`.

### Manual download

Every release on the [Releases page](https://github.com/n0rpan/ox/releases)
carries these files for each platform, each with a matching `.sha256` file:

| File | Contents |
| --- | --- |
| `ox-<target>.tar.gz` | The `ox` binary. The name is the same in every release, which gives the fixed URLs above. |
| `ox-<version>-<target>.tar.gz` | The same archive under a versioned name. |
| `ox-<version>-<target>` | The bare binary that `ox update` downloads. |

To install without the script, set `target` from the table above and `base`
to the channel you want:

```sh
target='x86_64-unknown-linux-gnu'
base='https://github.com/n0rpan/ox/releases/download/dev'
# For stable: base='https://github.com/n0rpan/ox/releases/latest/download'
curl -fsSLO "$base/ox-$target.tar.gz"
curl -fsSLO "$base/ox-$target.tar.gz.sha256"
sha256sum -c "ox-$target.tar.gz.sha256" &&
  tar -xzf "ox-$target.tar.gz" &&
  mkdir -p "$HOME/.local/bin" &&
  install -m 755 ox "$HOME/.local/bin/ox"
```

On macOS, use `shasum -a 256 -c` in place of `sha256sum -c`. If `ox` is
missing on PATH, run `export PATH="$HOME/.local/bin:$PATH"` in Bash or Zsh and
add the same line to `~/.bashrc` or `~/.zshrc` for future terminals. Run
`ox --version`, then `ox setup`.

Each CI run on a pull request also keeps its four `ox-<target>` builds as
workflow artifacts. Sign in to GitHub to download one from the run page.
[GitHub Actions builds](https://github.com/n0rpan/ox/actions/workflows/ci.yml)

### Build from source

Build from source if your Linux has an older glibc or uses musl, or if macOS
Gatekeeper blocks a downloaded binary. CI uses Rust 1.100.0.
[Apple’s Gatekeeper guidance](https://support.apple.com/en-us/102445)

```sh
cargo install --locked --git https://github.com/n0rpan/ox
ox --version
```

From a checkout, run `cargo install --locked --path .` in its root. Cargo
installs into `${CARGO_HOME:-$HOME/.cargo}/bin` by default. Add that directory
to `PATH` if the shell cannot find `ox`. Install the stock `codex` or `claude`
CLI separately.

## Add a provider

The first time you run `ox`, `ox codex` or `ox claude` in a terminal with no
provider and no saved sign-in, ox opens this setup for you. As soon as an API
provider has a usable key it prints `Setup is complete. Starting Codex.` (or
`Starting Claude Code.`) and opens the client in the same command. If that
client is not installed yet, ox prints its install command and the command to
run afterwards. Choosing **Done** leaves without launching. A launch with extra
arguments, or one that is not on a terminal, never opens setup. It reports that
no provider is configured.

Run `ox setup` in a terminal, choose **Add an API key**, and pick the service that
issued it. For OpenCode Go, OpenRouter, Anthropic and OpenAI, ox fills in the URL
and API format. For Alibaba Cloud, paste the endpoint shown in your Model Studio
account or the one already working in Oxa. Paste your key at the hidden prompt.
If you already exported the provider’s variable, ox uses that value. The menu
returns so you can add more providers or enable Brave.

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

Alibaba Cloud Model Studio uses region-, plan- and sometimes workspace-specific
URLs. Choose **Alibaba Cloud** in `ox setup` and paste the URL that matches
your API key. ox defaults to Chat Completions for an OpenAI-compatible base URL
and recognizes the Anthropic-compatible URL by its path. [Alibaba Cloud endpoints by region and plan](https://www.alibabacloud.com/help/en/model-studio/base-url)

For example, the Singapore **Token Plan** base URL worked with `qwen3.8-flash`
through both ox frontends. For scripted setup, load your OpenAI-compatible
base URL as `ALIBABA_BASE_URL` and its key as `ALIBABA_API_KEY`.

```sh
ox provider add alibaba \
  --base-url "$ALIBABA_BASE_URL" \
  --wire chat_completions --env-key ALIBABA_API_KEY \
  --no-discovery --model qwen3.8-flash
ox doctor --live --provider alibaba
ox claude --model alibaba/qwen3.8-flash
```

The international **Coding Plan** URL is a separate endpoint. It returned
HTTP 405 with a coding-agent restriction in a live relay check. If you see
this error, confirm the URL against the plan attached to your key.
[Alibaba Cloud Token Plan troubleshooting](https://www.alibabacloud.com/help/en/model-studio/token-plan-faq)

### Choose a Messages route for Claude Code

Choose a Messages endpoint to avoid translating Claude Code requests inside ox.
The provider determines which models, tools, and counting endpoints work on that route.
Keep the two protocols under separate provider names when you want both.

| Setup preset | Messages option |
| --- | --- |
| Anthropic | Already uses `https://api.anthropic.com/v1`. [Anthropic Messages](https://platform.claude.com/docs/en/api/messages) |
| OpenRouter | `https://openrouter.ai/api/v1`, with `wire_api = "anthropic_messages"`. ox uses bearer auth. [OpenRouter's Claude Code guide](https://openrouter.ai/docs/guides/coding-agents/claude-code-integration) |
| OpenCode Go | `https://opencode.ai/inference/go/anthropic/v1`, with `wire_api = "anthropic_messages"`. ox uses bearer auth and keeps its Go session routing. OpenCode may translate internally for some models. [Go endpoints](https://opencode.ai/docs/console/harnesses/#base-urls) |
| Alibaba Cloud | Paste your account's URL ending in `/apps/anthropic`. ox adds the API version path and discovers models through the OpenAI-compatible catalog on the same host. The model still has to be available on the Messages route. [Model Studio Messages](https://www.alibabacloud.com/help/en/model-studio/anthropic-api-messages) |
| OpenAI | Use Responses. ox translates the request and uses `/responses/input_tokens` for a provider-backed count. [OpenAI token counting](https://developers.openai.com/api/docs/guides/token-counting) |

For Alibaba, select **Alibaba Cloud** again and paste the second URL. When you
already use the Chat Completions route, setup adds `alibaba-messages` alongside
it. Paste your key for the Messages route. A previously configured Messages
route with `discovery = false` can use the companion catalog by turning discovery on.

For a custom provider, choose **Other or self-hosted**. A single endpoint works
with both clients through ox translation. To add another API format, answer yes
at **Add another API endpoint for this provider** and paste its full inference URL
ending in `/messages`, `/responses` or `/chat/completions`. Each route gets a model
alias prefix you can select in either client. If you add routes manually and
both use the same vendor key, give each route a distinct `env_key` reference.
This lets ox keep any saved key scoped to that route's base URL. For example,
with `OPENCODE_MESSAGES_API_KEY` available in your shell:

```sh
ox provider add opencode-go-messages \
  --base-url https://opencode.ai/inference/go/anthropic/v1 \
  --models-url https://opencode.ai/inference/go/v1/models \
  --wire anthropic_messages --env-key OPENCODE_MESSAGES_API_KEY
```

### Check the live connection

```sh
ox doctor --live --provider openrouter
```

This sends one small prompt to the first static model and checks for streamed text.
It can incur API charges. With discovery-only providers or to test another model,
add `--model <upstream-model-id>` using the provider's model ID. The probe uses the
provider's endpoint and credentials. Tool calling, Brave search and a Desktop
client session are separate checks. Plain `ox doctor` stays offline.

### Check and repair a setup

```sh
ox doctor
ox doctor --fix
```

`ox doctor` reads your setup offline and prints one line per fact, grouped
under `setup`, `clients`, `saved keys`, one `provider <name>` heading per
provider, and `config.toml`. Each line starts with a mark:

| Mark | Meaning |
| --- | --- |
| `✓` | Checked and fine |
| `·` | Information |
| `!` | Warning. `ox doctor --strict` exits non-zero for these too. |
| `✗` | Error. The launch would fail. |

The report ends with a `summary:` line and a `next:` line. `next:` is the one
command to run: the fix for the first error, or the launch command when
everything is fine. The exit status is 0 when healthy, 1 when problems were
found, and 2 for a usage error. Colour is used only on a terminal, and
`NO_COLOR` and `TERM=dumb` turn it off. Key values are never printed.

The `clients` group shows where `codex` and `claude` were found on `PATH`. A
missing client is information, with the install command for your platform,
because a host that only runs `ox serve` needs neither.

A plain `ox doctor` never changes a file. `ox doctor --fix` first repairs what
ox can safely repair, lists each repair under `repairs`, then runs the check:

| Situation | What `ox doctor --fix` does |
| --- | --- |
| `codeox.toml` is not valid TOML, or has no provider | Moves it to `codeox.toml.broken-<time>`. Saved keys are kept. Run `ox setup` to add your providers again. |
| `ox-api-keys.json` cannot be parsed | Moves it to `ox-api-keys.json.broken-<time>`. Paste your keys again in `ox setup`. |
| `ox-favorites.json` cannot be parsed | Moves it to `ox-favorites.json.broken-<time>`. Star your models again in the picker. |
| Temporary files left by an interrupted save | Removes them once they are at least 10 minutes old. |

`<time>` is the Unix time of the repair. Unreadable files are renamed, never
deleted, so every repair can be undone by renaming the file back. Running
`--fix` again prints `nothing needed repair`. A provider entry that parses but
is wrong, such as an invalid `wire_api` or URL, is reported and left alone, and
so is a `codeox.toml` that is a symbolic link.

Two situations are handled at launch, without the doctor:

- **A provider is configured but no key is usable.** A plain `ox codex` or
  `ox claude` on a terminal, with no saved sign-in for that client, says
  `<Provider> has no API key yet.` and asks for the key at the hidden prompt.
  It saves the key and continues the launch. Press Enter to skip.
- **The saved-key file became readable by other users.** See
  [where settings and keys live](#where-settings-and-keys-live).

Errors from ox share one shape: the problem on the first line, then optional
`why:` and `file:` lines, then a `fix:` line with a command you can copy.

```text
ox: could not find `codex` on PATH
  why: ox starts the stock Codex CLI and does not bundle it
  fix: npm install -g @openai/codex
```

On macOS the `fix:` line is `brew install --cask codex`. A mistyped ox command,
help topic, `--provider` name or `--model` provider prefix gets the corrected
command on its `fix:` line. Stock Codex subcommands such as `ox exec` and
`ox resume`, options, and prompts of more than one word still go to Codex
unchanged. A single word that is close to an ox command is treated as a typo.
To send it to Codex, run `ox codex <word>`.

### Where settings and keys live

Both clients read `codeox.toml` under `$CODEX_HOME`, which defaults to `~/.codex`. For
Claude Code, that is still the provider registry location. Claude Code's own settings and
session storage use its normal directory. `ox doctor` prints the registry directory.

In `codeox.toml`, `env_key` is the **name** of an environment variable. A key
pasted at the hidden prompt lives in `$CODEX_HOME/ox-api-keys.json` as plaintext
with owner-only permissions (mode 0600). ox uses it only for the configured
destination and keeps it out of the launched client environment. Protect this
file like other credentials and keep it out of version control.
If the file's permissions are loosened, for example by a restored backup or
synced dotfiles, ox sets them back to 0600 the next time it reads the file and
prints one line saying so. A key file owned by another user is refused.

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

`--model` is checked before the client starts. A provider prefix that matches
no configured provider stops the launch, with the closest provider name on the
error's `fix:` line. A model ID that is missing from a known provider's list
only prints a note with the closest match, and the launch continues as
requested. A value without a `/` is passed on unchecked.

If Codex shows a stale picker, check for `model_catalog_json` in
`$CODEX_HOME/ox-clients/codex/config.toml`. It takes precedence over the live
catalog and may have been copied from your native settings on first launch.
Pass an explicit alias if the relay profile already has a different default model.

Run `ox codex` or `ox claude` with no client arguments from a terminal to choose
from the same list of configured and discovered aliases with usable keys.
Type a model or provider name to filter immediately. Use the arrow keys to move,
**Tab** to toggle a star and move to the next match, and **Enter** to launch.
Use **Alt+↑/↓** on a star to reorder it, **Ctrl+F** to show only stars, and
**Ctrl+U** to clear the search. **Esc** closes the picker and keeps your changes.

Stars are saved in order by model alias in `$CODEX_HOME/ox-favorites.json`.
In API-backed launches, Codex sorts its `/model` catalog with stars first.
Claude Code shows the ordered lineup from ox in `/model` with the Default and
current model entries supplied by Claude Code. Managed settings or a
`modelPicker` explicitly passed to `ox claude --settings` can supply a different
lineup. The ox launch picker still shows the full available catalog by default.
Pass `--model <alias>` to launch directly.
[Claude Code model picker settings](https://code.claude.com/docs/en/settings-reference#modelpicker)

With no starred lineup, `ox claude` enables gateway model discovery for its
Claude Code process. Open `/model` to see eligible Claude aliases from the
configured providers. To opt out of discovery for a launch, run:

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

#### Context limits

ox reads advertised context limits from model discovery and uses a static
`context_window` entry when you provide one. Codex receives the limit in its
model catalog. For a custom model ID in `ox claude`, ox gives Claude Code the
starting model’s limit through `CLAUDE_CODE_MAX_CONTEXT_TOKENS`. A value you set
in your shell or Claude settings can override that. If discovery omits the limit,
set `context_window` on the model entry. For Alibaba Cloud's
`deepseek-v4.1-flash-1017`, ox uses the published 1,000,000-token limit when
the catalog omits it.
[Alibaba Cloud model announcement](https://www.alibabacloud.com/notice/detail?_p_lc=1&id=2225)

Claude Code reads the limit for the whole process. Relaunch through ox when
changing to a custom model with a different limit. Claude Code can keep its
built-in limit for IDs it recognizes as Claude models. Clients that connect
directly to `ox serve` can read known limits as `max_input_tokens` from
`/v1/models`. Claude Code's gateway discovery currently reads the names and
descriptions from that list, so direct connections need their own context setting.
[Claude Code context settings](https://code.claude.com/docs/en/model-config#correct-the-window-for-a-gateway-or-custom-model-id)

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

Add Anthropic in `ox setup`, then run `ox doctor` to check that your key is
available. Start a session on a model your Anthropic account can use:

```sh
ox claude --model anthropic/claude-sonnet-5 --permission-mode auto
```

On eligible native Anthropic routes, ox forwards the beta header, `safeguards`,
streamed `safeguard_results`, and tool-use IDs. Check the **Auto mode server** row
in Claude Code's `/status` to see whether server review is active.
[Claude Code classifier billing](https://code.claude.com/docs/en/auto-mode-classifier-billing#make-the-session-eligible)

When a session starts outside the direct Anthropic API, ox defaults
`CLAUDE_CODE_AUTO_MODE_SERVER` to `0`. Claude Code handles the review through
separate classifier requests, which can count toward API usage. To use a configured
Anthropic model for those requests, add `--client-classifier auto`:

```sh
ox claude --model openrouter/openai/gpt-5.4-mini --permission-mode auto \
  --client-classifier auto
```

ox looks for Sonnet 5, then Sonnet 4.6, in the catalog of configured direct
Anthropic routes with credentials. If neither is available, Claude Code chooses
its own classifier. You can also select a specific Anthropic Messages alias, such
as `--client-classifier anthropic/claude-sonnet-4-6`. That route receives the
action context needed for review. Model selection for classifier requests
requires Claude Code 2.1.302 or later. Claude Code can deny checked actions if
the classifier is unavailable. The server setting applies for the launch.
Start a new session on an eligible direct Anthropic route to use its server checks.
If another endpoint supports Anthropic server checks, set
`CLAUDE_CODE_AUTO_MODE_SERVER` to `1` for that launch.
[Claude Code classifier model selection](https://code.claude.com/docs/en/permission-modes#choose-the-classifier-model)

The [CLI settings example](../examples/claude-code-auto.settings.json) keeps
Claude Code's default classifier and asks before `WebFetch`.

### Fine-grained tool streaming

For a native Anthropic endpoint that supports fine-grained tool streaming, opt in
when starting Claude Code through ox:

```sh
CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING=1 \
  ox claude --model anthropic/claude-sonnet-5
```

Claude Code leaves this off by default on custom gateways. Enabling it lets a
compatible provider send tool arguments as they are generated. It applies to
tool arguments and leaves answer text generation and Auto mode review timing
to their respective upstream requests. To time provider inference on its own,
run `ox doctor --live --provider anthropic --model claude-sonnet-5`. It reports
time to the first stream event and first visible text.
[Claude Code feature pass-through](https://code.claude.com/docs/en/llm-gateway-protocol#feature-pass-through)

For Desktop, configure a stable [ox gateway](shared-gateway.md#claude-desktop-on-3p)
and adapt the [Desktop web-tools overlay](../examples/claude-desktop-web-tools.json).

## Sessions and sign-in

Use `ox setup` to add API providers, enable Brave or open either stock client's
sign-in. The menu returns after each action. Select **Done** when finished. Plain `ox codex`
and `ox claude` show that client's subscription at the top of the picker
when signed in, alongside the API models. Choosing the subscription opens a native session
with its own `/model` list. With no API providers, ox can start the signed-in
stock client directly. Bare `ox` defaults to Codex. Choose Claude Code in
setup or run `ox claude` to open a Claude session. With no API provider, `ox doctor` checks for
saved accounts without launching either client. Each client validates its own
credentials when you open a subscription session. If Codex uses its keyring,
ox reports the configured store and leaves the account check to Codex.
`ox help subscriptions` lists the sign-in commands.

| Launch | Where the client saves its settings and history |
| --- | --- |
| `codex`, `claude`, or a subscription selected in ox | Your usual client profile |
| `ox codex` with an API model | `$CODEX_HOME/ox-clients/codex` |
| `ox claude` with an API model | `$CODEX_HOME/ox-clients/claude` |

`$CODEX_HOME` defaults to `~/.codex`. The first API-backed launch seeds the
new profile with user settings. Login files and old sessions stay with your
usual client. Change API-session defaults, including `/model` preferences and
user-level MCP registrations, in the relay profile. ox keeps Codex’s SQLite
state and logs in that profile by default. Project and managed settings still apply.
Signing in or changing preferences after choosing a **subscription** in ox uses
the same profile as the plain command.

If an older ox launch left its alias selected as the native default, open plain
`codex` or `claude`, use `/model` to choose a model from that client's subscription,
and start a new session.

- **ChatGPT plan in Codex.** Run `ox login chatgpt` if you need to sign in.
  Codex manages its normal credentials. `ox login chatgpt status` reports a
  saved sign-in or configured keyring. Native Codex sign-in works without registering
  ox as a third-party integration. Use `ox codex --chatgpt` for that account or
  `ox codex` for API-backed sessions. In the project, use `ox codex resume` to pick
  a thread from the relay profile. Use a fresh thread and a reviewed summary to move between
  native ChatGPT and API-backed sessions. [Codex authentication](https://developers.openai.com/codex/auth)
- **Codex vendor switches** use the full alias in `/model` between completed turns.
  The new provider receives the portable conversation and tool history. ox keeps opaque
  provider continuation data scoped to its original route. Switch only when you intend
  to send the thread context to the new provider.
- **Claude subscription in Claude Code.** Run `ox login claude` to open Claude Code's
  sign-in, then `ox claude --native` to launch with its own settings. Use a Claude
  environment free of gateway and API overrides for subscription sessions.
  [Claude Code authentication](https://code.claude.com/docs/en/authentication)
- **Claude Code relay sessions** use the relay profile's project history. From the same project use
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

### Subscription main session with API workers

1. Add an API provider in `ox setup` and check it with `ox doctor`.
2. Run `ox codex` or `ox claude` and choose the subscription row. You can
   also run `ox codex --chatgpt` or `ox claude --native`.
3. Ask **“List my API models and subscription routes.”** ox lists API models
   with their provider and your stars first. It includes the saved ChatGPT and
   Claude subscription routes it detects. Use the full alias in a request such as **“Start a
   Codex goal worker on `opencode-go/glm-5.3` to fix the failing tests in this
   project. Let it edit files. Give me the job ID.”** Substitute your chosen alias.

The `ox_workers` MCP server is available only for that launch. It uses an authenticated loopback
connection and needs no `ox serve` process or saved MCP entry. Plain `codex`
and `claude` keep their existing MCP setup. If the tools are missing, check
`/mcp` in the subscription session and check any managed MCP policy.
Approve the tool call when your client prompts.

| Ask for | What starts |
| --- | --- |
| Codex session on an API model | Stock Codex in the project with the selected ox alias |
| Codex goal on an API model | A stock Codex app-server thread with Goal mode |
| Claude Code session or workflow on an API model | Stock Claude Code in the project with the selected ox alias |
| Claude Code session or workflow on your subscription | Stock Claude Code on the host's saved Claude account. Ask for a subscription worker and omit the API alias. |

`start_ox_worker` returns a job ID. Ask for status or the result with
`ox_worker_status`, or ask to stop it with `cancel_ox_worker`. Jobs run between
tool calls. Workers have no time or count limit by default; set `OX_WORKER_MAX_MINUTES` or
`OX_WORKER_MAX_PER_PARENT` (positive; unset or 0 means no limit) to add one. Without a running OX session host
they stop when the parent ox launch exits. When the session host is running
(OX Desktop or `ox host install`), it owns the worker as an ordinary session:
the worker keeps running after you close the terminal, its approvals reach
your desktop and paired phone, and a resumed parent can still read its status.
The status includes the stock client's session ID when available.
Work already written to the project stays there if you cancel.
To keep a finished worker's status and final answer available after its
process has been reaped or the host restarts, the session host keeps a small
copy of that final answer (at most 24 KiB, never the transcript) in its
ledger under the host state directory. The full history stays in the stock
client's own session files.

`ox claude` gives each new session an ID before the first turn by passing
`--session-id` to Claude Code, unless you chose a session yourself with
`--resume`, `--continue`, `--session-id`, `--fork-session` or `--from-pr`,
turned off saving with `--no-session-persistence`, or ran a subcommand such
as `mcp` or `update`. After you close the terminal, `ox claude --resume <that
id>` lets the main agent read and steer the workers it started. The ID is the
one Claude Code shows for the session. `ox codex resume <id>` does the same
for Codex. Only the launch that started a worker, or a later resume of its
session, controls it; another `ox` in the same project does not. Otherwise
use OX Desktop or a paired phone.

A host-owned worker's approvals and questions wait for a person in OX Desktop
or on a paired phone, never for the main agent. `ox_worker_status` then
reports `waiting_for_operator` with where to answer. To keep terminal workers
inside the launch, with approvals denied as before, add this to `codeox.toml`:

```toml
[workers]
host = "never"   # default "auto": use the session host when one is running
```

When you switch a session to YOLO in OX Desktop or on the phone, you can
also allow YOLO for its workers and handoffs. That grant covers the session's
descendants until you revoke it; running children keep their mode. To turn
the grant on by default for every session of a project, list the project:

```toml
[workers]
yolo_grant_projects = ["/home/me/src/app"]   # absolute project directories
```

There is no global switch. Only list projects whose agents you would let run
without permission prompts. Their YOLO workers and handoffs then start without
asking, and the session ledger records the project grant as what allowed them.

With the session host running, workers start in the main session's own
permission mode, mapped to the worker's runtime (Codex Read only ↔ Claude Plan,
Workspace ↔ Accept edits, Auto-review ↔ Auto, YOLO ↔ bypass permissions).
The main agent can lower it with `start_ox_worker`'s `permission_mode`
(`read`, `write`, `classifier` or `yolo`) and can never raise it. A worker
that would start in YOLO or bypass waits until you accept it in OX Desktop or
on a paired phone, unless you granted YOLO for that session's workers and
handoffs. Without a host, workers start with read access, and write access
when you tell the main agent a worker may edit files. Claude read workers use
Plan mode with the host and `dontAsk` with only read tools in a standalone
launch. Classifier and YOLO require the session host.

Write workers can edit files and run shell commands. Codex uses its Workspace
sandbox. For Claude Code, OX pre-approves Bash and enables Claude Code’s own
sandbox. The sandbox can restrict filesystem and network access. It is built
in on macOS and needs `bwrap` and `socat` on Linux. If those dependencies are
missing, OX shows a warning and Claude Code can run Bash unsandboxed with the
worker’s OS permissions. Codex read workers use the Read only sandbox.

Give concurrent writers separate working directories by starting their parent
sessions there. The main session and its built-in subagents use your subscription.
The worker bills the API provider you picked or the Claude subscription you
requested.

For a second opinion on a piece of text, ask for `delegate_to_ox_model`.
That tool makes one model request with a 90-second timeout. For development,
tests, and other multi-step tasks, ask for a background worker.

In the client, `/model` shows the subscription's account-specific model choices.
The worker catalog reports the detected subscription routes alongside the API
models and identifies Claude as a supported subscription worker.
[Codex subagent settings](https://learn.chatgpt.com/docs/agent-configuration/subagents) ·
[Claude Code subagent model selection](https://code.claude.com/docs/en/sub-agents#choose-a-model)

Goal and Workflow modes need support from the installed client and selected
model. If a mode fails, check the job status. You can also run it directly in
`ox codex --model <alias>` or `ox claude --model <alias>`.
[Codex Goal mode](https://learn.chatgpt.com/docs/long-running-work) ·
[Claude Code workflows](https://code.claude.com/docs/en/workflows)

### Continue a session in another runtime, route or model

In OX Desktop, type `/handoff codex:<alias> <request>` or `/handoff claude[:<alias>]
<request>` in a session, or choose **Continue in…** in the command palette. On
the phone, use **Continue in…** in the session menu. The new native session
starts with a budgeted selection of the real earlier messages, not a summary,
then your request. It links back to the source, and its agent can read the
rest of the source with `read_ox_thread`. It starts in the source's permission
mode unless you choose another with `--mode` or in the sheet. A Claude session
cannot move between billing routes, so switching from the Claude subscription
to an API alias is also a handoff. The source must be idle with no pending
requests. Before a conversation goes to a different provider, OX says which one.

### Upgrade from the original single-provider relay

If there is no `codeox.toml`, `ox codex` follows the old Responses provider settings in
`config.toml`. `ox claude`, multi-vendor routing and Brave need `codeox.toml`. Use
`ox provider add` to create it. ox does not migrate old entries or rewrite the `model`
value in `config.toml`. For a new session, choose the qualified alias explicitly.
To resume a legacy session, keep its original mode and model slug available. A
`model_catalog_json` override prevents legacy startup.
