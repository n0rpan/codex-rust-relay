# ox

Use Codex or Claude Code with your own model API keys. One provider setup works with both
CLIs. Run `ox codex` or `ox claude` to start a local relay that exits with the client. Run
`codex` or `claude` directly for your usual sign-in.

**Already installed?** Run `ox setup` to choose a subscription or add an API provider, or `ox --help` to
see the commands. Typing just `ox` in a terminal opens setup on first use.

Use `ox help desktop` for Codex and Claude Desktop, `ox help share` to connect
other devices, or `ox help t3` for T3 Code.
For a ChatGPT or Claude subscription, run `ox help subscriptions`.

## 1. Install

Install or update the CLI you want to use and make sure `codex --version` or
`claude --version` works in your terminal. ox uses the binary on your `PATH` at
launch. CI runs the [compatibility tests](docs/verification.md#client-updates)
against the latest official clients. [Install Codex](https://developers.openai.com/codex/cli/) ·
[Install Claude Code](https://code.claude.com/docs/en/quickstart)

Sign in to GitHub and open a successful [CI run](https://github.com/n0rpan/codex-rust-relay/actions/workflows/ci.yml)
with **Release binaries** jobs on the branch you want to install. Published snapshots
are retained for 30 days. The binaries target Linux with **glibc 2.39 or newer** (such as
Ubuntu 24.04) and **macOS 11 or newer**. Download your platform’s artifact from **Artifacts**.
Unzip it, then run these commands from the folder containing the `.tar.gz` and its checksum:

```sh
# Run this in the folder for a single platform artifact.
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c ox-*.tar.gz.sha256 && tar -xzf ox-*.tar.gz
else
  shasum -a 256 -c ox-*.tar.gz.sha256 && tar -xzf ox-*.tar.gz
fi && ./ox --version
```

Install it for your user account and make it available in this shell:

```sh
mkdir -p "$HOME/.local/bin"
install -m 755 ox "$HOME/.local/bin/ox"
export PATH="$HOME/.local/bin:$PATH"
```

If a new terminal cannot find `ox`, add that directory to your shell's startup
`PATH`. See [platform requirements and source installation](docs/configuration.md#install-options).

## 2. Add a provider

Run `ox setup`, choose **API key provider**, and follow the prompts. Enter `openrouter`, `anthropic`, or `openai`
to get connection defaults. When asked for an environment variable, enter its name,
such as `OPENROUTER_API_KEY`. Load the API key into your shell, then run `ox doctor`.

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

From your project directory, run `ox codex` or `ox claude`, then open `/model`
to choose a model. To select an alias directly:

```sh
ox codex --model openrouter/openai/gpt-5.4-mini
ox claude --model openrouter/openai/gpt-5.4-mini
```

Ask **“Read this project’s README and tell me how to run its tests. Leave the files as
they are.”** A reply grounded in the file confirms both the model connection and a local
read. Prompts and tool results go to the selected API provider and use that provider’s billing.

Use the same alias, **`provider/model-id`**, in either client. Add more providers with
`ox provider add`, then relaunch. Codex lists configured models in `/model`. In Claude Code,
`/model` shows [eligible discovered aliases](docs/configuration.md#model-names-and-discovery)
when launched with `ox claude`. Use `--model <alias>` to select any configured model.

## Common tasks

| I want to… | Where to go |
| --- | --- |
| Connect Anthropic, OpenAI, a Chat Completions vendor, or a local server | [Provider setup](docs/configuration.md#add-a-provider) |
| Add Brave citations or configure Claude Code Auto mode | [Search and Claude settings](docs/configuration.md#web-search-with-brave) |
| Resume a session, switch models, or keep my usual ChatGPT sign-in | [Sessions and sign-in](docs/configuration.md#sessions-and-sign-in) |
| Share one ox gateway on a tailnet or connect desktop clients and T3 Code | [Home-server setup](docs/shared-gateway.md) |
| See supported clients, API versions, and feature boundaries | [Compatibility](docs/compatibility.md) |
| Reproduce checks or measure the relay | [Verification and performance](docs/verification.md) |

## If setup stalls

| What you see | What to try |
| --- | --- |
| `ox: command not found` | Run `./ox --help` in the extracted folder. Move the binary to a directory on your `PATH`. |
| Linux reports a missing glibc version, or macOS blocks the downloaded binary | Use the [source installation](docs/configuration.md#install-options) and run `ox --version` again. |
| Client missing, or Codex’s bundled model catalog fails to load | Run `codex --version` or `claude --version` from the same terminal. For Codex also try `codex debug models --bundled`. If the problem began after an update, check the latest-client CI job. |
| `provider … is unavailable` | Run `ox doctor` and load the exact variable it names in the shell that launches ox. |
| `unknown model`, or an empty picker | Pass the full alias from setup. Check the vendor model ID, or pin it with a [static model entry](docs/configuration.md#model-names-and-discovery). |
| Authentication error after a clean `doctor` | Run `ox doctor --live --provider <name>` to check the key, model and streaming with one small API request. |
| Provider returns `404` or rejects the request format | Check that the [base URL and wire format](docs/configuration.md#add-a-provider) match the endpoint. Leave the request path, such as `/chat/completions`, off the base URL. |
| Provider returns `429` | Check that provider’s quota and rate limits, or choose another configured model. |
| A direct OpenAI key returns `codex_byok_restricted_by_api_credit_promo` | That account’s API-credit plan rejected the Codex request in live testing. Use an eligible provider credential, or launch plain `codex` for your usual ChatGPT session. |
| Brave configured but search unavailable | Run `ox doctor` to check that the key variable is set and see any Codex policy conflict. For one live Codex launch, use `ox codex -c 'web_search="live"' --model <alias>`. Brave checks the key when a search runs. [Search setup](docs/configuration.md#web-search-with-brave) |
