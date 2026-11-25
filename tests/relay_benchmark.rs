#![cfg(target_os = "linux")]
//! Offline, explicitly invoked performance benchmark for the codeox relay.
//!
//! This test is `#[ignore]`d on purpose: it waits for real wall-clock time,
//! launches a real relay process, and reports machine-dependent numbers, so it
//! must never run as part of the normal test suite.
//!
//! What it measures, using only the supplied release `ox` executable:
//!
//! * warmed idle RSS and CPU for a default 60 s;
//! * added time to the first SSE event compared with direct calls to the same
//!   loopback mock, on representative prompt/stream payloads;
//! * peak relay RSS while eight synchronized streams are in flight.
//!
//! Two inbound client shapes are available, selected with `OX_BENCH_CLIENT`:
//!
//! * `codex` (the default, historical behaviour) posts the Responses coding
//!   fixture to `/v1/responses`;
//! * `claude` posts an equivalent Anthropic Messages coding fixture to
//!   `/v1/messages`, the endpoint and payload shape stock Claude Code sends to
//!   `ox claude`.
//!   The same direct mock packets and the same three vendor wires are reused,
//!   so the numbers stay comparable across clients and wires.
//!
//! The fake stock launcher only keeps the relay alive. In `codex` mode it
//! answers `debug models --bundled`; in `claude` mode it records the
//! `ANTHROPIC_BASE_URL` origin and ephemeral `ANTHROPIC_AUTH_TOKEN` the relay
//! hands it. Either way the launcher is excluded from all process metrics and
//! never carries measured traffic: timed requests come only from this harness's
//! loopback HTTP client, and only the relay's `/proc/<pid>` values are read.
//!
//! # Run it
//!
//! ```sh
//! # Keep the exact artifact outside the build tree: cargo test --release may
//! # rebuild target/release/ox from whatever source is checked out.
//! cp /abs/path/to/release/ox /tmp/codeox-release-bench
//! CODEOX_BENCH_BIN=/tmp/codeox-release-bench \
//!   cargo test --test relay_benchmark --release -- --ignored --nocapture
//! ```
//!
//! For a quick local smoke check (not official numbers):
//!
//! ```sh
//! cp target/debug/ox /tmp/codeox-debug-bench
//! CODEOX_BENCH_BIN=/tmp/codeox-debug-bench \
//! CODEOX_BENCH_IDLE_SECS=2 CODEOX_BENCH_SAMPLES=8 CODEOX_BENCH_WARMUPS=3 \
//! CODEOX_BENCH_STREAM_HOLD_MS=300 \
//!   cargo test --test relay_benchmark -- --ignored --nocapture
//! ```
//!
//! For the Claude inbound shape (quick smoke):
//!
//! ```sh
//! OX_BENCH_CLIENT=claude \
//! CODEOX_BENCH_BIN=/tmp/codeox-debug-bench \
//! CODEOX_BENCH_IDLE_SECS=2 CODEOX_BENCH_SAMPLES=8 CODEOX_BENCH_WARMUPS=3 \
//! CODEOX_BENCH_STREAM_HOLD_MS=300 \
//! CODEOX_BENCH_PROTOCOLS=anthropic_messages,responses,chat_completions \
//!   cargo test --test relay_benchmark -- --ignored --nocapture
//! ```
//!
//! # Knobs (all optional)
//!
//! * `OX_BENCH_CLIENT` - `codex` (default) or `claude`. Selects which stock
//!   client protocol posts to the relay. `claude` exercises the `ox claude`
//!   Anthropic Messages gateway over every requested vendor wire; the launcher
//!   itself is a fake probe and contributes no client latency.
//! * `CODEOX_BENCH_BIN` - required executable under test. There is no implicit
//!   fallback, because `cargo test --release` can rebuild
//!   `target/release/ox`; copy the release binary to a stable path first.
//! * `CODEOX_BENCH_IDLE_SECS` - idle window, default 60.
//! * `CODEOX_BENCH_SAMPLES` - latency pairs, default 40.
//! * `CODEOX_BENCH_WARMUPS` - warmup pairs, default 10.
//! * `CODEOX_BENCH_STREAMS` - concurrent streams, default 8.
//! * `CODEOX_BENCH_STREAM_HOLD_MS` - how long each mock stream stays open,
//!   default 1500.
//! * `CODEOX_BENCH_PROTOCOLS` - comma list (responses, chat_completions,
//!   anthropic_messages); defaults to all three. Every requested protocol
//!   must complete for the benchmark to pass.
//! * `CODEOX_BENCH_KEEP=1` - keep the isolated temp directory and process logs.
//! * `CODEOX_BENCH_ALLOW_MISS=1` - report target misses without failing.
//!
//! Targets default to 15 MiB warm idle RSS, 0.1% idle CPU, added p95 first
//! event latency of 2 ms for Responses and 5 ms for translated protocols,
//! and 32 MiB peak RSS with eight streams. Use the `CODEOX_BENCH_MAX_*`
//! overrides only for diagnostic runs.
//!
//! Network access is loopback-only. Upstream credentials are fixed fake strings
//! passed explicitly to the relay; the relay's environment is cleared before
//! spawn so no real API keys or proxy settings leak into the benchmark.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const ALIAS: &str = "mock/bench-model";
const MODEL: &str = "bench-model";
/// Fake vendor key injected by the relay into upstream requests.
const RELAY_UPSTREAM_KEY: &str = "bench-fake-vendor-key-not-real";
/// Different fake key used only for direct-to-mock baseline calls, so mock
/// hits can prove the relay really used the configured vendor key.
const DIRECT_KEY: &str = "bench-direct-key-not-real";
const KEY_ENV: &str = "CODEOX_BENCH_UPSTREAM_KEY";
const CLIENT_ENV: &str = "OX_BENCH_CLIENT";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HEAD_BYTES: usize = 128 * 1024;

