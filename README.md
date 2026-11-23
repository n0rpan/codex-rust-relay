# ox

Run `ox codex` or `ox claude` from your project. Both commands use the same API
provider setup and model picker. When you’re signed in, the picker also lists that
client’s native subscription. An Anthropic API key lets either client use Claude
models through the gateway.

Without an API provider configured, plain `ox codex` and `ox claude` use the
corresponding client's existing subscription sign-in. Bare `ox` prefers Codex
and opens Claude Code when only Claude is signed in. Run `ox setup` to add an API
provider or sign in, and `ox doctor` to check what's configured.

Use `ox help desktop` for Codex and Claude Desktop, `ox help share` to connect
other devices, or `ox help t3` for T3 Code.
For a ChatGPT or Claude subscription, run `ox help subscriptions`.

## 1. Install

Install or update the CLI you want to use and make sure `codex --version` or
`claude --version` works in your terminal. ox uses the binary on your `PATH` at
launch. CI runs the [compatibility tests](docs/verification.md#client-updates)
against the latest official clients. [Install Codex](https://developers.openai.com/codex/cli/) ·
[Install Claude Code](https://code.claude.com/docs/en/quickstart)

On **macOS 11+** or **Linux with glibc 2.39+**, download the binary and checksum
for your machine, verify them, and install to `~/.local/bin/ox`.
[macOS and Linux install steps](docs/configuration.md#install-options)

Once installed, run:

```sh
ox --version
ox setup
```

Use `ox update --check` to check for a stable release and `ox update` to install it.
For unreleased builds, follow the [preview or source install steps](docs/configuration.md#preview-builds-and-source-installation).

## 2. Add a provider

1. Run `ox setup`, choose **Add an API key**, then pick the service that issued it.
2. Paste that service’s API key at the hidden prompt. ox fills in the connection
   settings and saves the key locally. For **Other or self-hosted**, paste the
   service’s API URL first.
3. Back in the menu, add another provider or enable Brave web search. Choose
   **Done**, then run `ox doctor`.

Use **Sign in with ChatGPT** or **Sign in with Claude** for a subscription
session in its own client. If your plan offers API credits, claim them and
add an API key from the linked organization to use those credits in both
clients and over the shared gateway.
[OpenAI API credits](https://developers.openai.com/api/docs/guides/chatgpt-api-credits) ·
[Claude Platform credits](https://platform.claude.com/docs/en/about-claude/api-credits-for-subscribers)

For repeatable setup with **OpenRouter**, the commands below use a model tested with
both clients. Load `OPENROUTER_API_KEY` from your secret manager or use this hidden
Bash or Zsh prompt for the current terminal:

```sh
printf 'OpenRouter API key (input hidden): '
IFS= read -rs OPENROUTER_API_KEY
printf '\n'
export OPENROUTER_API_KEY
```

```sh
ox setup openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --wire responses \
  --env-key OPENROUTER_API_KEY \
  --no-discovery --model openai/gpt-5.4-mini

ox doctor
```

Look for **0 error(s)**. `doctor` shows where to find `codeox.toml` on its `home:` line,
lists static model aliases, and checks credential references offline. If you already
have a provider configured, use its existing alias. [Other providers and setup options](docs/configuration.md#add-a-provider)

## 3. Try a session

From your project directory, run `ox codex` or `ox claude` and choose from all
available ox API aliases. Search updates as you type. Use **↑/↓** to move,
**Tab** to star or unstar, and **Enter** to launch the highlighted model.
You can star several models before starting a session. They stay at the
top in both launchers in the order you starred them. Use **Alt+↑/↓** to reorder,
**Ctrl+F** to see just your stars, and **Esc** to close the picker.

If one API model is available and you have no native subscription, ox selects
it for you. To skip the picker:

```sh
ox codex --model openrouter/openai/gpt-5.4-mini
ox claude --model openrouter/openai/gpt-5.4-mini
```

Ask **“Read this project’s README and tell me how to run its tests. Leave the files as
they are.”** A reply grounded in the file confirms both the model connection and a local
read. Prompts and tool results go to the selected API provider and use that provider’s billing.

Use the same alias, **`provider/model-id`**, in either client. Add more providers with
`ox provider add`, then relaunch. In API-backed sessions, **`/model`** in Codex
lists your stars first. In Claude Code it shows your starred lineup when you
have one. Use the ox launch picker to browse the full catalog.
[Catalog and client behavior](docs/configuration.md#model-names-and-discovery)

API-backed launches keep their model defaults and new sessions in separate ox
profiles. Plain `codex` and `claude` continue using your usual settings and sign-in.
Use `ox codex resume` or `ox claude --resume <session-id>` for a relay session.
Choosing a subscription in ox opens your usual client profile, so its sign-in
and settings changes are shared with the plain command.
[Profiles and session recovery](docs/configuration.md#sessions-and-sign-in)

**Use both in one session.** Pick your subscription in `ox codex` or
`ox claude`, then ask **“List my API models and subscription routes.”**
Pick an alias and ask **“Start a Codex goal worker on that model to
fix the failing tests. Let it edit this project and tell me the job ID.”**
The main conversation stays on your subscription, and ox runs the worker in
the background using the selected API provider.
[Delegate to an API worker](docs/configuration.md#subscription-main-session-with-api-workers)

## Common tasks

| I want to… | Where to go |
| --- | --- |
| Connect Anthropic, OpenAI, a Chat Completions vendor, or a local server | [Provider setup](docs/configuration.md#add-a-provider) |
| Add Brave citations or configure Claude Code Auto mode | [Search and Claude settings](docs/configuration.md#web-search-with-brave) |
| Resume a session, switch models, or keep my usual ChatGPT sign-in | [Sessions and sign-in](docs/configuration.md#sessions-and-sign-in) |
| Keep my subscription and delegate to an API model | [API workers](docs/configuration.md#subscription-main-session-with-api-workers) |
| Share one ox gateway on a tailnet or connect desktop clients and T3 Code | [Home-server setup](docs/shared-gateway.md) |
| See supported clients, API versions, and feature boundaries | [Compatibility](docs/compatibility.md) |
| Reproduce checks or measure the relay | [Verification and performance](docs/verification.md) |

## If setup stalls

| What you see | What to try |
| --- | --- |
| `ox: command not found` | Run `./ox --help` in the extracted folder. Move the binary to a directory on your `PATH`. |
| Linux reports a missing glibc version, or macOS blocks the downloaded binary | Use the [source installation](docs/configuration.md#install-options) and run `ox --version` again. |
| Client missing, or Codex’s bundled model catalog fails to load | Run `codex --version` or `claude --version` from the same terminal. For Codex also try `codex debug models --bundled`. If the problem began after an update, check the latest-client CI job. |
| `provider … is unavailable` | Run `ox doctor`. Add the missing key in `ox setup` or export the variable it names. |
| `unknown model`, or an empty picker | Pass the full alias from setup. Check the vendor model ID, or pin it with a [static model entry](docs/configuration.md#model-names-and-discovery). |
| Plain `codex` or `claude` opens on an ox alias saved by an older build | Use `/model` in the plain client to choose a subscription model, then start a fresh session. [Profiles and session recovery](docs/configuration.md#sessions-and-sign-in) |
| Authentication error after a clean `doctor` | Run `ox doctor --live --provider <name>` to check the key, model and streaming with one small API request. |
| Provider returns `404` or rejects the request format | Check that the [base URL and wire format](docs/configuration.md#add-a-provider) match the endpoint. Leave the request path, such as `/chat/completions`, off the base URL. |
| Provider returns `429` | Check that provider’s quota and rate limits, or choose another configured model. |
| A direct OpenAI key returns `codex_byok_restricted_by_api_credit_promo` | That account’s API-credit plan rejected the Codex request in live testing. Use an eligible provider credential, or launch plain `codex` for your usual ChatGPT session. |
| Brave configured but search unavailable | Run `ox doctor` to check that the key variable is set and see any Codex policy conflict. For one live Codex launch, use `ox codex -c 'web_search="live"' --model <alias>`. Brave checks the key when a search runs. [Search setup](docs/configuration.md#web-search-with-brave) |
