# Verification and performance

[Quickstart](../README.md) · [Compatibility matrix](compatibility.md)

## Reproduce the checks

Run these from the repository root. The stock-client fixtures use disposable homes,
dummy credentials and loopback vendor and Brave servers. They do not need API keys.

```sh
cargo fetch --locked
cargo test --locked --offline
cargo build --release --locked --offline
python3 tests/fixtures/setup_selector.py

CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 \
  cargo test --locked --offline --features test-brave --test stock_codex_smoke -- \
    --ignored --test-threads=1

OX_TEST_CLAUDE_BIN=/path/to/claude-2.1.330 \
  cargo test --locked --offline --features test-brave --test stock_claude_smoke -- \
    --ignored --test-threads=1

CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 OX_TEST_CLAUDE_BIN=/path/to/claude-2.1.330 \
  cargo test --locked --offline --test shared_gateway -- --ignored --nocapture

CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 \
  cargo test --locked --offline --test shared_codex_home -- \
    --ignored --test-threads=1

CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 \
  cargo test --locked --offline --bin ox stock_codex_emits_web_run -- --ignored
```

The setup selector check uses a POSIX pseudoterminal and Python 3.11 or later.
It runs the release binary and checks which stock client command each choice
would invoke, including reuse of an existing ChatGPT sign-in. The shared-home
check needs Python 3 and OpenSSL for a throwaway HTTPS fixture.
The coding fixtures need working stock-client sandbox support and permission to bind
loopback sockets. See the [Codex fixtures](../tests/fixtures/stock_codex/README.md)
and [Claude Code fixtures](../tests/fixtures/stock_claude/README.md) for details.

GitHub Actions runs the offline checks and packages native binaries for both
architectures on Linux and macOS. See [install options](configuration.md#install-options).

## Client updates

ox launches the `codex` or `claude` executable on `PATH` with its launch-scoped
provider settings and relay credential. Updating either client takes effect on
the next launch.

The [CI workflow](https://github.com/n0rpan/codex-rust-relay/actions/workflows/ci.yml)
runs the same coding, tool, search, resume and auth scenarios against two inputs:

- **Baseline** uses checksum-pinned Codex 0.171.0 and Claude Code 2.1.330 on PRs and
  pushes to `main`. It keeps a repeatable compatibility reference.
- **Latest** downloads the current official Codex release and Claude Code’s `latest`
  channel using their published SHA-256 values. It runs on PRs, pushes to `main`,
  and daily on the default branch. Each run records the resolved versions in its job
  summary. Use **Run workflow** to check for a new upstream release on demand.

A fixture that fails against a new client turns that job red. Check the Actions
logs for the failing scenario and client version. To reproduce with your installed
client, replace the fixture paths above with the output of `command -v codex` or
`command -v claude`. The tests exercise the behavior of the supplied binary.

Enable **Actions → Only notify for failed workflows** in your GitHub notification
settings for failure alerts. GitHub sends scheduled-run notifications to the account
associated with the schedule. [GitHub Actions notifications](https://docs.github.com/en/actions/concepts/workflows-and-actions/notifications-for-workflow-runs)

## Benchmark a release on Linux

```sh
cargo fetch --locked
cargo build --release --locked --offline
ox_release_copy=$(mktemp -d)
cp target/release/ox "$ox_release_copy/ox"

CODEOX_BENCH_BIN="$ox_release_copy/ox" \
  cargo test --release --locked --offline --test relay_benchmark -- --ignored --nocapture

OX_BENCH_CLIENT=claude CODEOX_BENCH_BIN="$ox_release_copy/ox" \
  cargo test --release --locked --offline --test relay_benchmark -- --ignored --nocapture
```

The two runs take about six minutes in total. For each API the harness samples
60 seconds of warmed idle memory and CPU, 40 paired first-event times against the same
loopback mock, and peak memory with eight concurrent streams. It measures the relay
process alone and excludes real model and network latency.

### Measured release

The ox 0.1.0 release from commit `22dfc62` met its memory, CPU, and added-latency
targets for all six client and protocol combinations. The test ran on a Qualcomm
Oryon host with 12 logical CPUs running Linux 7.3.0-77-omarchy (aarch64) on 2026-11-23.
SHA-256 of the local executable used in the test
`ee83bf98511205f872191c2c0a5084d042eb2c883a24e610e1ca3f051dfa00eb`.

| Client | Upstream API | Added p95 first event | Warm idle RSS | Peak RSS, eight streams |
| --- | --- | ---: | ---: | ---: |
| Codex | Responses | 0.749 ms | 5.586 MiB | 5.875 MiB |
| Codex | Chat Completions | 0.765 ms | 5.672 MiB | 5.961 MiB |
| Codex | Anthropic Messages | 1.181 ms | 5.680 MiB | 5.996 MiB |
| Claude Code | Responses | 0.554 ms | 5.625 MiB | 7.750 MiB |
| Claude Code | Chat Completions | 1.478 ms | 5.664 MiB | 8.074 MiB |
| Claude Code | Anthropic Messages | 0.768 ms | 5.613 MiB | 5.926 MiB |

Idle CPU recorded zero ticks in every 60-second window. The Codex and Claude Code
request fixtures were 2,248 B and 2,469 B. Direct Chat Completions and Anthropic Messages
requests were 1,974 B and 1,692 B. The Responses, Chat Completions and Anthropic SSE
fixtures were 3,184 B, 1,695 B and 1,433 B. The mock held the eight streams for 1,500 ms.
[Benchmark fixtures](../tests/fixtures/benchmark)