#[test]
#[ignore = "wall-clock benchmark; run explicitly with CODEOX_BENCH_BIN=<release codeox>"]
fn relay_benchmark() {
    let cfg = BenchConfig::from_env();
    let bin = cfg
        .bin
        .clone()
        .expect("set CODEOX_BENCH_BIN to the release executable");

    let before = BinaryIdentity::read(&bin).unwrap_or_else(|error| panic!("{error}"));
    if before.looks_debug {
        println!(
            "WARNING: {} looks like a debug build; official numbers need the release binary.",
            bin.display()
        );
    }

    let fixtures = Fixtures::load(&cfg.fixtures_dir, cfg.client)
        .unwrap_or_else(|error| panic!("could not load benchmark fixtures: {error}"));
    let mock = MockServer::start(Arc::new(MockData {
        relay_key: RELAY_UPSTREAM_KEY.to_owned(),
        direct_key: DIRECT_KEY.to_owned(),
        responses: fixtures.responses_events.clone(),
        chat: fixtures.chat_events.clone(),
        anthropic: fixtures.anthropic_events.clone(),
        hold: Duration::from_millis(cfg.stream_hold_ms),
    }));
    let temp = TempDir::new(cfg.keep);
    let fake =
        FakeLauncher::install(temp.path(), cfg.client).unwrap_or_else(|error| panic!("{error}"));

    print_header(&cfg, &before, &fixtures, &mock);

    let mut verdicts: Vec<Verdict> = Vec::new();
    let mut hard_error: Option<String> = None;
    let mut ran_any = false;

    for protocol in &cfg.protocols {
        println!();
        println!("== protocol {} ==", protocol.label());
        match run_protocol(&cfg, &fixtures, &mock, temp.path(), &fake, &bin, *protocol) {
            Ok(Some(report)) => {
                ran_any = true;
                print_protocol_report(&report);
                verdicts.extend(report.verdicts(&cfg.targets));
            }
            Ok(None) => {
                hard_error = Some(format!(
                    "{} is unavailable in the benchmark executable",
                    protocol.label()
                ));
            }
            Err(error) => {
                println!("FAIL: protocol {}: {error}", protocol.label());
                hard_error = Some(format!("{} failed: {error}", protocol.label()));
            }
        }
    }

    let after = BinaryIdentity::read(&bin).unwrap_or_else(|error| panic!("{error}"));
    if ran_any && let (Some(left), Some(right)) = (&before.sha256, &after.sha256) {
        if left != right {
            println!(
                "WARNING: {} changed during the run; rerun against a stable copy of the release binary.",
                bin.display()
            );
        }
        verdicts.push(Verdict::new(
            "binary",
            "unchanged-during-run",
            if left == right { 0.0 } else { 1.0 },
            0.0,
            "flag",
            left == right,
        ));
    }

    println!();
    println!("== targets ==");
    for verdict in &verdicts {
        println!(
            "  {:<34} {:>12.3} {:<5} target <= {:>8.3}  {}",
            verdict.label(),
            verdict.observed,
            verdict.unit,
            verdict.target,
            if verdict.pass { "PASS" } else { "MISS" }
        );
    }
    let misses = verdicts.iter().filter(|verdict| !verdict.pass).count();
    println!();
    if let Some(error) = hard_error {
        panic!("benchmark did not complete: {error}");
    }
    if misses == 0 && ran_any {
        println!("RESULT: PASS ({} checks)", verdicts.len());
    } else if cfg.allow_miss {
        println!("RESULT: {misses} target miss(es), allowed by CODEOX_BENCH_ALLOW_MISS=1");
    } else {
        panic!(
            "RESULT: {misses} target miss(es); rerun with CODEOX_BENCH_ALLOW_MISS=1 to report without failing"
        );
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Protocol {
    Responses,
    ChatCompletions,
    AnthropicMessages,
}

impl Protocol {
    fn all() -> [Protocol; 3] {
        [
            Protocol::Responses,
            Protocol::ChatCompletions,
            Protocol::AnthropicMessages,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Protocol::Responses => "responses",
            Protocol::ChatCompletions => "chat_completions",
            Protocol::AnthropicMessages => "anthropic_messages",
        }
    }

    fn wire_api(self) -> &'static str {
        match self {
            Protocol::Responses => "responses",
            Protocol::ChatCompletions => "chat_completions",
            Protocol::AnthropicMessages => "anthropic_messages",
        }
    }

    fn upstream_path(self) -> &'static str {
        match self {
            Protocol::Responses => "/v1/responses",
            Protocol::ChatCompletions => "/v1/chat/completions",
            Protocol::AnthropicMessages => "/v1/messages",
        }
    }

    fn direct_body(self, fixtures: &Fixtures) -> &[u8] {
        match self {
            Protocol::Responses => &fixtures.responses_direct,
            Protocol::ChatCompletions => &fixtures.chat_upstream,
            Protocol::AnthropicMessages => &fixtures.anthropic_upstream,
        }
    }

    fn events(self, fixtures: &Fixtures) -> &[Vec<u8>] {
        match self {
            Protocol::Responses => &fixtures.responses_events,
            Protocol::ChatCompletions => &fixtures.chat_events,
            Protocol::AnthropicMessages => &fixtures.anthropic_events,
        }
    }

    fn parse(raw: &str) -> Option<Protocol> {
        match raw.trim() {
            "responses" => Some(Protocol::Responses),
            "chat" | "chat_completions" => Some(Protocol::ChatCompletions),
            "anthropic" | "anthropic_messages" | "messages" => Some(Protocol::AnthropicMessages),
            _ => None,
        }
    }
}

/// Which stock client shape posts to the relay.
///
/// Only this harness's loopback HTTP client is timed. The stock launcher is a
/// fake that keeps the relay process alive and reports the relay's loopback
/// origin and ephemeral bearer; it never performs a model request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BenchClient {
    Codex,
    Claude,
}

impl BenchClient {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// Exact word `ox` consumes to choose this client launch mode.
    fn launch_word(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    fn inbound_path(self) -> &'static str {
        match self {
            Self::Codex => "/v1/responses",
            Self::Claude => "/v1/messages",
        }
    }

    fn inbound_body(self, fixtures: &Fixtures) -> &[u8] {
        match self {
            Self::Codex => &fixtures.responses_inbound,
            Self::Claude => fixtures
                .claude_inbound
                .as_deref()
                .expect("the Claude inbound fixture is loaded for OX_BENCH_CLIENT=claude"),
        }
    }
}

struct Targets {
    idle_rss_mib: f64,
    idle_cpu_pct: f64,
    added_p95_override_ms: Option<f64>,
    peak_rss_mib: f64,
}

impl Targets {
    fn added_p95_ms(&self, protocol: Protocol) -> f64 {
        self.added_p95_override_ms.unwrap_or(match protocol {
            Protocol::Responses => 2.0,
            Protocol::AnthropicMessages | Protocol::ChatCompletions => 5.0,
        })
    }
}

struct BenchConfig {
    bin: Option<PathBuf>,
    client: BenchClient,
    fixtures_dir: PathBuf,
    idle_secs: u64,
    settle_ms: u64,
    samples: usize,
    warmups: usize,
    streams: usize,
    stream_hold_ms: u64,
    keep: bool,
    allow_miss: bool,
    protocols: Vec<Protocol>,
    targets: Targets,
}

impl BenchConfig {
    fn from_env() -> Self {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let bin = match std::env::var_os("CODEOX_BENCH_BIN") {
            Some(value) if !value.is_empty() => {
                let path = PathBuf::from(value);
                let path = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| manifest.clone())
                        .join(path)
                };
                if !path.is_file() {
                    panic!("CODEOX_BENCH_BIN={} is not a file", path.display());
                }
                Some(path)
            }
            _ => None,
        };
        let client = match std::env::var(CLIENT_ENV) {
            Ok(raw) if !raw.trim().is_empty() => BenchClient::parse(&raw).unwrap_or_else(|| {
                panic!("unknown {CLIENT_ENV}={raw:?}; expected \"codex\" or \"claude\"")
            }),
            _ => BenchClient::Codex,
        };
        let fixtures_dir = std::env::var_os("CODEOX_BENCH_FIXTURES")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| manifest.join("tests/fixtures/benchmark"));
        let protocols = match std::env::var("CODEOX_BENCH_PROTOCOLS") {
            Ok(raw) if !raw.trim().is_empty() => raw
                .split(',')
                .map(|name| {
                    Protocol::parse(name)
                        .unwrap_or_else(|| panic!("unknown CODEOX_BENCH_PROTOCOLS entry {name:?}"))
                })
                .collect(),
            _ => Protocol::all().to_vec(),
        };
        Self {
            bin,
            client,
            fixtures_dir,
            idle_secs: env_u64("CODEOX_BENCH_IDLE_SECS", 60).max(1),
            settle_ms: env_u64("CODEOX_BENCH_SETTLE_MS", 2_000),
            samples: env_u64("CODEOX_BENCH_SAMPLES", 40).max(1) as usize,
            warmups: env_u64("CODEOX_BENCH_WARMUPS", 10) as usize,
            streams: env_u64("CODEOX_BENCH_STREAMS", 8).max(1) as usize,
            stream_hold_ms: env_u64("CODEOX_BENCH_STREAM_HOLD_MS", 1_500),
            keep: env_flag("CODEOX_BENCH_KEEP"),
            allow_miss: env_flag("CODEOX_BENCH_ALLOW_MISS"),
            protocols,
            targets: Targets {
                idle_rss_mib: env_f64("CODEOX_BENCH_MAX_IDLE_RSS_MIB").unwrap_or(15.0),
                idle_cpu_pct: env_f64("CODEOX_BENCH_MAX_IDLE_CPU_PCT").unwrap_or(0.1),
                added_p95_override_ms: env_f64("CODEOX_BENCH_MAX_ADDED_P95_MS"),
                peak_rss_mib: env_f64("CODEOX_BENCH_MAX_PEAK_RSS_MIB").unwrap_or(32.0),
            },
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().map(|value| {
        let parsed: f64 = value
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("invalid {name}"));
        assert!(parsed.is_finite() && parsed >= 0.0, "invalid {name}");
        parsed
    })
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct Fixtures {
    files: Vec<(String, u64)>,
    responses_inbound: Vec<u8>,
    /// Present only for `OX_BENCH_CLIENT=claude`; the default Codex run loads
    /// exactly the same fixture set it always has.
    claude_inbound: Option<Vec<u8>>,
    responses_direct: Vec<u8>,
    chat_upstream: Vec<u8>,
    anthropic_upstream: Vec<u8>,
    responses_events: Vec<Vec<u8>>,
    chat_events: Vec<Vec<u8>>,
    anthropic_events: Vec<Vec<u8>>,
}

