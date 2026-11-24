# Run ox on a home server

[Quickstart](../README.md) · [Provider setup](configuration.md) · [Compatibility](compatibility.md)

`ox serve` runs independently of Codex and Claude. It listens on `127.0.0.1:8787`
by default. One server keeps the model registry and vendor keys. Devices connect
using a separate gateway key and run their own clients, tools and sessions.

The worker MCP server runs inside an `ox codex` or `ox claude` subscription
launch on the client machine. It serves that launch's jobs over loopback.
[Run a background worker](configuration.md#subscription-main-session-with-api-workers)

Run `ox help share`, `ox help desktop`, or `ox help t3` for the steps in your
terminal. For a single host, start with loopback. For clients on another device,
use the HTTPS address from [Tailscale Serve](#2-publish-it-inside-your-tailnet).

## 1. Configure and start ox

[Install ox](configuration.md#install-options) on the server and run `ox setup` as the user who will run the service.
Choose **Add an API key**, select the service, and paste its key. For example,
to use the config directory expected by the sample systemd service:

```sh
export CODEX_HOME="$HOME/.config/ox"
ox setup
ox doctor
```

ox serve loads the keys saved by the wizard for that directory. You can also
[configure providers with environment variables](configuration.md#scripted-or-advanced-setup).
For a live check, run `ox doctor --live --provider openrouter`. It sends one small
streamed prompt and checks for text. The API request may be billed. Plain `ox doctor`
stays offline.

Generate a new gateway secret with `openssl rand -hex 32`. Store it in your password
manager and load the same value as `OX_GATEWAY_KEY` each time the server starts.
Share that key with the client devices. Keep the upstream vendor keys on the server.

```sh
ox serve --port 8787
```

If the port is taken, ox reports `port 8787 is already in use` and its `fix:`
line gives the next port as a ready command. ox never picks another port by
itself, because your clients are configured with the port number.

Use the foreground command to check setup. For persistence on Linux, the
[systemd user-service example](../examples/ox.service) expects
`~/.local/bin/ox`, `~/.config/ox/codeox.toml` and `~/.config/ox/gateway.env`. Keep
`gateway.env` private (mode 0600). Put `OX_GATEWAY_KEY` there, plus one `NAME=value`
line for each vendor key you manage in the environment. Install the example into `~/.config/systemd/user/ox.service`,
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

The desktop app stores gateway settings you apply. Adding a provider and
selecting it as current changes that app's saved settings. For a manual
configuration with separate settings and history, put the example in
`config.toml` under a dedicated `CODEX_HOME` and launch the client with that
environment variable.

Open **Settings → Model Providers & Gateways**. If custom gateways are available,
choose **Add → Gateway or custom provider**. Name it **ox**, use the API base URL
including `/v1` and choose **Environment variable** authentication with
`OX_GATEWAY_KEY` as the variable name. If available, select **Set as current
provider**. For manual setup, replace the URL and alias in
[the Codex configuration example](../examples/codex-gateway.toml) and keep the
catalog path at `/v1/catalog`. Supply `OX_GATEWAY_KEY` to the app process through
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

1. Open **Developer → Configure Third-Party Inference…** in Claude Desktop on a
   supported device. Under **Connection**, pick **Gateway** and **Static API key**.
   Enter the Tailscale HTTPS origin, the `OX_GATEWAY_KEY` value, and **Bearer**.
   **x-api-key** also works with ox. The gateway key grants access to the shared
   ox server. Keep the completed profile private.
2. Pick a native Anthropic model for the first test. The example profile uses
   `anthropic/claude-sonnet-5`. Change `inferenceModels[].name` to the exact ID
   returned by the gateway's `/v1/models`. Set `behavesAs` to the underlying
   Claude model ID. Without an explicit list, Desktop filters discovery to IDs
   it recognizes as Claude. ox advertises known model limits and Claude family
   tiers for native Messages routes.
3. Apply the settings and choose **Save & Restart**. Start a new Chat or Code
   session. Confirm that the selected model replies and that a Code session can
   read a file in a disposable project. Use the in-app export for a managed
   deployment. [Example profile](../examples/claude-desktop-gateway.json) · [Anthropic setup](https://claude.com/docs/third-party/claude-desktop/gateway#configure-the-app)

For Alibaba and other aliases Desktop cannot discover as Claude models, add the
exact ox alias under **Models**. For example, enter
`alibaba-messages/deepseek-v4.1-flash-1017` and give it the display name
**DeepSeek V4.1 Flash · Alibaba**. Desktop sends that alias to ox and ox sends the
underlying model ID to Alibaba. To set a Claude capability profile, open the entry's
**Claude model behind this ID** field (`behavesAs` in JSON) and enter the full
Claude ID, for example `claude-sonnet-5-5`. This changes effort and context
assumptions. Use it only when the route supports the settings Desktop will send.
For an Alibaba model, start with its actual context limit and a clear display
name. [Desktop model controls](https://claude.com/docs/third-party/claude-desktop/models#map-an-unrecognized-model-id-to-a-claude-model)

**Context and caching.** Set `modelPrefer1mContext` for discovery, or `supports1m`
on a matching `inferenceModels` entry, when the selected route supports 1M. ox
advertises `supports_1m` when it knows the model has at least 1M input tokens.
For unrecognized aliases with a different window, Desktop can use
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` in Claude Code managed settings. Choose the
smallest supported limit among the aliases that share that setting.
On native routes ox retains 1-hour `cache_control` markers, cache creation and
read usage, signed thinking, SSE pings, and the provider's compaction error
wording. The provider determines which features it accepts. Check cache reads on
subsequent turns and run `/compact` in a long test session on the Mac. Keep
buffering off and use a proxy timeout long enough for compaction streams.
[Context and compaction](https://claude.com/docs/third-party/claude-desktop/gateway#context-window-and-compaction)

**Auto mode and MCP tool search.** Server-side Auto mode works only with an
upstream that returns real verdicts. Test each model in a new local Code session,
then look for `cli_api_auto_mode_server_check` with state `verdict` under
**Help → Troubleshooting → Generate Diagnostic Report**. For a translated or
otherwise incompatible route, set `inferenceGatewayAutoModeServerChecks` to
`false` on the same Desktop profile to select the built-in check while it is
available. `toolSearchEnabled` is a separate opt-in. Turn it on only for routes
that accept the tool-search beta, `defer_loading`, and `tool_reference`.
[Auto mode diagnostics](https://claude.com/docs/third-party/claude-desktop/gateway#test-auto-mode-checks-on-one-device)

The `ox serve` tests cover the API contract with loopback providers. A live
Sonnet 5 probe through ox cached 2,926 tokens on a 1-hour breakpoint and read the
same 2,926 tokens on the next request. The Desktop picker, actual compaction, and
Auto mode verdicts still need the Mac app check. A provider's native `402` billing
error is preserved. ox serves a
static gateway key and does not issue per-user budgets or validate enterprise
OIDC tokens. For Desktop search, use an upstream that supports it or configure
[Desktop’s bundled web tools](../examples/claude-desktop-web-tools.json).

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
  Claude runtime. In the instance's **Environment variables**, set
  `CLAUDE_CONFIG_DIR` to a dedicated directory on the execution host to keep
  its settings and history separate from your usual Claude Code profile.
  Set `ANTHROPIC_BASE_URL` to the HTTPS origin,
  `ANTHROPIC_AUTH_TOKEN` to the gateway key as a sensitive value, and
  `ANTHROPIC_API_KEY` to an explicitly empty value. Add your exact model alias
  as a custom model in T3 Code. For the standalone Claude Code CLI pointed at the
  same daemon, use [model discovery](configuration.md#model-names-and-discovery)
  with the Claude process to populate its own `/model` picker. [T3 Code Claude router settings](https://github.com/pingdotgg/t3code/blob/main/docs/user/providers-claude.md#other-routers)

T3 Code manages which instance, secret and aliases a thread uses. Add or update
vendor routes in the server’s `codeox.toml`. Sessions and tool execution stay in
the selected client environment. Enable the new instance for your project and
select it and its custom model when you start a thread.

For a native subscription session on a remote execution host, sign in with the
corresponding stock client on that host under the account that runs it. Connect
to that host using your client's remote execution support. The shared ox API
gateway uses provider API keys and its own gateway key. For Claude models in
Codex, add a Claude Console API provider to ox and select its alias in Codex.
[Claude Code credential use](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use)

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
