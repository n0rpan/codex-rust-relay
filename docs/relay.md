# The ox relay and CLI

This page covers the standalone `ox` CLI: the small relay that lets stock Codex and Claude Code use any model API. For OX Desktop, the session host and the phone, start at the [README](../README.md).

## Why ox

- **Any model in either client.** Use a Claude model in Codex, a GPT model in
  Claude Code, or an OpenRouter, OpenCode Go, Alibaba Cloud or self-hosted
  model in both. The alias is the same everywhere: `provider/model-id`.
- **One setup for both clients.** Providers, keys, starred models and Brave web
  search are configured once and shared by `ox codex` and `ox claude`.
- **Subscription and API side by side.** When you are signed in, the picker
  lists your ChatGPT or Claude plan next to your API models. API sessions use
  a separate ox profile, so your usual settings and history stay untouched.
- **Background workers.** Keep the main conversation on your subscription and
  ask it to start a Codex goal or a Claude Code workflow on an API model. ox
  runs it in the background and gives you a job ID.
- **One gateway for every device.** `ox serve` shares the same providers with
  Codex Desktop, Claude Desktop, T3 Code and other machines on your tailnet.
  The provider keys stay on the host.
- **It tells you the fix.** `ox doctor` ends with the one command to run next,
  `ox doctor --fix` repairs what ox can repair itself, and every ox error
  carries a `fix:` line you can copy.