impl Fixtures {
    fn load(dir: &Path, client: BenchClient) -> Result<Self, String> {
        let mut files = Vec::new();
        let mut read = |relative: &str| -> Result<Vec<u8>, String> {
            let path = dir.join(relative);
            let bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            files.push((relative.to_owned(), bytes.len() as u64));
            Ok(bytes)
        };
        let responses_inbound = read("responses/inbound.json")?;
        let responses_direct = {
            let text = String::from_utf8(responses_inbound.clone())
                .map_err(|_| "responses/inbound.json is not UTF-8".to_owned())?;
            text.replace(&format!("\"{ALIAS}\""), &format!("\"{MODEL}\""))
                .into_bytes()
        };
        let chat_upstream = read("chat_completions/upstream.json")?;
        let anthropic_upstream = read("anthropic_messages/upstream.json")?;
        let responses_events = load_events(&mut read, "responses/stream.sse")?;
        let chat_events = load_events(&mut read, "chat_completions/stream.sse")?;
        let anthropic_events = load_events(&mut read, "anthropic_messages/stream.sse")?;
        let claude_inbound = match client {
            BenchClient::Codex => None,
            BenchClient::Claude => Some(read("claude_messages/inbound.json")?),
        };
        Ok(Self {
            files,
            responses_inbound,
            claude_inbound,
            responses_direct,
            chat_upstream,
            anthropic_upstream,
            responses_events,
            chat_events,
            anthropic_events,
        })
    }
}

fn load_events(
    read: &mut impl FnMut(&str) -> Result<Vec<u8>, String>,
    relative: &str,
) -> Result<Vec<Vec<u8>>, String> {
    let raw = read(relative)?;
    let text = String::from_utf8(raw).map_err(|_| format!("{relative} is not UTF-8"))?;
    let normalized = text.replace("\r\n", "\n");
    let mut events = Vec::new();
    for block in normalized.split("\n\n") {
        let block = block.trim_matches('\n');
        if block.trim().is_empty() {
            continue;
        }
        let mut bytes = block.as_bytes().to_vec();
        bytes.extend_from_slice(b"\n\n");
        events.push(bytes);
    }
    if events.is_empty() {
        return Err(format!("{relative} contains no SSE events"));
    }
    Ok(events)
}

// ---------------------------------------------------------------------------
// Benchmark body
// ---------------------------------------------------------------------------

struct ProtocolReport {
    protocol: Protocol,
    client: BenchClient,
    inbound_path: &'static str,
    idle: IdleReport,
    latency: LatencyReport,
    streams: StreamReport,
    upstream_hits: usize,
    relay_key_hits: usize,
    rejected: u64,
}

impl ProtocolReport {
    fn verdicts(&self, targets: &Targets) -> Vec<Verdict> {
        let added_p95_ms = targets.added_p95_ms(self.protocol);
        vec![
            Verdict::new(
                self.protocol.label(),
                "idle-rss-median",
                self.idle.rss_median_mib,
                targets.idle_rss_mib,
                "MiB",
                self.idle.rss_median_mib <= targets.idle_rss_mib,
            ),
            Verdict::new(
                self.protocol.label(),
                "idle-cpu",
                self.idle.cpu_pct,
                targets.idle_cpu_pct,
                "%core",
                self.idle.cpu_pct <= targets.idle_cpu_pct,
            ),
            Verdict::new(
                self.protocol.label(),
                "added-first-event-p95",
                self.latency.added.p95,
                added_p95_ms,
                "ms",
                self.latency.added.p95 <= added_p95_ms,
            ),
            Verdict::new(
                self.protocol.label(),
                "peak-rss-8-streams",
                self.streams.peak_mib,
                targets.peak_rss_mib,
                "MiB",
                self.streams.peak_mib <= targets.peak_rss_mib,
            ),
            Verdict::new(
                self.protocol.label(),
                "mock-key-rewrite",
                if self.relay_key_hits > 0 { 1.0 } else { 0.0 },
                1.0,
                "flag",
                self.relay_key_hits > 0 && self.rejected == 0,
            ),
        ]
    }
}

struct IdleReport {
    secs: f64,
    rss_first_mib: f64,
    rss_median_mib: f64,
    rss_max_mib: f64,
    cpu_pct: f64,
    samples: usize,
}

struct LatencyReport {
    warmups: usize,
    samples: usize,
    direct: Stats,
    relay: Stats,
    added: Stats,
}

struct StreamReport {
    streams: usize,
    baseline_mib: f64,
    peak_mib: f64,
    peak_active: usize,
    first_event_p95_ms: f64,
}

#[derive(Clone, Copy, Default)]
struct Stats {
    p50: f64,
    p95: f64,
    max: f64,
}

impl Stats {
    fn from(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(|left, right| left.total_cmp(right));
        Self {
            p50: percentile(&sorted, 50.0),
            p95: percentile(&sorted, 95.0),
            max: *sorted.last().unwrap(),
        }
    }
}

