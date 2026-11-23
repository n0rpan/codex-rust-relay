# Run ox on a home server

[Quickstart](../README.md) · [Provider setup](configuration.md) · [Compatibility](compatibility.md)

`ox serve` runs independently of Codex and Claude. It listens on `127.0.0.1:8787`
by default. One server keeps the model registry and vendor keys. Devices connect
using a separate gateway key and run their own clients, tools and sessions.

Run `ox help share`, `ox help desktop`, or `ox help t3` for the steps in your
terminal. For a single host, start with loopback. For clients on another device,
use the HTTPS address from [Tailscale Serve](#2-publish-it-inside-your-tailnet).

## 1. Configure and start ox

Install ox on the server, then create its config and add providers using the same
[provider setup](configuration.md#add-a-provider) commands. For a dedicated config
directory on the server:

```sh
export CODEX_HOME="$HOME/.config/ox"
ox provider add openrouter \
  --base-url https://openrouter.ai/api/v1 --wire responses \
  --env-key OPENROUTER_API_KEY --no-discovery --model openai/gpt-5.4-mini
```

Load `OPENROUTER_API_KEY` and any other vendor keys into the environment on the server.
For a live check, run `ox doctor --live --provider openrouter`. It sends one small
streamed prompt and checks for text. The API request may be billed. Plain `ox doctor`
stays offline.

Generate a new gateway secret with `openssl rand -hex 32`. Store it in your password
manager and load the same value as `OX_GATEWAY_KEY` each time the server starts.
Share that key with the client devices. Keep the upstream vendor keys on the server.

```sh
ox serve --port 8787
```

Use the foreground command to check setup. For persistence on Linux, the
[systemd user-service example](../examples/ox.service) expects
`~/.local/bin/ox`, `~/.config/ox/codeox.toml` and `~/.config/ox/gateway.env`. Keep
`gateway.env` private (mode 0600). Use one `NAME=value` line for each credential,
including `OX_GATEWAY_KEY`. Install the example into `~/.config/systemd/user/ox.service`,
then start it. Stop any foreground copy using port 8787 before enabling the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now ox.service
systemctl --user status ox.service
```

For a server that should run after logout, enable user lingering for the service
account or have the system administrator install a system service for it. Restart
`ox.service` after changing credentials or the provider configuration. The old key
expires on restart if you replace it.

## 2. Publish it inside your tailnet

With Tailscale connected on the server and HTTPS enabled for the tailnet, run:

```sh
tailscale serve --bg http://127.0.0.1:8787
tailscale serve status
```

Use the `https://…ts.net` URL shown by Serve. Keep the host reachable only to the
devices or users you intend to give access to. ox authenticates every model request
with the gateway key. The key gives access to all routes in that ox service. If
this machine already uses Serve on HTTPS port 443, choose another HTTPS port or a
separate host. [Tailscale Serve and access controls](https://tailscale.com/docs/reference/tailscale-cli/serve)

From a second device, load `OX_GATEWAY_KEY` there and replace the example URL:

```sh
printf 'header = "Authorization: Bearer %s"\n' "$OX_GATEWAY_KEY" |
  curl --fail --silent --show-error --config - 'https://ox.example-tailnet.ts.net/v1/models'
```

The response lists model aliases. A bare request without the key should return
HTTP 401. Use the HTTPS origin from Serve for Claude and the same origin plus
`/v1` for Codex.

## 3. Connect the clients

| Client | Endpoint | Credential |
| --- | --- | --- |
| Codex CLI, Codex App Server, or Codex Desktop | `https://…ts.net/v1` | Bearer key from `OX_GATEWAY_KEY` |
| Claude Code or Claude Desktop on 3P | `https://…ts.net` | Gateway bearer key |
| Codex-native catalog | `https://…ts.net/v1/catalog` | Same bearer key |

### Codex Desktop

Open **Settings → Model Providers & Gateways**. If custom gateways are available,
choose **Add → Gateway or custom provider**. Name it **ox**, use the API base URL
including `/v1` and choose **Environment variable** authentication with
`OX_GATEWAY_KEY` as the variable name. If available, select **Set as current
provider**. For manual setup, merge
[the Codex configuration example](../examples/codex-gateway.toml) into the
execution host’s user-level Codex `config.toml`. Replace the URL and alias. Keep the
catalog URL at `/v1/catalog`. Supply `OX_GATEWAY_KEY` to the app process through
your normal desktop secret delivery, then restart the app. A variable exported in
a terminal is available only to processes launched from it.

Start a new local task with the ox provider and ask it to reply `gateway-ok`. Check
the selected provider and model. The gateway key authenticates ox inference.
Desktop sign-in, hosted Codex tasks and app integrations depend on the client’s
own account and policy. [OpenAI’s desktop gateway setup](https://learn.chatgpt.com/docs/enterprise/connect-to-a-gateway#configure-the-provider)

To use standalone Brave search in a Codex client that supports it, configure
`[search]` on the server. In the client config, set `web_search = "live"` at the top
level, `standalone_web_search = true` in `[features]`, and
`supports_standalone_web_search = true` in its ox provider table. The client then
calls the gateway at `/v1/alpha/search`. [Brave setup](configuration.md#web-search-with-brave)

### Claude Desktop on 3P

Use **Configure Third-Party Inference** on a supported desktop installation or use
your managed profile. Set the inference provider to **Gateway**. Use the HTTPS
origin, bearer auth and the gateway key. [Claude Desktop profile example](../examples/claude-desktop-gateway.json)

Start with a native Anthropic route and set `inferenceModels` to the exact route
aliases. In the example, the server needs an `anthropic` provider with model ID
`claude-sonnet-5`. The `behavesAs` field tells Desktop which Anthropic model the
alias represents. Use supported model values matching your provider.
[Claude Desktop gateway configuration](https://claude.com/docs/third-party/claude-desktop/gateway)

Native Anthropic routes preserve successful Messages streams, usage and signed
thinking, and forward bounded native error responses when they contain no
configured credentials. Desktop-specific session recovery, compaction and Auto
mode need end-to-end testing on the installed app. Translated routes lack
Anthropic-only features such as signed thinking and server-side review. The
relay's Brave bridge handles Claude Code’s dedicated search call. For Desktop’s
provider-side search, use a native Anthropic route that supports it or configure
[Desktop’s bundled web tools](../examples/claude-desktop-web-tools.json) separately.

### T3 Code

In **Settings → Providers**, add a separate provider instance named **ox**. Select
the environment that will run the client. Its execution host must be able to
reach the tailnet URL.

- **Codex instance.** Use manual setup with the gateway API URL ending in `/v1`
  and the ox key. Supply the exact model alias as a custom model. For an instance
  backed by its own Codex home, the [Codex config example](../examples/codex-gateway.toml)
  also sets the native catalog URL. Keep the ordinary ChatGPT instance for
  ChatGPT-backed sessions. [T3 Code Codex provider settings](https://github.com/pingdotgg/t3code/blob/main/docs/user/providers-codex.md)
- **Claude instance.** T3 Code runs the Claude Agent SDK with its configured
  Claude runtime. Use a separate Claude config directory if you also sign in
  with Claude on this host. In the instance's **Environment variables**, set
  `ANTHROPIC_BASE_URL` to the HTTPS origin,
  `ANTHROPIC_AUTH_TOKEN` to the gateway key as a sensitive value, and
  `ANTHROPIC_API_KEY` to an explicitly empty value. Add your exact model alias
  as a custom model in T3 Code. For the standalone Claude Code CLI pointed at the
  same daemon, use [model discovery](configuration.md#model-names-and-discovery)
  with the Claude process to populate its own `/model` picker. [T3 Code Claude router settings](https://github.com/pingdotgg/t3code/blob/main/docs/user/providers-claude.md#other-routers)

T3 Code manages which instance, secret and aliases a thread uses. Add or update
vendor routes in the server’s `codeox.toml`. Sessions and tool execution stay in
the selected client environment. Enable the new instance for your project and
select it and its custom model when you start a thread.

For subscription sessions in T3 Code, sign in with the stock client on its
execution host. Select the native Codex or Claude instance that uses that sign-in.
For Claude, T3 keeps using the Agent SDK and its configured runtime. Use the
native instance's Claude config directory when signing in and keep the gateway
environment variables on the separate ox instance. [T3 Code Claude accounts](https://github.com/pingdotgg/t3code/blob/main/docs/user/providers-claude.md#separate-accounts-or-configurations)

## Verification boundary

The repository verifies the shared HTTP endpoint, auth rejection, vendor-key
isolation and reuse of the same port and key after a restart. The opt-in stock
fixture sends a Codex App Server turn and a Claude Code CLI turn to the same
daemon using disposable homes and loopback vendor fixtures. It verifies Codex's
catalog lookup and Claude Code's discovery of Claude aliases across providers.
The Claude Agent SDK was also tested over all three provider protocols with
loopback fixtures, including a local file read on a native Anthropic route.
T3 Code 0.1.11 completed a turn with each of its Codex and Claude provider
instances through one gateway, using an isolated T3 server and dummy keys.
Desktop UI workflows and a real cross-device tailnet connection require the
corresponding installations and device access.

For a Mac smoke test, first check the authenticated `/v1/models` request from
the Mac. Then open a fresh local task in each desktop app, select an ox alias
and ask it to read a small file in a test workspace. Verify the reply streams
and includes a detail from the file. This exercises inference and a local tool
in each app.
