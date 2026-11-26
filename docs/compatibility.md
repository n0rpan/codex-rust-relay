# What works with ox

[Quickstart](../README.md) · [Configuration](configuration.md) · [Verification](verification.md)

## Verified client and API combinations

The baseline matrix uses unmodified CLI binaries and loopback provider and Brave fixtures.
“Coding” covers file reads, edits, and test commands. “Resume” restarts ox and resumes
the same client session. “Switch” changes the vendor in the same Codex session.

| Upstream API | Codex 0.171.0 | Claude Code 2.1.330 |
| --- | --- | --- |
| OpenAI Responses | Coding, Brave citations, resume, switch | Coding, Brave citations, resume |
| OpenAI Chat Completions | Coding, Brave citations, resume, switch | Coding, Brave citations, resume |
| Anthropic Messages `2023-06-01` | Coding, Brave citations, resume, switch | Coding, Brave citations, resume |
| Anthropic Messages `2026-10-21` | Coding, Brave citations, resume, switch | Coding, Brave citations, resume |

Stock Codex and Claude Code fixtures compare the native profile trees before
and after API-backed launches. They verify that relay sessions use separate
settings and history. The clients execute tools locally. [Client profiles](configuration.md#sessions-and-sign-in)
ox launches the client found on `PATH`. For the current verified versions, see the
`Stock clients (latest)` job in the [latest CI run](https://github.com/oxstack/ox/actions/workflows/ci.yml).

Subscription sessions started through ox can delegate to API workers through
MCP. The local fixtures cover a stock Codex goal, a stock Claude Code workflow,
workspace edits, and cancellation. [Background workers](configuration.md#subscription-main-session-with-api-workers)

### Live provider checks

These checks used live provider accounts. Auto mode was exercised in these runs.
The loopback suite above checks tool execution and permissions separately.

| Provider and model | Codex | Claude Code |
| --- | --- | --- |
| OpenRouter Responses · `openai/gpt-5.4-mini` | Coding, restart and resume, Brave citations | Read and edit, including Auto mode with the Anthropic classifier route |
| Alibaba Chat Completions · `deepseek-v4.1-flash-1017` | Coding | Read and edit |
| Anthropic Messages · `claude-haiku-5-5` | Coding on both tested API versions | Brave citations on `2023-06-01` |
| Anthropic Messages · `claude-sonnet-5` | Outside the live checks | Read and edit in Auto mode on `2026-10-21` |

Direct OpenAI returned `codex_byok_restricted_by_api_credit_promo` for the API-credit
account tested. The Responses success above used a separate OpenRouter account.
Tool availability, context windows, quotas and key permissions depend on the provider and model.

## Claude Code gateway protocol

Use `ANTHROPIC_BASE_URL` with `ox serve`, or let `ox claude` configure the
connection for you. ox exposes the Anthropic Messages format and routes each
model to the API configured for its provider.
[Claude Code gateway guide](https://code.claude.com/docs/en/llm-gateway-protocol)

| Feature | What ox supports |
| --- | --- |
| Messages and authentication | `POST /v1/messages` accepts Claude Code's `?beta=true` request. Authenticate to ox with a bearer token or `x-api-key`. The provider uses its own configured API credential. |
| Native Anthropic streaming | Streams the upstream event sequence directly, including pings, partial tool arguments, signed thinking and safeguards. Fine-grained tool streaming is a [client opt-in](configuration.md#fine-grained-tool-streaming). |
| Native Anthropic request fields | Preserves system block order, cache markers and beta body fields. Forwards bounded `anthropic-*` capability headers, including repeated beta values. A configured `anthropic_version` wins over the client's version. With no pin, ox uses a valid client version or the `2023-06-01` default. |
| Retries and limits | Native Messages preserves standard bounded error envelopes and retry or usage-limit metadata when those values contain no configured credential. A synthesized error can lack the details Claude Code uses for automatic feature recovery. |
| Model discovery | Authenticated `GET /v1/models` reports the aliases and any known context limits. Claude Code's automatic discovery filters IDs to names containing `claude` or `anthropic`. Use ox's [starred lineup and context settings](configuration.md#model-names-and-discovery) for other models. |
| Translated streaming | Delivers text, tool events and terminal status. After 15 seconds without a downstream event, ox sends a ping while waiting for the next upstream event. The upstream read timeout still applies. Use [Claude Code's classifier](configuration.md#claude-code-auto-mode) for safety review. |
| Token counting | `POST /v1/messages/count_tokens` uses the selected provider's counting endpoint. The Responses adapter translates the prompt and tools for `/responses/input_tokens`. If counting is unavailable, including on Chat Completions routes, Claude Code can estimate usage locally. |
| Connection warm-up | `HEAD /api/hello` returns `204` locally. It uses no credentials and makes no provider request. |
| Other connection methods | Bedrock InvokeModel, Vertex `rawPredict`, AWS workspace headers and Claude apps gateway sign-in are outside this implementation. Stock subscription sign-ins run in their native clients. |

ox authenticates upstream with the provider credential. It filters client auth,
workspace, and hop-by-hop headers before forwarding API capability headers.
Only direct Anthropic requests carry the client's `beta=true` query flag.
For older Claude models behind custom aliases,
you can set `modelOverrides` in Claude Code to give the alias the upstream
model's capabilities. Claude Code's fast-mode availability check and WebFetch
domain check use its own direct connection to `api.anthropic.com`.
[Custom model capabilities](https://code.claude.com/docs/en/llm-gateway-protocol#settings-for-unrecognized-model-ids)

## Where API translation ends

- **Native paths.** Codex to a Responses provider streams the upstream response directly.
  Claude Code to Anthropic preserves Messages features the selected upstream supports,
  including signed thinking, cache markers, and Auto mode safeguards. The relay applies
  the upstream model ID and the effective Anthropic API version's sampling rules.
- **Client tools.** Coding tools run in the CLI. For translated Codex requests, ox maps
  functions, namespaced calls such as `web.run`, and freeform `apply_patch` into the
  upstream tool format. Code-mode `additional_tools` and arbitrary provider-hosted
  tools are outside that mapping. Native Anthropic server tools depend on the upstream.
- **Attachments and output formats.** Codex to Anthropic maps user and tool-result
  images (base64 data URLs and URLs) but rejects files, audio and structured output.
  The Chat Completions adapter maps supported user and tool-result images, and
  structured-output requests. Claude Code's translated paths handle supported
  inline or URL images and reject document and file-ID blocks. Use the native
  route if your workflow relies on attachments or provider-specific output.
- **Conversation state.** Translated Codex requests require streaming and client
  replay. They reject provider-side continuation via `previous_response_id`,
  `conversation`, `store: true`, background mode, and item references. Compaction
  for translated routes is local to Codex. Claude Code switching within a session
  is outside the verified matrix. Use a new session when changing model or vendor.
- **Anthropic details.** The Codex bridge replays signed thinking for the matching route.
  Parallel Anthropic tool calls can still arrive when Codex asks to disable parallel tools.
  `pause_turn` becomes a failed turn on the Anthropic-to-Codex adapter.
- **Claude Code translation.** Responses and Chat Completions cannot supply Anthropic
  signed thinking or server-side Auto mode verdicts. Use [Claude Code’s classifier](configuration.md#claude-code-auto-mode)
  for translated models. Provider-hosted containers have no equivalent in these routes.
  Anthropic cache-control markers are replaced by each provider's own caching.
  [What ox does on each route](verification.md#how-ox-stays-fast)
- **Streaming.** If a vendor promises a streaming API but sends a JSON body for a
  streaming request, translated inference can fail. A translated stream failure is
  surfaced to the client. A native Responses stream can end with a transport error.
  Restart or retry after checking the selected endpoint.

## Search and fetch

| Feature | What to expect |
| --- | --- |
| Codex `web.run` via Brave | Live text queries and citation links. Up to four queries and eight domains per filter group. Recency accepts 1, 7, 30 or 365 days. |
| Codex page open, click, find, images, screenshots or other standalone operations | The Brave bridge returns a tool error. It supports `search_query` only. |
| Codex domain rules | Allowed domains filter returned URLs. The bridge rejects a blocked-domain filter. |
| Claude Code `WebSearch` via Brave | One Brave query per dedicated CLI search request. Use allowed domains or blocked domains separately. Country filters and citation links are supported. The tested CLI can discover the tool with `ToolSearch`. |
| Claude Code `WebFetch` | The CLI fetches under its own permissions and network settings. ox routes any follow-up model call. |
| Brave data flow | Brave receives the explicit search query and supported filters. Search output is visible to the inference provider once the client includes it in conversation history. |

Codex search requests are capped at 1 MiB and rendered output at 12 KiB per query
(48 KiB per request). Claude Code receives a bounded text summary and citation metadata.
Both use the configured `search.max_results` and timeout. Brave search and provider-hosted
search may have separate costs. [Enable search](configuration.md#web-search-with-brave)

## Desktop and hosted clients

The local launchers bind an ephemeral loopback address and exit with their clients.
`ox serve` supplies a stable endpoint and gateway key for external clients. The
shared endpoint is tested against stock Codex App Server and Claude Code CLI.
The Claude Desktop 3P API fixture checks model capability metadata, 1-hour
caching fields, streaming tool IDs and verdicts, and native context-limit and
billing errors. Desktop app UI flows, hosted sessions, Desktop compaction and
cross-device tailnet access still require device testing.
[Claude Desktop setup and verification](shared-gateway.md#claude-desktop-on-3p)

The [Desktop web-tools overlay](../examples/claude-desktop-web-tools.json) is a
configuration example for an existing Desktop 3P deployment. Set your allowed fetch hosts
and point `headersHelper` at an executable that returns a JSON header map with
`X-Subscription-Token`. Desktop's bundled Brave connector runs separately from ox.
The example allows search in Chat and Cowork and asks before Web Fetch. Code sessions
apply their own tool permissions and shell sandbox rules. [Claude Desktop web tools](https://claude.com/docs/third-party/claude-desktop/web-tools)

## Runtime limits and credentials

- The relay binds only to `127.0.0.1`. Local launches use a new 256-bit token.
  The persistent gateway uses the separately configured `OX_GATEWAY_KEY`.
  It attaches the configured upstream credential to that route, strips inbound
  auth and hop-by-hop headers, and does not follow upstream redirects. Responses redirects return an error
  with no redirect URL.
- Claude Code starts with a private launch settings file that preserves unrelated
  settings and is removed on normal exit. Conflicting managed routing or
  credential rules can block launch.
- Model discovery runs up to four requests at a time, with a 20-second startup
  budget and an 8 MiB response cap per vendor. Failed discovery leaves static entries available. Only the selected provider
  receives inference requests. If it fails, select another provider explicitly.
- The relay allows 10 seconds to read incoming HTTP headers. Routed request bodies
  are capped at 32 MiB. Translated streams are bounded to
  1 MiB per SSE event and 32 MiB of upstream data. The inference read timeout is
  300 seconds between reads. An active stream can last longer.
- Successful native Responses stream bytes are forwarded. Bounded native Messages
  error envelopes pass through when they contain no configured credentials.
  Other HTTP error details are reduced and redacted. Buffered native Messages
  replies also keep numeric `Retry-After`, boolean `x-should-retry`, and bounded
  unified rate-limit headers when they contain no configured credential.
  Translated failures preserve numeric `Retry-After` on `429` where supported.
  Closing the client stream cancels the upstream request.
- Client connections use `TCP_NODELAY`. After a translated stream has sent its
  final event, ox reads the short rest of the provider's HTTP body in the
  background, for at most 2 seconds and 64 KiB, so the connection is reused by
  the next request. A failed or cut-off stream drops its connection at once.
