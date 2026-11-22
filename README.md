# Codex Rust Relay

Use a custom OpenAI-compatible provider with Codex CLI. The relay translates the provider's live model list for the `/model` picker and forwards Responses API requests over an authenticated loopback connection.

## Setup

Install the relay with Cargo and make sure `codex` is on your `PATH`.

```sh
cargo install --locked --path .
```

Set these fields in `$CODEX_HOME/config.toml`. The default directory is `~/.codex`. Replace the demo model and URL with your provider's values, and set `env_key` to the name of the environment variable that holds your API key. The relay sends that key to the provider as a bearer token.

```toml
model = "demo-coding"
model_provider = "demo"

[model_providers.demo]
name = "Demo Provider"
base_url = "https://api.example.com/v1"
env_key = "DEMO_API_KEY"
wire_api = "responses"
```

Set the API key in your shell. This Bash example reads it without echoing it.

```bash
read -rsp 'Provider API key: ' DEMO_API_KEY
printf '\n'
export DEMO_API_KEY
codex-provider-relay
```

Use `/model` in Codex. The launcher accepts normal Codex arguments, such as `codex-provider-relay --model demo-coding`.

The provider must support `POST /responses` and an OpenAI-compatible model list. Discovery uses `GET /models` under `base_url` by default. If your provider lists models at another path on the same origin, set `model_catalog_url` in the provider table. Use HTTPS for remote providers. HTTP is supported on loopback for local providers.