- **You do not feel it.** ox adds well under a millisecond to the first token
  and about a tenth of a millisecond per token after that. It uses no CPU when
  idle and about 6 MiB of memory. The numbers and their conditions are in
  [Speed](#speed).
## How it works

```text
  codex / claude          ox                          your provider
  (stock CLI, tools  -->  127.0.0.1, one process, --> OpenAI Responses,
   run locally)           one thread                  Chat Completions or
        HTTP on loopback                HTTPS         Anthropic Messages
```

- **ox starts the stock client for you.** It points that one launch at a
  loopback address and gives it a random token that is valid only for that
  launch. The client is unmodified and runs its tools on your machine as usual.
- **Same format in and out is passed through.** Codex to a Responses API and
  Claude Code to a Messages API have their streams forwarded as they arrive.
- **Different formats are translated.** ox rewrites the request and reframes
  the streamed reply one event at a time, including tool calls. It never
  buffers a whole reply. [Where translation ends](compatibility.md#where-api-translation-ends)
- **What stays on your machine.** ox listens only on `127.0.0.1`. A provider
  key is sent only to the provider URL it was saved for and is kept out of the
  client's environment. Pasted keys live in a private file (mode 0600). ox has
  no telemetry and keeps no log of prompts or replies.
- **What does leave.** Your prompts and tool results go to the provider you
  selected, and that provider bills you. With Brave enabled, search queries go
  to Brave.
## Speed

Measured on 2026-11-23 with a release build, on a 4-vCPU Intel Xeon 2.1 GHz
virtual machine running Linux 6.18. Client, ox and a mock provider all ran on
loopback, so these numbers are the overhead ox adds. Model and network time are
excluded.

| What | Measured |
| --- | --- |
| Time added before the first token | p95 under 0.6 ms on all six client and API combinations |
| Delay added to each streamed token | about 0.1 ms at the median, at most 0.15 ms at p95, at 200 tokens per second |
| Tokens per second | unchanged: total stream time +0.0% |
| CPU while idle | 0% in a 10-second sample |
| Memory | about 6 MiB resident when idle |
| Size | one executable of about 5.6 MB, one thread, no background process |

A few things keep it there. Each token costs three system calls (wait, read,
write) on one thread, with no batching between provider and client. Client
sockets use `TCP_NODELAY`, so a client that is busy rendering does not stall
the tokens behind it. Provider connections are opened while the client
starts, shared over HTTP/2 and kept alive through pauses, so requests do not
pay a new TCP and TLS handshake. Where a provider caches prompts only on
request, ox makes the request, so a long conversation is not processed from
scratch on each turn.
[How to reproduce these numbers](verification.md#how-ox-stays-fast)
## Use it

**Pick a model.** `ox codex` and `ox claude` open the same picker. Type to
search, **↑/↓** to move, **Tab** to star, **Enter** to launch. Stars stay at
the top in both clients. **Alt+↑/↓** reorders them, **Ctrl+F** shows only
stars, **Esc** closes the picker. To skip it:

```sh
ox codex --model openrouter/openai/gpt-5.4-mini
ox claude --model openrouter/openai/gpt-5.4-mini
```

**Check it end to end.** Ask **“Read this project’s README and tell me how to
run its tests. Leave the files as they are.”** A reply grounded in the file
confirms both the model connection and a local file read.

**Add a provider from a script.** Load the key into `OPENROUTER_API_KEY` from
your secret manager first:

```sh
ox setup openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --wire responses \
  --env-key OPENROUTER_API_KEY \
  --no-discovery --model openai/gpt-5.4-mini

ox doctor
```

**Use both in one session.** Pick your subscription in `ox codex` or
`ox claude`, then ask **“List my API models and subscription routes.”** Pick an
alias and ask **“Start a Codex goal worker on that model to fix the failing
tests. Let it edit this project and tell me the job ID.”** The main
conversation stays on your subscription, and ox runs the worker in the
background on the API provider you chose.
[Delegate to an API worker](configuration.md#subscription-main-session-with-api-workers)

**Resume.** API sessions live in their own ox profile. Use `ox codex resume`
or `ox claude --resume <session-id>`.
[Profiles and session recovery](configuration.md#sessions-and-sign-in)
## Release channels

The table covers the standalone CLI. On a managed desktop installation, `ox update` updates the entire package. Older desktop installations need one run of the installer with `--desktop` first.

| Channel | What it is | Version looks like | Install | Stay current |
| --- | --- | --- | --- | --- |
| **stable** | Tagged releases, `vX.Y.Z` | `0.1.0` | `curl … \| sh` | `ox update` |
| **dev** | One pre-release, rebuilt from every commit on `main` | `0.0.1-dev.42` | `curl … \| sh -s -- --dev` | `ox update` |

- **No stable release has been tagged yet.** Today the dev pre-release is the
  only download. The plain install command notices that and installs it.
- **Dev versions** are the latest stable version plus one patch, followed by
  `-dev.` and the CI run number. With no stable release that is `0.0.1-dev.N`.
  Every dev version sorts below the stable release it leads up to.
- **A build follows its own channel.** `ox update --check` looks for a newer
  release and `ox update` installs it over the running executable after
  verifying its SHA-256 digest. `ox update --dev` and `ox update --stable`
  switch channels.
- **Manual download.** Every release has `ox-<target>.tar.gz` and a matching
  `.sha256` for four platforms on the
  [Releases page](https://github.com/oxstack/ox/releases).
  [Manual steps, platform names and source builds](configuration.md#install-options)
## Common tasks

| I want to… | Where to go |
| --- | --- |
| Connect Anthropic, OpenAI, a Chat Completions vendor, or a local server | [Provider setup](configuration.md#add-a-provider) |
| Pin a model, rename an alias, or understand the picker and `/model` | [Model names and discovery](configuration.md#model-names-and-discovery) |
| Add Brave citations or configure Claude Code Auto mode | [Search and Claude settings](configuration.md#web-search-with-brave) |
| Resume a session, switch models, or keep my usual ChatGPT sign-in | [Sessions and sign-in](configuration.md#sessions-and-sign-in) |
| Keep my subscription and delegate to an API model | [API workers](configuration.md#subscription-main-session-with-api-workers) |
| Share one ox gateway on a tailnet, or connect desktop clients and T3 Code | [Home-server setup](shared-gateway.md) |
| Install by hand, switch channels, or build from source | [Install options](configuration.md#install-options) |
| See supported clients, API versions, and feature boundaries | [Compatibility](compatibility.md) |
| Reproduce the checks or measure the relay | [Verification and performance](verification.md) |

## Troubleshooting

Start with the doctor. It works offline, never prints a key, and its last line
is the command to run next:

```text
$ ox doctor
…
provider openrouter
  · wire: responses
  ✗ credential reference OPENROUTER_API_KEY is not set; add the key in ox setup or export it before launching
  ✓ validation: ok

summary: 1 provider(s), 1 error(s), 0 warning(s)
next: ox setup   (choose the same service again to paste its key)
```

- `ox doctor` only reads. It never changes a file.
- `ox doctor --fix` repairs what ox can repair itself, then runs the check. An
  unreadable `codeox.toml`, saved-key file or starred-models file is moved to
  `<name>.broken-<time>` next to the original. Nothing you wrote is deleted,
  and running it twice is safe.
- `ox doctor --live --provider NAME` sends one small real request to check the
  key, the model and streaming. It can be billed.

[Everything the doctor checks and repairs](configuration.md#check-and-repair-a-setup)

Errors from ox name the problem and the command that fixes it:

```text
ox: unknown command "docter"
  fix: ox doctor   (to send "docter" to Codex instead: ox codex docter)
```

| What you see | What to do |
| --- | --- |
| `ox: command not found` after installing | Add `~/.local/bin` to your `PATH`. The installer prints the exact line for your shell. |
| The installer reports glibc older than 2.39, or musl | [Build from source](configuration.md#build-from-source). |
| `no stable ox release has been published yet` from `ox update` | Run `ox update --dev`. This is expected until the first stable tag. |
| ``could not find `codex` on PATH`` (or `claude`) | Run the install command on the error's `fix:` line. ox starts the stock client and does not bundle it. |
| `could not parse codeox.toml` | Run `ox doctor --fix`, then `ox setup`. Saved keys are kept. |
| `provider … is unavailable`, or a provider has no key | Run `ox doctor`. Its `next:` line says what to do: usually `ox setup`, choosing the same service again to paste its key. You can also export the variable it names. |
| `unknown provider … in --model` | Use the alias on the `fix:` line. `ox doctor` lists your providers and static aliases. |
| `unknown command` for a one-word prompt | Send it with `ox codex <word>`, or quote a longer prompt. |
| `unknown model`, or an empty picker | Pass the full alias. Check the vendor model ID, or pin it with a [static model entry](configuration.md#model-names-and-discovery). |
| Authentication error after a clean `doctor` | Run `ox doctor --live --provider <name>`. |
| Provider returns `404` or rejects the request format | Check that the [base URL and wire format](configuration.md#add-a-provider) match the endpoint. Leave the request path, such as `/chat/completions`, off the base URL. |
| Provider returns `429` | Check that provider’s quota and rate limits, or choose another configured model. |
| `port … is already in use` from `ox serve` | Stop the other program, or use the port on the `fix:` line and update your client settings to match. |
| Codex’s bundled model catalog fails to load | Run `codex --version` and `codex debug models --bundled` from the same terminal. If it began after a client update, check the [latest-client CI job](verification.md#client-updates). |
| Plain `codex` or `claude` opens on an ox alias saved by an older build | Use `/model` in the plain client to choose a subscription model, then start a fresh session. [Profiles and session recovery](configuration.md#sessions-and-sign-in) |
| A direct OpenAI key returns `codex_byok_restricted_by_api_credit_promo` | That account’s API-credit plan rejected the Codex request in live testing. Use an eligible provider credential, or launch plain `codex` for your usual ChatGPT session. |
| Brave configured but search unavailable | Run `ox doctor` to check that the key variable is set and see any Codex policy conflict. For one live Codex launch, use `ox codex -c 'web_search="live"' --model <alias>`. [Search setup](configuration.md#web-search-with-brave) |