fn percentile(sorted: &[f64], percent: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((percent / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn run_protocol(
    cfg: &BenchConfig,
    fixtures: &Fixtures,
    mock: &MockServer,
    temp_root: &Path,
    fake: &FakeLauncher,
    bin: &Path,
    protocol: Protocol,
) -> Result<Option<ProtocolReport>, String> {
    let home = temp_root.join(protocol.label());
    fs::create_dir_all(&home).map_err(|error| format!("{}: {error}", home.display()))?;
    write_relay_config(&home, &mock.base_url(), protocol)?;

    let mut relay = Relay::spawn(bin, &home, fake, cfg.client)?;
    let info = match relay.wait_for_launch(Duration::from_secs(20))? {
        LaunchResult::Ready(info) => info,
        LaunchResult::Unwired(reason) => {
            println!(
                "  codeox rejected the config as not yet wired: {}",
                one_line(&reason)
            );
            return Ok(None);
        }
    };
    let relay_addr = parse_loopback_base(&info.base_url)?;

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last: Option<String>;
    loop {
        match http_get(relay_addr, "/v1/models", &info.token, HTTP_TIMEOUT) {
            Ok((200, body)) => {
                let text = String::from_utf8_lossy(&body);
                if !text.contains(ALIAS) {
                    return Err(format!(
                        "relay /v1/models did not advertise {ALIAS:?}: {}",
                        truncate(&text, 240)
                    ));
                }
                break;
            }
            Ok((status, body)) => {
                last = Some(format!(
                    "HTTP {status}: {}",
                    truncate(&String::from_utf8_lossy(&body), 160)
                ));
            }
            Err(error) => last = Some(error),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "relay did not become ready within 10 s ({}); stderr: {}",
                last.as_deref().unwrap_or("no response"),
                relay.stderr_tail()
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }

    let inbound = cfg.client.inbound_body(fixtures);
    let relay_path = cfg.client.inbound_path();
    let direct_body = protocol.direct_body(fixtures);
    let upstream = protocol.upstream_path();
    let event_blocks = protocol.events(fixtures).len();
    if event_blocks < 2 {
        return Err(format!(
            "{} stream fixture needs at least two events",
            protocol.label()
        ));
    }

    for _ in 0..cfg.warmups {
        post_first_event(mock.addr(), upstream, DIRECT_KEY, direct_body, HTTP_TIMEOUT)
            .map_err(|error| format!("direct warmup failed: {error}"))?;
        post_first_event(relay_addr, relay_path, &info.token, inbound, HTTP_TIMEOUT)
            .map_err(|error| format!("relay warmup failed: {error}"))?;
    }

    let mut direct = Vec::with_capacity(cfg.samples);
    let mut relay_latency = Vec::with_capacity(cfg.samples);
    let mut added = Vec::with_capacity(cfg.samples);
    for index in 0..cfg.samples {
        let (direct_ms, relay_ms) = if index % 2 == 0 {
            let relay_ms =
                post_first_event(relay_addr, relay_path, &info.token, inbound, HTTP_TIMEOUT)
                    .map_err(|error| format!("relay sample {index} failed: {error}"))?;
            let direct_ms =
                post_first_event(mock.addr(), upstream, DIRECT_KEY, direct_body, HTTP_TIMEOUT)
                    .map_err(|error| format!("direct sample {index} failed: {error}"))?;
            (direct_ms, relay_ms)
        } else {
            let direct_ms =
                post_first_event(mock.addr(), upstream, DIRECT_KEY, direct_body, HTTP_TIMEOUT)
                    .map_err(|error| format!("direct sample {index} failed: {error}"))?;
            let relay_ms =
                post_first_event(relay_addr, relay_path, &info.token, inbound, HTTP_TIMEOUT)
                    .map_err(|error| format!("relay sample {index} failed: {error}"))?;
            (direct_ms, relay_ms)
        };
        direct.push(direct_ms);
        relay_latency.push(relay_ms);
        added.push(relay_ms - direct_ms);
    }

    let idle = measure_idle(&mut relay, cfg)?;
    let streams = measure_streams(
        &relay,
        cfg,
        relay_addr,
        &info.token,
        relay_path,
        inbound,
        mock,
    )?;

    let (upstream_hits, relay_key_hits) = {
        let hits = mock.hits.lock().unwrap();
        let upstream_hits = hits
            .iter()
            .filter(|hit| hit.path == upstream && hit.auth_ok && hit.body_len > 0)
            .count();
        let relay_key_hits = hits
            .iter()
            .filter(|hit| {
                hit.path == upstream && hit.used_relay_key && hit.model.as_deref() == Some(MODEL)
            })
            .count();
        (upstream_hits, relay_key_hits)
    };
    let rejected = mock.rejected.load(Ordering::Relaxed);
    let expected_hits = 2 * (cfg.warmups + cfg.samples);
    if upstream_hits < expected_hits {
        return Err(format!(
            "mock saw {upstream_hits} upstream hits, expected at least {expected_hits}"
        ));
    }
    if relay_key_hits == 0 {
        return Err("mock saw no upstream request using the relay-injected vendor key".to_owned());
    }
    if rejected > 0 {
        return Err(format!(
            "mock rejected {rejected} request(s) with wrong credentials"
        ));
    }

    let _ = relay.stderr_tail();

    Ok(Some(ProtocolReport {
        protocol,
        client: cfg.client,
        inbound_path: relay_path,
        idle,
        latency: LatencyReport {
            warmups: cfg.warmups,
            samples: cfg.samples,
            direct: Stats::from(&direct),
            relay: Stats::from(&relay_latency),
            added: Stats::from(&added),
        },
        streams,
        upstream_hits,
        relay_key_hits,
        rejected,
    }))
}

fn measure_idle(relay: &mut Relay, cfg: &BenchConfig) -> Result<IdleReport, String> {
    thread::sleep(Duration::from_millis(cfg.settle_ms));
    let pid = relay.pid();
    let first = rss_kib(pid).ok_or("relay exited before idle measurement")?;
    let cpu_start = cpu_ticks(pid).ok_or("could not read relay CPU before idle window")?;
    let sampler = RssSampler::start(pid, Duration::from_millis(250));
    let started = Instant::now();
    thread::sleep(Duration::from_secs(cfg.idle_secs));
    let elapsed = started.elapsed().as_secs_f64();
    let cpu_end = cpu_ticks(pid).ok_or("could not read relay CPU after idle window")?;
    let rss_samples = sampler.stop();
    if rss_samples.is_empty() {
        return Err("no RSS samples collected during idle window".to_owned());
    }
    let mut sorted = rss_samples.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2] as f64 / 1024.0;
    let max = *sorted.last().unwrap() as f64 / 1024.0;
    let ticks = cpu_end.saturating_sub(cpu_start) as f64;
    let cpu_pct = ticks / clock_ticks_per_sec() as f64 / elapsed * 100.0;
    Ok(IdleReport {
        secs: elapsed,
        rss_first_mib: first as f64 / 1024.0,
        rss_median_mib: median,
        rss_max_mib: max,
        cpu_pct,
        samples: rss_samples.len(),
    })
}

fn measure_streams(
    relay: &Relay,
    cfg: &BenchConfig,
    relay_addr: SocketAddr,
    token: &str,
    relay_path: &str,
    inbound: &[u8],
    mock: &MockServer,
) -> Result<StreamReport, String> {
    thread::sleep(Duration::from_millis(cfg.settle_ms.min(500)));
    let pid = relay.pid();
    let baseline = rss_kib(pid).ok_or("relay exited before stream measurement")? as f64 / 1024.0;
    let sampler = RssSampler::start(pid, Duration::from_millis(5));
    mock.peak_active.store(0, Ordering::SeqCst);
    mock.measure_concurrency.store(true, Ordering::SeqCst);
    let barrier = Arc::new(Barrier::new(cfg.streams + 1));
    let body = Arc::new(inbound.to_vec());
    let mut handles = Vec::with_capacity(cfg.streams);
    for index in 0..cfg.streams {
        let barrier = Arc::clone(&barrier);
        let body = Arc::clone(&body);
        let token = token.to_owned();
        let relay_path = relay_path.to_owned();
        handles.push(thread::spawn(move || {
            barrier.wait();
            post_drain(
                relay_addr,
                &relay_path,
                &token,
                &body,
                HTTP_TIMEOUT,
                Some(index),
            )
        }));
    }
    barrier.wait();
    let mut first_events = Vec::with_capacity(cfg.streams);
    let mut failures = Vec::new();
    for (index, handle) in handles.into_iter().enumerate() {
        match handle.join() {
            Ok(Ok(sample)) => {
                if sample.bytes == 0 {
                    failures.push(format!("stream {index} returned an empty body"));
                }
                first_events.push(sample.first_ms);
            }
            Ok(Err(error)) => failures.push(format!("stream {index}: {error}")),
            Err(_) => failures.push(format!("stream {index} panicked")),
        }
    }
    mock.measure_concurrency.store(false, Ordering::SeqCst);
    let rss_samples = sampler.stop();
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    let peak_active = mock.peak_active.load(Ordering::SeqCst);
    if peak_active != cfg.streams {
        return Err(format!(
            "mock saw {peak_active} concurrent streams, expected {}",
            cfg.streams
        ));
    }
    let peak = rss_samples
        .iter()
        .copied()
        .max()
        .unwrap_or(baseline as u64 * 1024) as f64
        / 1024.0;
    let mut sorted_first = first_events;
    sorted_first.sort_by(|left, right| left.total_cmp(right));
    Ok(StreamReport {
        streams: cfg.streams,
        baseline_mib: baseline,
        peak_mib: peak.max(baseline),
        peak_active,
        first_event_p95_ms: percentile(&sorted_first, 95.0),
    })
}

// ---------------------------------------------------------------------------
// Relay process and fake stock launcher
// ---------------------------------------------------------------------------

/// Minimal stand-in for the stock client binary `ox` launches.
///
/// The launcher exists only to keep the relay alive and to report the
/// loopback origin plus ephemeral bearer that `ox` bound for this launch. It
/// answers the version/catalog probes each client needs and then sleeps; it
/// never talks to a model and is excluded from every measured metric.
struct FakeLauncher {
    bin_dir: PathBuf,
    bundled: Option<PathBuf>,
}

impl FakeLauncher {
    fn install(root: &Path, client: BenchClient) -> Result<Self, String> {
        let bin_dir = root.join("fake-bin");
        fs::create_dir_all(&bin_dir).map_err(|error| format!("{}: {error}", bin_dir.display()))?;
        let (script_text, bundled) = match client {
            BenchClient::Codex => {
                let bundled = root.join("fake-bundled-models.json");
                fs::write(&bundled, br#"{"models":[]}"#)
                    .map_err(|error| format!("{}: {error}", bundled.display()))?;
                (FAKE_CODEX_SCRIPT, Some(bundled))
            }
            // The Claude probe only records the launch environment; `ox claude`
            // sends Claude Code no model catalog request of its own.
            BenchClient::Claude => (FAKE_CLAUDE_SCRIPT, None),
        };
        let script = bin_dir.join(client.launch_word());
        fs::write(&script, script_text)
            .map_err(|error| format!("{}: {error}", script.display()))?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("{}: {error}", script.display()))?;
        Ok(Self { bin_dir, bundled })
    }

    fn path_with_fake(&self) -> String {
        let original = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned());
        format!("{}:{}", self.bin_dir.display(), original)
    }
}

const FAKE_CODEX_SCRIPT: &str = r#"#!/bin/sh
set -eu
if [ "${1:-}" = "debug" ] && [ "${2:-}" = "models" ]; then
  cat "$FAKE_CODEX_BUNDLED"
  exit 0
fi
case "${1:-}" in
  --version|-V)
    printf 'codex-benchmark-fake 0.0.0\n'
    exit 0
    ;;
esac
tmp="$FAKE_LAUNCH.tmp.$$"
{
  printf 'token=%s\n' "${CODEOX_TOKEN:-}"
  printf 'home=%s\n' "${CODEX_HOME:-}"
  for arg in "$@"; do
    printf 'arg=%s\n' "$arg"
  done
} > "$tmp"
mv "$tmp" "$FAKE_LAUNCH"
printf '%s\n' "$$" > "$FAKE_PID"
trap 'exit 0' TERM INT HUP
while :; do
  sleep 1
done
"#;

/// Claude Code launch probe. `ox claude` hands the child the loopback origin
/// in `ANTHROPIC_BASE_URL` and the launch-scoped bearer in
/// `ANTHROPIC_AUTH_TOKEN`; those values are recorded verbatim so the harness
/// can drive the relay without a real stock client. The probe never sends a
/// model request.
const FAKE_CLAUDE_SCRIPT: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version|-V)
    printf 'claude-benchmark-fake 0.0.0\n'
    exit 0
    ;;
esac
tmp="$FAKE_LAUNCH.tmp.$$"
{
  printf 'base_url=%s\n' "${ANTHROPIC_BASE_URL:-}"
  printf 'token=%s\n' "${ANTHROPIC_AUTH_TOKEN:-}"
  printf 'model=%s\n' "${ANTHROPIC_MODEL:-}"
  for arg in "$@"; do
    printf 'arg=%s\n' "$arg"
  done
} > "$tmp"
mv "$tmp" "$FAKE_LAUNCH"
printf '%s\n' "$$" > "$FAKE_PID"
trap 'exit 0' TERM INT HUP
while :; do
  sleep 1
done
"#;

struct LaunchInfo {
    base_url: String,
    token: String,
}

enum LaunchResult {
    Ready(LaunchInfo),
    Unwired(String),
}

struct Relay {
    child: Child,
    pid: u32,
    launch_file: PathBuf,
    pid_file: PathBuf,
    stderr_file: PathBuf,
    cleaned: bool,
}

impl Relay {
    fn spawn(
        bin: &Path,
        home: &Path,
        fake: &FakeLauncher,
        client: BenchClient,
    ) -> Result<Self, String> {
        let launch_file = home.join("fake-launch.txt");
        let pid_file = home.join("fake-client.pid");
        let stderr_file = home.join("relay.stderr");
        let stdout_file = home.join("relay.stdout");
        let stdout = File::create(&stdout_file)
            .map_err(|error| format!("{}: {error}", stdout_file.display()))?;
        let stderr = File::create(&stderr_file)
            .map_err(|error| format!("{}: {error}", stderr_file.display()))?;
        let mut command = Command::new(bin);
        command
            .arg(client.launch_word())
            .env_clear()
            .env("PATH", fake.path_with_fake())
            .env("HOME", home)
            .env("CODEX_HOME", home)
            .env("TMPDIR", home)
            .env("LANG", "C.UTF-8")
            .env(KEY_ENV, RELAY_UPSTREAM_KEY)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("FAKE_LAUNCH", &launch_file)
            .env("FAKE_PID", &pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .process_group(0);
        if let Some(bundled) = &fake.bundled {
            command.env("FAKE_CODEX_BUNDLED", bundled);
        }
        if let Some(libraries) =
            std::env::var_os("LD_LIBRARY_PATH").filter(|value| !value.is_empty())
        {
            command.env("LD_LIBRARY_PATH", libraries);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", bin.display()))?;
        let pid = child.id();
        Ok(Self {
            child,
            pid,
            launch_file,
            pid_file,
            stderr_file,
            cleaned: false,
        })
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    fn wait_for_launch(&mut self, timeout: Duration) -> Result<LaunchResult, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(info) = read_launch_info(&self.launch_file) {
                return Ok(LaunchResult::Ready(info));
            }
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| format!("could not poll relay: {error}"))?
            {
                let stderr = self.stderr_tail();
                if is_unwired_message(&stderr) {
                    return Ok(LaunchResult::Unwired(stderr));
                }
                return Err(format!(
                    "codeox exited before spawning the Codex child ({status}); stderr: {}",
                    one_line(&stderr)
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for the fake launcher file; stderr: {}",
                    one_line(&self.stderr_tail())
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stderr_tail(&self) -> String {
        let text = fs::read_to_string(&self.stderr_file).unwrap_or_default();
        let tail: String = text
            .lines()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        truncate(&tail, 2_000)
    }

    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        signal_group(self.pid, "TERM");
        thread::sleep(Duration::from_millis(100));
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(pid) = read_pid(&self.pid_file) {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        signal_group(self.pid, "KILL");
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn read_launch_info(path: &Path) -> Option<LaunchInfo> {
    let text = fs::read_to_string(path).ok()?;
    let mut token = None;
    let mut base_url = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("token=") {
            if !value.is_empty() {
                token = Some(value.to_owned());
            }
        } else if let Some(value) = line.strip_prefix("arg=model_providers.codeox.base_url=") {
            if let Ok(parsed) = serde_json::from_str::<String>(value) {
                base_url = Some(parsed);
            }
        } else if let Some(value) = line.strip_prefix("base_url=") {
            // `ox claude` puts the loopback origin directly in
            // ANTHROPIC_BASE_URL (no `/v1` suffix).
            if !value.is_empty() {
                base_url = Some(value.to_owned());
            }
        }
    }
    Some(LaunchInfo {
        base_url: base_url?,
        token: token?,
    })
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn is_unwired_message(stderr: &str) -> bool {
    stderr.contains("reserved for a future codeox adapter") || stderr.contains("unknown wire_api")
}

fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn write_relay_config(home: &Path, mock_base: &str, protocol: Protocol) -> Result<(), String> {
    let text = format!(
        r#"[providers.mock]
name = "Benchmark Mock"
base_url = "{mock_base}"
env_key = "{KEY_ENV}"
wire_api = "{}"
discovery = false

[[providers.mock.models]]
id = "{MODEL}"
name = "Benchmark Mock Model"
context_window = 128000
input_modalities = ["text"]
reasoning_efforts = ["low", "medium", "high"]
"#,
        protocol.wire_api()
    );
    let path = home.join("codeox.toml");
    fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))
}

// ---------------------------------------------------------------------------
// Loopback mock vendor
// ---------------------------------------------------------------------------

struct MockData {
    relay_key: String,
    direct_key: String,
    responses: Vec<Vec<u8>>,
    chat: Vec<Vec<u8>>,
    anthropic: Vec<Vec<u8>>,
    hold: Duration,
}

#[derive(Clone)]
struct Hit {
    path: String,
    model: Option<String>,
    auth_ok: bool,
    used_relay_key: bool,
    body_len: usize,
}

struct MockServer {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<Hit>>>,
    rejected: Arc<AtomicU64>,
    peak_active: Arc<AtomicUsize>,
    measure_concurrency: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl MockServer {
    fn start(data: Arc<MockData>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback mock");
        let addr = listener.local_addr().expect("mock addr");
        listener.set_nonblocking(true).expect("nonblocking mock");
        let hits = Arc::new(Mutex::new(Vec::new()));
        let rejected = Arc::new(AtomicU64::new(0));
        let peak_active = Arc::new(AtomicUsize::new(0));
        let measure_concurrency = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_hits = Arc::clone(&hits);
        let thread_rejected = Arc::clone(&rejected);
        let thread_peak = Arc::clone(&peak_active);
        let thread_measure = Arc::clone(&measure_concurrency);
        let join = thread::spawn(move || {
            let state = Arc::new(MockState {
                hits: thread_hits,
                rejected: thread_rejected,
                peak_active: thread_peak,
                measure_concurrency: thread_measure,
                active: AtomicUsize::new(0),
            });
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let data = Arc::clone(&data);
                        let state = Arc::clone(&state);
                        thread::spawn(move || {
                            handle_mock(socket, data, state);
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            hits,
            rejected,
            peak_active,
            measure_concurrency,
            stop,
            join: Some(join),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct MockState {
    hits: Arc<Mutex<Vec<Hit>>>,
    rejected: Arc<AtomicU64>,
    peak_active: Arc<AtomicUsize>,
    measure_concurrency: Arc<AtomicBool>,
    active: AtomicUsize,
}

fn handle_mock(mut socket: TcpStream, data: Arc<MockData>, state: Arc<MockState>) {
    let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(10)));
    let request = match read_http_request(&mut socket) {
        Ok(request) => request,
        Err(_) => return,
    };
    let path = request.path.split('?').next().unwrap_or("").to_owned();
    let auth_header = request
        .headers
        .get("authorization")
        .map(String::as_str)
        .unwrap_or("");
    let api_key = request
        .headers
        .get("x-api-key")
        .map(String::as_str)
        .unwrap_or("");
    let relay_auth = format!("Bearer {}", data.relay_key);
    let direct_auth = format!("Bearer {}", data.direct_key);
    let used_relay_key = auth_header == relay_auth || api_key == data.relay_key;
    let used_direct_key = auth_header == direct_auth || api_key == data.direct_key;
    let auth_ok = used_relay_key || used_direct_key;
    let model = serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    state.hits.lock().unwrap().push(Hit {
        path: path.clone(),
        model,
        auth_ok,
        used_relay_key,
        body_len: request.body.len(),
    });
    if !auth_ok {
        state.rejected.fetch_add(1, Ordering::Relaxed);
        let _ = write_json(
            &mut socket,
            401,
            r#"{"error":{"message":"bad benchmark key"}}"#,
        );
        return;
    }
    if request.method == "GET" && path.ends_with("/models") {
        let body = format!(
            r#"{{"object":"list","data":[{{"id":"{MODEL}","object":"model","created":1700000000,"owned_by":"benchmark"}}]}}"#
        );
        let _ = write_json(&mut socket, 200, &body);
        return;
    }
    let events = if path.ends_with("/responses") || path.ends_with("/responses/compact") {
        &data.responses
    } else if path.ends_with("/chat/completions") {
        &data.chat
    } else if path.ends_with("/messages") {
        &data.anthropic
    } else {
        let _ = write_json(&mut socket, 404, r#"{"error":{"message":"no route"}}"#);
        return;
    };
    if state.measure_concurrency.load(Ordering::SeqCst) {
        let now_active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.peak_active.fetch_max(now_active, Ordering::SeqCst);
        let _ = write_sse(&mut socket, events, data.hold);
        state.active.fetch_sub(1, Ordering::SeqCst);
    } else {
        let _ = write_sse(&mut socket, events, Duration::ZERO);
    }
}

struct MockRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request(socket: &mut TcpStream) -> Result<MockRequest, String> {
    let mut buffer = Vec::new();
    let head_end = loop {
        if let Some(index) = find(&buffer, b"\r\n\r\n") {
            break index;
        }
        if !read_more(socket, &mut buffer, MAX_HEAD_BYTES)? {
            return Err("connection closed before request headers".to_owned());
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let mut body = buffer[head_end + 4..].to_vec();
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        body = read_chunked_body(socket, body)?;
    } else if let Some(length) = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
    {
        if length > 16 * 1024 * 1024 {
            return Err("mock request body too large".to_owned());
        }
        while body.len() < length {
            if !read_more(socket, &mut body, 16 * 1024 * 1024)? {
                return Err("connection closed before request body".to_owned());
            }
        }
        body.truncate(length);
    }
    Ok(MockRequest {
        method,
        path,
        headers,
        body,
    })
}

fn read_chunked_body(socket: &mut TcpStream, mut buffer: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut decoded = Vec::new();
    loop {
        let line_end = loop {
            if let Some(index) = find(&buffer, b"\r\n") {
                break index;
            }
            if !read_more(socket, &mut buffer, 16 * 1024 * 1024)? {
                return Err("connection closed in chunk header".to_owned());
            }
        };
        let line = String::from_utf8_lossy(&buffer[..line_end]).to_string();
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("bad chunk size {size_text:?}"))?;
        buffer.drain(..line_end + 2);
        if size == 0 {
            return Ok(decoded);
        }
        while buffer.len() < size + 2 {
            if !read_more(socket, &mut buffer, 16 * 1024 * 1024)? {
                return Err("connection closed in chunk body".to_owned());
            }
        }
        decoded.extend_from_slice(&buffer[..size]);
        buffer.drain(..size + 2);
        if decoded.len() > 16 * 1024 * 1024 {
            return Err("chunked mock request too large".to_owned());
        }
    }
}

fn write_json(socket: &mut TcpStream, status: u16, body: &str) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes())?;
    socket.write_all(body.as_bytes())?;
    socket.flush()?;
    socket.shutdown(Shutdown::Write).ok();
    Ok(())
}

fn write_sse(socket: &mut TcpStream, events: &[Vec<u8>], hold: Duration) -> io::Result<()> {
    socket.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    )?;
    socket.flush()?;
    let count = events.len();
    let delay = if count > 1 {
        hold / (count as u32 - 1)
    } else {
        Duration::ZERO
    };
    for (index, event) in events.iter().enumerate() {
        socket.write_all(event)?;
        socket.flush()?;
        if index + 1 < count && !delay.is_zero() {
            thread::sleep(delay);
        }
    }
    socket.shutdown(Shutdown::Write).ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal blocking HTTP client
// ---------------------------------------------------------------------------

fn http_connect(addr: SocketAddr, timeout: Duration) -> Result<TcpStream, String> {
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|error| format!("connect {addr}: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("write timeout: {error}"))?;
    stream.set_nodelay(true).ok();
    Ok(stream)
}

fn http_get(
    addr: SocketAddr,
    path: &str,
    bearer: &str,
    timeout: Duration,
) -> Result<(u16, Vec<u8>), String> {
    let mut stream = http_connect(addr, timeout)?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {bearer}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write GET: {error}"))?;
    let (status, rest) = read_head(&mut stream)?;
    let body = read_to_end(&mut stream, rest, 2 * 1024 * 1024)?;
    Ok((status, body))
}

fn post_first_event(
    addr: SocketAddr,
    path: &str,
    bearer: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<f64, String> {
    let started = Instant::now();
    let mut stream = http_connect(addr, timeout)?;
    write_post(&mut stream, addr, path, bearer, body)?;
    let (status, rest) = read_head(&mut stream)?;
    if status != 200 {
        let body = read_to_end(&mut stream, rest, 64 * 1024).unwrap_or_default();
        return Err(format!(
            "POST {path} returned HTTP {status}: {}",
            truncate(&String::from_utf8_lossy(&body), 240)
        ));
    }
    read_to_first_event(&mut stream, rest, 1024 * 1024)?;
    Ok(started.elapsed().as_secs_f64() * 1_000.0)
}

struct StreamSample {
    first_ms: f64,
    bytes: usize,
}

fn post_drain(
    addr: SocketAddr,
    path: &str,
    bearer: &str,
    body: &[u8],
    timeout: Duration,
    _index: Option<usize>,
) -> Result<StreamSample, String> {
    let started = Instant::now();
    let mut stream = http_connect(addr, timeout)?;
    write_post(&mut stream, addr, path, bearer, body)?;
    let (status, rest) = read_head(&mut stream)?;
    if status != 200 {
        let body = read_to_end(&mut stream, rest, 64 * 1024).unwrap_or_default();
        return Err(format!(
            "POST {path} returned HTTP {status}: {}",
            truncate(&String::from_utf8_lossy(&body), 240)
        ));
    }
    let mut buffer = rest;
    let mut first_ms = None;
    loop {
        if first_ms.is_none() && find_event_end(&buffer) {
            first_ms = Some(started.elapsed().as_secs_f64() * 1_000.0);
        }
        if buffer.len() > 16 * 1024 * 1024 {
            return Err("stream exceeded 16 MiB".to_owned());
        }
        if !read_more(&mut stream, &mut buffer, 16 * 1024 * 1024)? {
            break;
        }
    }
    let first_ms = first_ms.ok_or("stream ended without an SSE event")?;
    Ok(StreamSample {
        first_ms,
        bytes: buffer.len(),
    })
}

fn write_post(
    stream: &mut TcpStream,
    addr: SocketAddr,
    path: &str,
    bearer: &str,
    body: &[u8],
) -> Result<(), String> {
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {bearer}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .map_err(|error| format!("write headers: {error}"))?;
    stream
        .write_all(body)
        .map_err(|error| format!("write body: {error}"))?;
    stream.flush().map_err(|error| format!("flush: {error}"))
}

fn read_head(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), String> {
    let mut buffer = Vec::new();
    let head_end = loop {
        if let Some(index) = find(&buffer, b"\r\n\r\n") {
            break index;
        }
        if !read_more(stream, &mut buffer, MAX_HEAD_BYTES)? {
            return Err("connection closed before response headers".to_owned());
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]);
    let status_line = head.lines().next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line {status_line:?}"))?;
    Ok((status, buffer[head_end + 4..].to_vec()))
}

fn read_to_first_event(
    stream: &mut TcpStream,
    mut buffer: Vec<u8>,
    limit: usize,
) -> Result<Vec<u8>, String> {
    loop {
        if find_event_end(&buffer) {
            return Ok(buffer);
        }
        if buffer.len() > limit {
            return Err("first SSE event exceeded limit".to_owned());
        }
        if !read_more(stream, &mut buffer, limit)? {
            return Err("connection closed before the first SSE event".to_owned());
        }
    }
}

fn read_to_end(
    stream: &mut TcpStream,
    mut buffer: Vec<u8>,
    limit: usize,
) -> Result<Vec<u8>, String> {
    loop {
        if buffer.len() > limit {
            return Err("response exceeded limit".to_owned());
        }
        if !read_more(stream, &mut buffer, limit)? {
            return Ok(buffer);
        }
    }
}

fn read_more(stream: &mut TcpStream, buffer: &mut Vec<u8>, limit: usize) -> Result<bool, String> {
    let mut chunk = [0u8; 8192];
    match stream.read(&mut chunk) {
        Ok(0) => Ok(false),
        Ok(read) => {
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() > limit {
                return Err(format!("response exceeded {limit} bytes"));
            }
            Ok(true)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err("timed out waiting for loopback HTTP data".to_owned())
        }
        Err(error) => Err(format!("read failed: {error}")),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn find_event_end(buffer: &[u8]) -> bool {
    find(buffer, b"\n\n").is_some() || find(buffer, b"\r\n\r\n").is_some()
}

fn parse_loopback_base(base_url: &str) -> Result<SocketAddr, String> {
    let rest = base_url
        .strip_prefix("http://")
        .ok_or_else(|| format!("expected an http loopback base_url, got {base_url:?}"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let authority = authority
        .strip_prefix("localhost:")
        .map(|port| format!("127.0.0.1:{port}"))
        .unwrap_or_else(|| authority.to_owned());
    let addr = authority
        .parse::<SocketAddr>()
        .map_err(|error| format!("bad loopback authority {authority:?}: {error}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!("refusing non-loopback relay address {addr}"));
    }
    Ok(addr)
}

// ---------------------------------------------------------------------------
// Process metrics and host/binary identity
// ---------------------------------------------------------------------------

fn rss_kib(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields = stat[close + 1..].split_whitespace().collect::<Vec<_>>();
    let user = fields.get(11)?.parse::<u64>().ok()?;
    let system = fields.get(12)?.parse::<u64>().ok()?;
    Some(user + system)
}

fn clock_ticks_per_sec() -> u64 {
    Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(100)
}

struct RssSampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<u64>>>,
}

impl RssSampler {
    fn start(pid: u32, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut samples = Vec::new();
            while !thread_stop.load(Ordering::Relaxed) {
                if let Some(value) = rss_kib(pid) {
                    samples.push(value);
                }
                thread::sleep(interval);
            }
            if let Some(value) = rss_kib(pid) {
                samples.push(value);
            }
            samples
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop(mut self) -> Vec<u64> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    }
}

struct TempDir {
    path: PathBuf,
    keep: bool,
}

impl TempDir {
    fn new(keep: bool) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("codeox-bench-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("create benchmark temp dir");
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
        Self { path, keep }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.keep {
            println!("kept benchmark directory: {}", self.path.display());
        } else {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct BinaryIdentity {
    path: PathBuf,
    size: u64,
    mtime_unix: Option<u64>,
    sha256: Option<String>,
    file_summary: Option<String>,
    looks_debug: bool,
}

impl BinaryIdentity {
    fn read(path: &Path) -> Result<Self, String> {
        let metadata =
            fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        let mut magic = [0u8; 4];
        File::open(path)
            .and_then(|mut file| file.read_exact(&mut magic))
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if &magic != b"\x7fELF" {
            return Err(format!("{} is not an ELF executable", path.display()));
        }
        let mtime_unix = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        Ok(Self {
            path: path.to_path_buf(),
            size: metadata.len(),
            mtime_unix,
            sha256: sha256(path),
            file_summary: file_summary(path),
            looks_debug: path
                .components()
                .any(|component| component.as_os_str() == "debug"),
        })
    }
}

fn sha256(path: &Path) -> Option<String> {
    for (program, prefix) in [("sha256sum", Vec::new()), ("shasum", vec!["-a", "256"])] {
        let Ok(output) = Command::new(program).args(prefix).arg(path).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(text) = String::from_utf8(output.stdout) else {
            continue;
        };
        if let Some(first) = text.split_whitespace().next()
            && first.len() == 64 && first.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(first.to_ascii_lowercase());
            }
    }
    None
}

fn file_summary(path: &Path) -> Option<String> {
    let output = Command::new("file").arg("-b").arg(path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(truncate(text.trim(), 200))
}

fn machine_summary() -> Vec<String> {
    let uname = command_text("uname", &["-srm"]);
    let cpus = std::thread::available_parallelism()
        .map(|value| value.get().to_string())
        .unwrap_or_else(|_| "?".to_owned());
    let cpu_model = fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("model name") || line.starts_with("Hardware"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_owned())
        })
        .or_else(lscpu_model)
        .unwrap_or_else(|| "unknown".to_owned());
    let os = fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("PRETTY_NAME="))
                .map(|line| {
                    line.trim_start_matches("PRETTY_NAME=")
                        .trim_matches('"')
                        .to_owned()
                })
        })
        .unwrap_or_else(|| "unknown".to_owned());
    vec![
        format!("kernel: {}", one_line(&uname)),
        format!("os: {os}"),
        format!("cpus: {cpus}"),
        format!("cpu: {cpu_model}"),
        format!(
            "harness: {} (relay under test is the supplied executable)",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        ),
    ]
}

fn lscpu_model() -> Option<String> {
    let output = Command::new("lscpu").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let value = |key: &str| {
        text.lines()
            .find(|line| line.trim_start().starts_with(key))
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    match (value("Vendor ID"), value("Model name")) {
        (Some(vendor), Some(model)) => Some(format!("{vendor} {model}")),
        (None, Some(model)) => Some(model),
        (Some(vendor), None) => Some(vendor),
        _ => None,
    }
}

fn command_text(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|| "unavailable".to_owned())
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

struct Verdict {
    protocol: &'static str,
    metric: &'static str,
    observed: f64,
    target: f64,
    unit: &'static str,
    pass: bool,
}

impl Verdict {
    fn new(
        protocol: &'static str,
        metric: &'static str,
        observed: f64,
        target: f64,
        unit: &'static str,
        pass: bool,
    ) -> Self {
        Self {
            protocol,
            metric,
            observed,
            target,
            unit,
            pass,
        }
    }

    fn label(&self) -> String {
        format!("{}/{}", self.protocol, self.metric)
    }
}

fn print_header(
    cfg: &BenchConfig,
    identity: &BinaryIdentity,
    fixtures: &Fixtures,
    mock: &MockServer,
) {
    println!("==== codeox relay benchmark ====");
    println!("binary: {}", identity.path.display());
    println!(
        "binary size: {} bytes, mtime_unix={:?}",
        identity.size, identity.mtime_unix
    );
    println!(
        "binary sha256: {}",
        identity.sha256.as_deref().unwrap_or("unavailable")
    );
    if let Some(summary) = &identity.file_summary {
        println!("binary file: {summary}");
    }
    for line in machine_summary() {
        println!("{line}");
    }
    println!(
        "client: {} (inbound {}); timed traffic is generated by this harness, not the stock launcher",
        cfg.client.label(),
        cfg.client.inbound_path()
    );
    println!(
        "settings: idle={}s samples={} warmups={} streams={} hold={}ms",
        cfg.idle_secs, cfg.samples, cfg.warmups, cfg.streams, cfg.stream_hold_ms
    );
    println!(
        "targets: idle-rss<={} MiB, idle-cpu<={} %core, added-p95<={} ms Responses or {} ms translated, peak-rss<={} MiB",
        cfg.targets.idle_rss_mib,
        cfg.targets.idle_cpu_pct,
        cfg.targets.added_p95_ms(Protocol::Responses),
        cfg.targets.added_p95_ms(Protocol::ChatCompletions),
        cfg.targets.peak_rss_mib
    );
    println!("mock vendor: {} (loopback only)", mock.base_url());
    println!(
        "metrics scope: relay /proc/<pid> only; fake launcher child and in-process mock excluded"
    );
    println!("fixtures:");
    for (name, size) in &fixtures.files {
        println!("  {name:<40} {size:>7} B");
    }
}

fn print_protocol_report(report: &ProtocolReport) {
    let protocol = report.protocol;
    println!(
        "protocol: client={} inbound={} wire_api={} upstream={} alias={} upstream_model={}",
        report.client.label(),
        report.inbound_path,
        protocol.wire_api(),
        protocol.upstream_path(),
        ALIAS,
        MODEL
    );
    println!(
        "idle: {:.1}s warmed, RSS median {:.2} MiB (first {:.2}, max {:.2}), CPU {:.4}% of one core ({} samples)",
        report.idle.secs,
        report.idle.rss_median_mib,
        report.idle.rss_first_mib,
        report.idle.rss_max_mib,
        report.idle.cpu_pct,
        report.idle.samples
    );
    let latency = &report.latency;
    println!(
        "latency: first SSE event direct p50 {:.3} ms / p95 {:.3} ms; relay p50 {:.3} ms / p95 {:.3} ms; added p50 {:.3} ms / p95 {:.3} ms / max {:.3} ms (n={}, warmup={})",
        latency.direct.p50,
        latency.direct.p95,
        latency.relay.p50,
        latency.relay.p95,
        latency.added.p50,
        latency.added.p95,
        latency.added.max,
        latency.samples,
        latency.warmups
    );
    let streams = &report.streams;
    println!(
        "streams: {} synchronized, peak RSS {:.2} MiB (baseline {:.2}, delta {:.2}), mock peak concurrency {}, first-event p95 {:.3} ms",
        streams.streams,
        streams.peak_mib,
        streams.baseline_mib,
        streams.peak_mib - streams.baseline_mib,
        streams.peak_active,
        streams.first_event_p95_ms
    );
    println!(
        "mock: {} upstream hits, {} used the relay-injected key, {} rejected",
        report.upstream_hits, report.relay_key_hits, report.rejected
    );
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut shortened = text.chars().take(limit).collect::<String>();
    shortened.push('…');
    shortened
}
