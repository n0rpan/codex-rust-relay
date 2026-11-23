//! Opt-in end-to-end tests against the **installed stock Claude Code CLI**.
//!
//! These tests reproduce the loopback proof that exercises the gateway with
//! real Claude Code 2.1.330: read a disposable file, edit it, run a check, then
//! resume the same session after the relay process restarted.
//!
//! Four scenarios cover the three upstream wires and both configured Anthropic
//! Messages versions:
//!
//! * Responses wire.
//! * Chat Completions wire (translated by the gateway).
//! * Anthropic Messages wire, configured `anthropic_version = "2023-06-01"`.
//! * Anthropic Messages wire, configured `anthropic_version = "2026-10-21"`.
//!
//! Six more scenarios, built only with `--features test-brave`, add stock
//! Claude Code's native `WebSearch` tool to the same task: four call it
//! directly on each wire, two discover it first on the translated wires. Stock
//! 2.1.330 implements `WebSearch` as a dedicated, mechanical Messages
//! follow-up; the relay answers it from a loopback Brave fixture instead of
//! forwarding it to the vendor. Those scenarios assert exactly one direct Brave
//! query with the fixture token, the citation URLs and snippets in Claude
//! Code's `WebSearch` tool result, the same citations replayed in the resumed
//! vendor request, and the usual read/edit/test proof.
//!
//! Two further search scenarios, one per translated wire, reproduce the
//! discovery half of the stock lifecycle that the direct-call cases skip:
//! stock Claude Code advertises `ToolSearch` but withholds `WebSearch`'s
//! schema, the fixture model calls `ToolSearch` with `select:WebSearch`, stock
//! Claude Code answers locally with a `tool_reference`, and only then does the
//! vendor request advertise a callable `WebSearch` function with its full
//! schema. The model emits `WebSearch` only after that schema is visible.
//!
//! Every scenario drives the same genuine task through stock Claude Code:
//! the fixture model asks for Claude Code's own `Read`, `Edit` and `Bash`
//! tools, Claude Code executes them against the disposable project, and the
//! fixture then answers with its final text. A second `ox claude --resume`
//! invocation proves the completed tool history survives a relay restart.
//!
//! Scope and safety properties:
//!
//! * A fresh `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, TMPDIR and XDG tree per
//!   scenario; no user settings, sessions or credentials are read.
//! * The stock CLI path is explicit, `OX_TEST_CLAUDE_BIN`. Running an ignored
//!   test without it fails with a message naming the variable instead of
//!   silently skipping the integration.
//! * Only loopback sockets are used, with every proxy variable pointing at a
//!   dead loopback port so accidental public egress fails closed.
//! * Permissions are explicit allowlists for the fixture tools, never
//!   `--dangerously-skip-permissions` or `bypassPermissions`.
//! * No Git writes: the disposable project is not a repository.
//!
//! The search scenarios need the relay binary built with the `test-brave`
//! feature: that test-only seam lets `BraveSearch` accept the loopback Brave
//! fixture through `CODEOX_TEST_BRAVE_BASE_URL` with the dummy
//! `brave-fixture-key`. Production configuration has no such override.
//!
//! Run the offline portion with plain `cargo test --test stock_claude_smoke`.
//! Run the real integration manually; add `--features test-brave` to include
//! the six search scenarios:
//!
//! ```sh
//! OX_TEST_CLAUDE_BIN=/path/to/claude-2.1.330 \
//!   cargo test --features test-brave --test stock_claude_smoke \
//!   -- --ignored --nocapture
//! ```
#![allow(dead_code)]

#[path = "../src/catalog.rs"]
mod catalog;
#[path = "../src/provider.rs"]
mod provider;

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const PROMPT_INITIAL: &str = "Use the Read tool to read fixture.txt, then use the Edit tool to replace broken with fixed, then use Bash to print fixture.txt and verify it says fixed.";
const PROMPT_RESUME: &str = "Resume the fixture.";
const FINAL_TEXT: &str = "Coding fixture passed.";
const RESUME_TEXT: &str = "Resumed with coding history.";
const CHECK_OUTPUT: &str = "CODEOX_CHECK_PASSED";
const BASH_CMD: &str =
    "cat fixture.txt && test \"$(cat fixture.txt)\" = fixed && printf '%s' CODEOX_CHECK_PASSED";
// Search scenarios: stock Claude Code's own `WebSearch` call, answered by the
// relay from the loopback Brave fixture.
const SEARCH_PROMPT: &str = "Search the web for codeox, then use the Read tool to read fixture.txt, then use the Edit tool to replace broken with fixed, then use Bash to print fixture.txt and verify it says fixed.";
const SEARCH_QUERY: &str = "codeox";
const SEARCH_URL: &str = "https://www.rust-lang.org/learn";
const SEARCH_TITLE: &str = "Learn Rust";
const SEARCH_SNIPPET: &str = "The Rust Book is the primary learning resource.";
const SEARCH_SECOND_URL: &str = "https://doc.rust-lang.org/book/";
const BRAVE_ENV_KEY: &str = "BRAVE_SEARCH_API_KEY";
const BRAVE_FIXTURE_KEY: &str = "brave-fixture-key";
const TEST_BRAVE_BASE_ENV: &str = "CODEOX_TEST_BRAVE_BASE_URL";
/// Exact dedicated system instruction stock Claude Code 2.1.330 sends for its
/// `WebSearch` follow-up. Seeing it at the vendor means the relay failed to
/// intercept the follow-up.
const DEDICATED_SEARCH_PROMPT: &str = "You run a web search for another Claude model, which is waiting on the result and can search again. Search once, then reply with a brief summary of the results.";
const RUN_TIMEOUT: Duration = Duration::from_secs(120);
const CLAUDE_BIN_ENV: &str = "OX_TEST_CLAUDE_BIN";

/// Serialize the real CLI runs: each scenario has its own ports and isolation
/// tree, but the stock CLI is a shared, stateful installation.
static CLI_LOCK: Mutex<()> = Mutex::new(());

fn cli_lock() -> MutexGuard<'static, ()> {
    CLI_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    Responses,
    Chat,
    Anthropic,
}

impl Wire {
    fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Chat => "chat_completions",
            Self::Anthropic => "anthropic_messages",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::Responses => "/v1/responses",
            Self::Chat => "/v1/chat/completions",
            Self::Anthropic => "/v1/messages",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Endpoint {
    wire: Wire,
    anthropic_version: Option<&'static str>,
}

fn same_wire(wire: Wire, anthropic_version: Option<&'static str>) -> Endpoint {
    Endpoint {
        wire,
        anthropic_version,
    }
}

// ---------------------------------------------------------------------------
// Offline configuration checks
// ---------------------------------------------------------------------------

#[test]
fn every_claude_scenario_config_is_accepted_offline() {
    for endpoint in [
        same_wire(Wire::Responses, None),
        same_wire(Wire::Chat, None),
        same_wire(Wire::Anthropic, Some("2023-06-01")),
        same_wire(Wire::Anthropic, Some("2026-10-21")),
    ] {
        let raw = scenario_config(endpoint, 9_001);
        let config = provider::CodeoxConfig::load_from_str(&raw).unwrap_or_else(|error| {
            panic!(
                "the Claude Code fixture config for {:?} is rejected: {error}\n{raw}",
                endpoint.wire
            )
        });
        assert_eq!(config.vendors.len(), 1, "{raw}");
        let vendor = &config.vendors[0];
        assert_eq!(vendor.key, "first");
        assert_eq!(vendor.env_key.as_deref(), Some("FIRST_KEY"));
        assert_eq!(
            vendor.wire,
            match endpoint.wire {
                Wire::Responses => provider::Wire::Responses,
                Wire::Chat => provider::Wire::ChatCompletions,
                Wire::Anthropic => provider::Wire::AnthropicMessages,
            },
            "{raw}"
        );
        assert_eq!(vendor.models.len(), 1, "{raw}");
        assert_eq!(vendor.models[0].id, "one-model");
        assert_eq!(
            vendor.anthropic.version,
            endpoint.anthropic_version.unwrap_or("2023-06-01"),
            "{raw}"
        );
        assert_eq!(
            provider::alias_for(&vendor.key, "one-model", None).expect("alias"),
            "first/one-model"
        );
    }
}

#[test]
fn every_claude_search_scenario_config_is_accepted_offline() {
    for endpoint in [
        same_wire(Wire::Responses, None),
        same_wire(Wire::Chat, None),
        same_wire(Wire::Anthropic, Some("2023-06-01")),
        same_wire(Wire::Anthropic, Some("2026-10-21")),
    ] {
        let raw = search_scenario_config(endpoint, 9_001);
        let config = provider::CodeoxConfig::load_from_str(&raw).unwrap_or_else(|error| {
            panic!(
                "the Claude Code search fixture config for {:?} is rejected: {error}\n{raw}",
                endpoint.wire
            )
        });
        let search = config
            .search
            .as_ref()
            .unwrap_or_else(|| panic!("the search table must be parsed: {raw}"));
        assert_eq!(search.env_key, BRAVE_ENV_KEY, "{raw}");
        assert_eq!(search.max_results, 4, "{raw}");
        assert_eq!(search.timeout_seconds, 30, "{raw}");
    }
}

#[test]
fn manual_suite_requires_an_explicit_claude_path() {
    let error = stock_claude_binary_from(None).expect_err("an unset variable must fail");
    assert!(
        error.contains(CLAUDE_BIN_ENV),
        "the error must name {CLAUDE_BIN_ENV}: {error}"
    );
    let error = stock_claude_binary_from(Some(OsString::from("/definitely/missing/claude")))
        .expect_err("a missing file must fail");
    assert!(
        error.contains("/definitely/missing/claude"),
        "the error must name the bad path: {error}"
    );
}

// ---------------------------------------------------------------------------
// Opt-in integration scenarios
// ---------------------------------------------------------------------------

#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN to the stock Claude Code binary"]
fn stock_claude_responses_edit_check_resume() {
    run_scenario(same_wire(Wire::Responses, None));
}

#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN to the stock Claude Code binary"]
fn stock_claude_chat_completions_edit_check_resume() {
    run_scenario(same_wire(Wire::Chat, None));
}

#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN to the stock Claude Code binary"]
fn stock_claude_anthropic_old_messages_edit_check_resume() {
    run_scenario(same_wire(Wire::Anthropic, Some("2023-06-01")));
}

#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN to the stock Claude Code binary"]
fn stock_claude_anthropic_current_messages_edit_check_resume() {
    run_scenario(same_wire(Wire::Anthropic, Some("2026-10-21")));
}

// Search + coding scenarios. Each one proves that stock Claude Code's own
// native `WebSearch` tool reaches the loopback Brave fixture, that Brave's
// citation URLs and snippets come back through Claude Code's tool result and
// the resumed vendor request, and that the same session still reads, edits,
// tests and resumes. The `test-brave` feature is the test-only seam that lets
// `BraveSearch` accept the fixture's loopback origin; these tests only exist
// when that feature is compiled in.

#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN and build with --features test-brave"]
fn stock_claude_responses_search_coding_resume() {
    run_search_scenario(same_wire(Wire::Responses, None), false);
}

#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN and build with --features test-brave"]
fn stock_claude_chat_completions_search_coding_resume() {
    run_search_scenario(same_wire(Wire::Chat, None), false);
}

#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN and build with --features test-brave"]
fn stock_claude_anthropic_old_messages_search_coding_resume() {
    run_search_scenario(same_wire(Wire::Anthropic, Some("2023-06-01")), false);
}

#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN and build with --features test-brave"]
fn stock_claude_anthropic_current_messages_search_coding_resume() {
    run_search_scenario(same_wire(Wire::Anthropic, Some("2026-10-21")), false);
}

// Discovery variants on the translated wires. Stock Claude Code declares
// `WebSearch` as a deferred tool without a schema, so a real model must call
// its local `ToolSearch` first. These scenarios assert that ordering and that
// the following vendor request advertises the schema the model just fetched.
#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN and build with --features test-brave"]
fn stock_claude_responses_toolsearch_search_coding_resume() {
    run_search_scenario(same_wire(Wire::Responses, None), true);
}

#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN and build with --features test-brave"]
fn stock_claude_chat_completions_toolsearch_search_coding_resume() {
    run_search_scenario(same_wire(Wire::Chat, None), true);
}

fn run_scenario(endpoint: Endpoint) {
    let _guard = cli_lock();
    let claude = stock_claude_binary();
    let version = claude_version(&claude);
    println!("Testing stock Claude Code: {version}");
    let relay = PathBuf::from(env!("CARGO_BIN_EXE_ox"));

    let label = format!(
        "{}-{}",
        endpoint.wire.as_str(),
        endpoint.anthropic_version.unwrap_or("none")
    );
    let isolation = TempTree::new(&label);
    let home = isolation.path.join("home");
    let claude_home = isolation.path.join("claude-config");
    let codex_home = isolation.path.join("codex-home");
    let project = isolation.path.join("project");
    let tmp = isolation.path.join("tmp");
    let bin = isolation.path.join("bin");
    for directory in [
        &home,
        &claude_home,
        &codex_home,
        &project,
        &tmp,
        &bin,
        &isolation.path.join("cache"),
        &isolation.path.join("config"),
        &isolation.path.join("data"),
    ] {
        fs::create_dir_all(directory).expect("create isolation directory");
    }
    write_claude_shim(&bin, &claude);
    let settings_path = claude_home.join("settings.json");
    let client_settings = json!({
        "env": {
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:1",
            "ANTHROPIC_AUTH_TOKEN": "fixture-stale-claude-token",
            "FIRST_KEY": "fixture-stale-vendor-key",
        }
    })
    .to_string();
    fs::write(&settings_path, &client_settings).expect("write conflicting client settings");

    let fixture = spawn_fixture(FixtureSpec {
        wire: endpoint.wire,
        upstream_model: "one-model",
        expected_key: "first-fixture-key",
        anthropic_version: endpoint.anthropic_version,
        fixture_path: project.join("fixture.txt"),
        search: false,
        discover: false,
    });
    fs::write(
        codex_home.join("codeox.toml"),
        scenario_config(endpoint, fixture.port),
    )
    .expect("write codeox.toml");
    let fixture_file = project.join("fixture.txt");
    fs::write(&fixture_file, "broken\n").expect("write disposable fixture");

    let environment = scenario_env(&isolation.path, &home, &claude_home, &codex_home, &tmp);

    let initial = run_claude(
        &relay,
        &project,
        &environment,
        &[
            "-p",
            PROMPT_INITIAL,
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "first/one-model",
            "--allowedTools",
            "Read",
            "Edit",
            "Bash",
        ],
    );
    let initial_events = json_lines(&initial);
    let session = claude_session(&initial_events);
    let calls = assert_coding_stream(&initial_events);
    assert_eq!(
        fs::read_to_string(&fixture_file).expect("read fixture after initial turn"),
        "fixed\n",
        "only Claude Code may edit the disposable file"
    );

    let resumed = run_claude(
        &relay,
        &project,
        &environment,
        &[
            "-p",
            "--resume",
            &session,
            PROMPT_RESUME,
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "first/one-model",
            "--allowedTools",
            "Read",
            "Edit",
            "Bash",
        ],
    );
    let resumed_events = json_lines(&resumed);
    assert_eq!(
        claude_session(&resumed_events),
        session,
        "the resume run must continue the same Claude Code session"
    );
    assert_final_result(&resumed_events, RESUME_TEXT);
    assert_eq!(fs::read_to_string(settings_path).unwrap(), client_settings);
    assert!(
        !fs::read_dir(&tmp).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".codeox-claude-settings-")
        }),
        "both launch settings files should be removed after the children exit"
    );
    for output in [&initial, &resumed] {
        assert!(!output.stdout.contains("fixture-stale-"));
        assert!(!output.stderr.contains("fixture-stale-"));
    }
    assert_eq!(
        fs::read_to_string(&fixture_file).expect("read fixture after resume"),
        "fixed\n",
        "resume must not touch the workspace"
    );

    let requests = fixture.requests();
    assert_eq!(
        requests.len(),
        5,
        "four coding turns plus one resume request; saw {:?}",
        requests.iter().map(|r| r.stage).collect::<Vec<_>>()
    );
    for (index, (request, stage)) in requests.iter().zip([0_usize, 1, 2, 3, 3]).enumerate() {
        assert_request_contract(
            endpoint,
            request,
            "one-model",
            "first-fixture-key",
            stage,
            index,
        );
    }
    assert_first_request_tools(endpoint.wire, &requests[0].body);

    let resume_body = &requests[4].body;
    let mut history = resume_body.to_string();
    history.push_str(&session);
    for needle in [PROMPT_INITIAL, PROMPT_RESUME, FINAL_TEXT] {
        assert!(
            history.contains(needle),
            "the resume request must replay {needle:?}; body:\n{resume_body}"
        );
    }
    for call in &calls.calls {
        assert!(
            resume_body.to_string().contains(&call.id),
            "the resume request must replay tool call {:?} ({:?}); body:\n{resume_body}",
            call.name,
            call.id
        );
    }
    assert_history(endpoint.wire, resume_body, &calls, false, false);
    assert!(
        resume_body.to_string().contains(PROMPT_RESUME),
        "the new resume prompt must reach the vendor"
    );

    let violations = fixture.violations();
    assert!(
        violations.is_empty(),
        "the upstream fixture observed gateway contract violations: {violations:#?}"
    );

    println!(
        "PASS {} ({}): stock Claude Code {} read, edited, checked and resumed the fixture \
         ({} upstream requests, tools {:?})",
        endpoint.wire.as_str(),
        endpoint.anthropic_version.unwrap_or("no version"),
        version,
        requests.len(),
        calls
            .calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>()
    );
}

/// One vendor, stock Claude Code's native `WebSearch`, the loopback Brave
/// fixture, and the same coding task. With `discover`, the fixture first calls
/// stock `ToolSearch` so the vendor request after it must advertise the
/// deferred `WebSearch` schema. The two `ox claude` calls each launch a fresh
/// relay process, so the second call is a restart plus resume of the same
/// Claude Code session.
#[cfg(feature = "test-brave")]
fn run_search_scenario(endpoint: Endpoint, discover: bool) {
    let _guard = cli_lock();
    let claude = stock_claude_binary();
    let version = claude_version(&claude);
    println!("Testing stock Claude Code: {version}");
    let relay = PathBuf::from(env!("CARGO_BIN_EXE_ox"));

    let label = format!(
        "{}-{}-{}",
        endpoint.wire.as_str(),
        if discover {
            "toolsearch-search"
        } else {
            "search"
        },
        endpoint.anthropic_version.unwrap_or("none")
    );
    let isolation = TempTree::new(&label);
    let home = isolation.path.join("home");
    let claude_home = isolation.path.join("claude-config");
    let codex_home = isolation.path.join("codex-home");
    let project = isolation.path.join("project");
    let tmp = isolation.path.join("tmp");
    let bin = isolation.path.join("bin");
    for directory in [
        &home,
        &claude_home,
        &codex_home,
        &project,
        &tmp,
        &bin,
        &isolation.path.join("cache"),
        &isolation.path.join("config"),
        &isolation.path.join("data"),
    ] {
        fs::create_dir_all(directory).expect("create isolation directory");
    }
    write_claude_shim(&bin, &claude);

    let fixture = spawn_fixture(FixtureSpec {
        wire: endpoint.wire,
        upstream_model: "one-model",
        expected_key: "first-fixture-key",
        anthropic_version: endpoint.anthropic_version,
        fixture_path: project.join("fixture.txt"),
        search: true,
        discover,
    });
    let brave = spawn_brave_fixture();
    fs::write(
        codex_home.join("codeox.toml"),
        search_scenario_config(endpoint, fixture.port),
    )
    .expect("write codeox.toml");
    let fixture_file = project.join("fixture.txt");
    fs::write(&fixture_file, "broken\n").expect("write disposable fixture");

    let mut environment = scenario_env(&isolation.path, &home, &claude_home, &codex_home, &tmp);
    environment.push((BRAVE_ENV_KEY.to_owned(), BRAVE_FIXTURE_KEY.to_owned()));
    environment.push((
        TEST_BRAVE_BASE_ENV.to_owned(),
        format!("http://127.0.0.1:{}", brave.port),
    ));

    let initial = run_claude(
        &relay,
        &project,
        &environment,
        &[
            "-p",
            SEARCH_PROMPT,
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "first/one-model",
            "--allowedTools",
            "Read",
            "Edit",
            "Bash",
            "WebSearch",
        ],
    );
    let initial_events = json_lines(&initial);
    let session = claude_session(&initial_events);
    let calls = assert_search_coding_stream(&initial_events, discover);
    assert_eq!(
        fs::read_to_string(&fixture_file).expect("read fixture after initial turn"),
        "fixed\n",
        "only Claude Code may edit the disposable file"
    );

    let resumed = run_claude(
        &relay,
        &project,
        &environment,
        &[
            "-p",
            "--resume",
            &session,
            PROMPT_RESUME,
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "first/one-model",
            "--allowedTools",
            "Read",
            "Edit",
            "Bash",
            "WebSearch",
        ],
    );
    let resumed_events = json_lines(&resumed);
    assert_eq!(
        claude_session(&resumed_events),
        session,
        "the resume run must continue the same Claude Code session"
    );
    assert_final_result(&resumed_events, RESUME_TEXT);
    assert_eq!(
        fs::read_to_string(&fixture_file).expect("read fixture after resume"),
        "fixed\n",
        "resume must not touch the workspace"
    );

    let requests = fixture.requests();
    // Discovery adds one upstream request (`ToolSearch`); everything else is
    // unchanged. The dedicated search follow-up is answered locally either way.
    let final_stage = 4 + usize::from(discover);
    let expected_requests = 6 + usize::from(discover);
    assert_eq!(
        requests.len(),
        expected_requests,
        "expected ToolSearch (when enabled), WebSearch, read, edit, check and final text, \
         then one resume request; the dedicated search follow-up must be answered locally \
         and never reach the vendor; saw {:?}",
        requests.iter().map(|r| r.stage).collect::<Vec<_>>()
    );
    let mut stages = (0..=final_stage).collect::<Vec<_>>();
    stages.push(final_stage);
    for (index, (request, stage)) in requests.iter().zip(stages).enumerate() {
        assert_request_contract(
            endpoint,
            request,
            "one-model",
            "first-fixture-key",
            stage,
            index,
        );
    }
    assert_first_request_tools(endpoint.wire, &requests[0].body);
    if discover {
        assert_toolsearch_discovery(endpoint.wire, &requests);
    }
    assert_no_dedicated_search_reached_vendor(&requests);

    // The citation content Claude Code received from Brave is replayed to the
    // vendor immediately after the search and again after the relay restart.
    let after_search = 1 + usize::from(discover);
    assert_search_citation_replayed(
        endpoint.wire,
        &requests[after_search].body,
        "the request after WebSearch",
    );
    let resume_index = expected_requests - 1;
    assert_eq!(
        requests[resume_index].stage, final_stage,
        "resume must replay the completed coding history including WebSearch"
    );
    assert!(
        requests[resume_index]
            .body
            .to_string()
            .contains(PROMPT_RESUME),
        "the new resume prompt must reach the vendor"
    );
    let resume_body = requests[resume_index].body.clone();
    let mut history = resume_body.to_string();
    history.push_str(&session);
    for needle in [SEARCH_PROMPT, PROMPT_RESUME, FINAL_TEXT] {
        assert!(
            history.contains(needle),
            "the resume request must replay {needle:?}; body:\n{resume_body}"
        );
    }
    for call in &calls.calls {
        assert!(
            resume_body.to_string().contains(&call.id),
            "the resume request must replay tool call {:?} ({:?}); body:\n{resume_body}",
            call.name,
            call.id
        );
    }
    assert_history(endpoint.wire, &resume_body, &calls, true, discover);
    assert_search_citation_replayed(endpoint.wire, &resume_body, "resume after restart");

    let hits = brave.hits();
    assert_eq!(
        hits.len(),
        1,
        "stock Claude Code must make exactly one direct Brave search: {hits:#?}"
    );
    assert_eq!(hits[0].path, "/res/v1/llm/context");
    assert_eq!(hits[0].token.as_deref(), Some(BRAVE_FIXTURE_KEY));
    assert!(
        hits[0]
            .query
            .contains(&("maximum_number_of_urls".to_owned(), "4".to_owned())),
        "Brave must receive the configured result limit: {:?}",
        hits[0].query
    );
    assert!(
        hits[0]
            .query
            .contains(&("q".to_owned(), SEARCH_QUERY.to_owned())),
        "Brave must receive stock Claude Code's exact query: {:?}",
        hits[0].query
    );
    for (name, value) in &hits[0].query {
        if name == "q" {
            continue;
        }
        assert!(
            !value.contains("You run a web search")
                && !value.contains(SEARCH_PROMPT)
                && !value.contains("Perform a web search"),
            "Brave must receive only the query and bounded options, not request state: \
             {name}={value:?}"
        );
    }

    let violations = fixture
        .violations()
        .into_iter()
        .chain(brave.violations())
        .collect::<Vec<_>>();
    assert!(
        violations.is_empty(),
        "the loopback fixtures observed gateway contract violations: {violations:#?}"
    );

    println!(
        "PASS {} ({}) {}search coding: stock Claude Code {} {}, received Brave citations, \
         read, edited, checked, restarted the relay and resumed ({} upstream requests, \
         1 Brave request, tools {:?})",
        endpoint.wire.as_str(),
        endpoint.anthropic_version.unwrap_or("no version"),
        if discover { "toolsearch " } else { "" },
        version,
        if discover {
            "discovered WebSearch through ToolSearch, then called WebSearch once"
        } else {
            "called WebSearch once"
        },
        requests.len(),
        calls
            .calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Stock CLI lookup and isolation
// ---------------------------------------------------------------------------

fn stock_claude_binary() -> PathBuf {
    stock_claude_binary_from(env::var_os(CLAUDE_BIN_ENV)).unwrap_or_else(|error| panic!("{error}"))
}

fn stock_claude_binary_from(raw: Option<OsString>) -> Result<PathBuf, String> {
    let Some(raw) = raw else {
        return Err(format!(
            "{CLAUDE_BIN_ENV} is not set. These tests require the real installed stock Claude \
             Code CLI; set {CLAUDE_BIN_ENV} to e.g. \
             /home/$USER/.local/share/mise/installs/claude/2.1.330/claude"
        ));
    };
    let path = PathBuf::from(raw);
    if !path.is_file() {
        return Err(format!(
            "{CLAUDE_BIN_ENV} points at {path:?}, which is not a file"
        ));
    }
    Ok(path)
}

fn claude_version(claude: &Path) -> String {
    let output = Command::new(claude)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("could not run {claude:?} --version: {error}"));
    assert!(
        output.status.success(),
        "{claude:?} --version failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .to_owned()
}

/// `ox claude` launches `claude` by name. Put the explicitly configured stock
/// binary first on the child's PATH through a symlink directory so no ambient
/// installation can be picked up by accident.
#[cfg(unix)]
fn write_claude_shim(bin: &Path, claude: &Path) {
    let shim = bin.join("claude");
    std::os::unix::fs::symlink(claude, &shim)
        .unwrap_or_else(|error| panic!("could not symlink {shim:?} -> {claude:?}: {error}"));
}

#[cfg(not(unix))]
compile_error!("the stock Claude Code smoke suite currently supports Unix hosts only");

struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "ox-stock-claude-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create isolation root");
        Self { path }
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn scenario_config(endpoint: Endpoint, port: u16) -> String {
    let version = endpoint
        .anthropic_version
        .map(|version| format!("anthropic_version = \"{version}\"\n"))
        .unwrap_or_default();
    format!(
        r#"[providers.first]
name = "First Fixture Vendor"
base_url = "http://127.0.0.1:{port}/v1"
env_key = "FIRST_KEY"
wire_api = "{wire}"
discovery = false
max_output_tokens = 4096
{version}
[[providers.first.models]]
id = "one-model"
name = "First Fixture Model"
context_window = 131072
input_modalities = ["text"]
reasoning_efforts = ["low", "medium", "high"]
"#,
        wire = endpoint.wire.as_str(),
    )
}

/// The coding fixture config plus the opt-in `[search]` table that lets the
/// relay answer stock Claude Code's dedicated `WebSearch` follow-up from Brave.
fn search_scenario_config(endpoint: Endpoint, port: u16) -> String {
    let mut raw = scenario_config(endpoint, port);
    raw.push_str(
        r#"
[search]
env_key = "BRAVE_SEARCH_API_KEY"
max_results = 4
"#,
    );
    raw
}

/// A minimal environment. Nothing from the invoking user's shell is inherited:
/// no real provider keys, no `ANTHROPIC_*` state, no user settings or sessions.
fn scenario_env(
    root: &Path,
    home: &Path,
    claude_home: &Path,
    codex_home: &Path,
    tmp: &Path,
) -> Vec<(String, String)> {
    let dead_proxy = "http://127.0.0.1:1";
    [
        (
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        ),
        ("HOME", home.display().to_string()),
        ("CLAUDE_CONFIG_DIR", claude_home.display().to_string()),
        ("CODEX_HOME", codex_home.display().to_string()),
        ("FIRST_KEY", "first-fixture-key".to_owned()),
        ("TMPDIR", tmp.display().to_string()),
        ("TMP", tmp.display().to_string()),
        ("TEMP", tmp.display().to_string()),
        ("XDG_CACHE_HOME", root.join("cache").display().to_string()),
        ("XDG_CONFIG_HOME", root.join("config").display().to_string()),
        ("XDG_DATA_HOME", root.join("data").display().to_string()),
        ("TERM", "dumb".to_owned()),
        ("LANG", "C.UTF-8".to_owned()),
        ("LC_ALL", "C.UTF-8".to_owned()),
        ("SHELL", "/bin/bash".to_owned()),
        // Keep the stock CLI's own incidental traffic local and failing closed.
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
        ("DISABLE_TELEMETRY", "1".to_owned()),
        ("DISABLE_AUTOUPDATER", "1".to_owned()),
        ("NO_PROXY", "127.0.0.1,localhost".to_owned()),
        ("no_proxy", "127.0.0.1,localhost".to_owned()),
        ("HTTP_PROXY", dead_proxy.to_owned()),
        ("HTTPS_PROXY", dead_proxy.to_owned()),
        ("ALL_PROXY", dead_proxy.to_owned()),
        ("http_proxy", dead_proxy.to_owned()),
        ("https_proxy", dead_proxy.to_owned()),
        ("all_proxy", dead_proxy.to_owned()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

// ---------------------------------------------------------------------------
// Running stock Claude Code through ox
// ---------------------------------------------------------------------------

struct RunOutput {
    stdout: String,
    stderr: String,
}

fn run_claude(
    relay: &Path,
    project: &Path,
    environment: &[(String, String)],
    args: &[&str],
) -> RunOutput {
    let mut command = Command::new(relay);
    command
        .arg("claude")
        .args(args)
        .current_dir(project)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    let label = format!("ox claude {args:?}");
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("could not start {label}: {error}"));
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_to_string(stdout));
    let stderr_reader = thread::spawn(move || read_to_string(stderr));

    let start = Instant::now();
    let mut timed_out = false;
    let status: ExitStatus = loop {
        match child.try_wait().expect("wait for claude") {
            Some(status) => break status,
            None if start.elapsed() > RUN_TIMEOUT => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().expect("wait after kill");
            }
            None => thread::sleep(Duration::from_millis(20)),
        }
    };
    let stdout = stdout_reader.join().expect("stdout reader");
    let stderr = stderr_reader.join().expect("stderr reader");
    if timed_out {
        panic!(
            "{label} timed out after {RUN_TIMEOUT:?}\nstdout:\n{}\nstderr:\n{}",
            tail(&stdout),
            tail(&stderr)
        );
    }
    assert!(
        status.success(),
        "{label} exited with {status}\nstdout:\n{}\nstderr:\n{}",
        tail(&stdout),
        tail(&stderr)
    );
    RunOutput { stdout, stderr }
}

fn read_to_string(mut stream: impl Read) -> String {
    let mut text = String::new();
    let _ = stream.read_to_string(&mut text);
    text
}

fn tail(text: &str) -> String {
    let start = text.len().saturating_sub(8000);
    text[start..].to_owned()
}

fn json_lines(output: &RunOutput) -> Vec<Value> {
    let events = output
        .stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    assert!(
        !events.is_empty(),
        "stock Claude Code produced no stream-json events\nstdout:\n{}\nstderr:\n{}",
        tail(&output.stdout),
        tail(&output.stderr)
    );
    events
}

// ---------------------------------------------------------------------------
// Stock Claude Code output assertions
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ToolCall {
    id: String,
    name: String,
    input: Value,
}

#[derive(Debug)]
struct CodingStream {
    calls: Vec<ToolCall>,
    results: Vec<(String, Value)>,
}

impl CodingStream {
    fn call(&self, name: &str) -> &ToolCall {
        self.calls
            .iter()
            .find(|call| call.name == name)
            .unwrap_or_else(|| panic!("no {name} call: {:#?}", self.calls))
    }

    fn result_for(&self, name: &str) -> &Value {
        let call = self.call(name);
        self.results
            .iter()
            .find(|(id, _)| id == &call.id)
            .map(|(_, block)| block)
            .unwrap_or_else(|| panic!("no result for the {name} call: {:#?}", self.results))
    }
}

fn claude_session(events: &[Value]) -> String {
    events
        .iter()
        .find(|event| event["type"] == "system" && event["subtype"] == "init")
        .and_then(|event| event["session_id"].as_str())
        .unwrap_or_else(|| {
            panic!("stock Claude Code did not emit a session init event: {events:#?}")
        })
        .to_owned()
}

fn assert_coding_stream(events: &[Value]) -> CodingStream {
    let stream = assert_tool_stream(events, &["Read", "Edit", "Bash"], "read, edit and check");
    assert_read_edit_check(&stream);
    assert_final_result(events, FINAL_TEXT);
    stream
}

/// The same coding task with stock Claude Code's native `WebSearch` first. The
/// fixture model asks for one query; Claude Code executes it locally through
/// the relay's dedicated search follow-up and reports the Brave citations in a
/// normal `tool_result` before the read, edit and check calls. With `discover`,
/// the fixture first calls `ToolSearch` and the assertions require its
/// `tool_reference` result to precede the `WebSearch` call.
fn assert_search_coding_stream(events: &[Value], discover: bool) -> CodingStream {
    let mut expected = vec!["WebSearch", "Read", "Edit", "Bash"];
    let description = if discover {
        expected.insert(0, "ToolSearch");
        "tool-search, search, read, edit and check"
    } else {
        "search, read, edit and check"
    };
    let stream = assert_tool_stream(events, &expected, description);
    if discover {
        let discovery = stream.call("ToolSearch");
        assert_eq!(
            discovery.input["query"], "select:WebSearch",
            "the fixture model must discover WebSearch by name before calling it: {:#?}",
            discovery.input
        );
        assert_eq!(
            discovery.input["max_results"], 5,
            "the fixture model must send ToolSearch's required bounded result count: {:#?}",
            discovery.input
        );
        let result = stream.result_for("ToolSearch");
        let reference = result["content"]
            .as_array()
            .map(|blocks| {
                blocks.iter().any(|block| {
                    block["type"] == "tool_reference" && block["tool_name"] == "WebSearch"
                })
            })
            .unwrap_or(false);
        assert!(
            reference,
            "stock Claude Code's ToolSearch result must surface WebSearch as an ordinary \
             tool_reference block before the model may call it: {result}"
        );
    }
    let web = stream.result_for("WebSearch");
    assert_ne!(web["is_error"], true, "the WebSearch tool failed: {web}");
    let content = web["content"].to_string();
    for needle in [
        "Links: [",
        SEARCH_URL,
        SEARCH_TITLE,
        SEARCH_SNIPPET,
        SEARCH_SECOND_URL,
    ] {
        assert!(
            content.contains(needle),
            "Claude Code's WebSearch tool result must carry {needle:?}: {web}"
        );
    }
    assert_eq!(
        stream.call("WebSearch").input["query"],
        SEARCH_QUERY,
        "the fixture model's search query must reach Claude Code's WebSearch tool: {:#?}",
        stream.call("WebSearch").input
    );
    // Claude Code's own structured sidecar proves it ran exactly one search and
    // received Brave's citation objects, independent of the rendered string.
    let sidecar = events
        .iter()
        .find_map(|event| {
            let result = &event["tool_use_result"];
            (result["query"] == SEARCH_QUERY).then_some(result)
        })
        .unwrap_or_else(|| {
            panic!("stock Claude Code did not report a tool_use_result for WebSearch: {events:#?}")
        });
    assert_eq!(
        sidecar["searchCount"], 1,
        "Claude Code must report exactly one search: {sidecar}"
    );
    assert_eq!(
        sidecar["results"][0]["content"][0]["url"], SEARCH_URL,
        "Claude Code must receive Brave's first citation URL: {sidecar}"
    );
    assert_eq!(
        sidecar["results"][0]["content"][0]["title"], SEARCH_TITLE,
        "Claude Code must receive Brave's first citation title: {sidecar}"
    );
    assert_read_edit_check(&stream);
    assert_final_result(events, FINAL_TEXT);
    stream
}

fn assert_tool_stream(events: &[Value], expected: &[&str], description: &str) -> CodingStream {
    let init = events
        .iter()
        .find(|event| event["type"] == "system" && event["subtype"] == "init")
        .unwrap_or_else(|| {
            panic!("stock Claude Code did not emit a session init event: {events:#?}")
        });
    assert_eq!(init["model"], "first/one-model", "{init}");
    let tools = init["tools"]
        .as_array()
        .map(|tools| tools.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    for name in expected {
        assert!(
            tools.contains(name),
            "stock Claude Code did not advertise {name}: {tools:?}"
        );
    }

    let mut calls = Vec::new();
    let mut results: Vec<(String, Value)> = Vec::new();
    for event in events {
        let content = match event["type"].as_str() {
            Some("assistant") => event["message"]["content"].as_array(),
            Some("user") => event["message"]["content"].as_array(),
            _ => None,
        };
        let Some(blocks) = content else { continue };
        for block in blocks {
            match block["type"].as_str() {
                Some("tool_use") => calls.push(ToolCall {
                    id: block["id"].as_str().unwrap_or_default().to_owned(),
                    name: block["name"].as_str().unwrap_or_default().to_owned(),
                    input: block["input"].clone(),
                }),
                Some("tool_result") => results.push((
                    block["tool_use_id"].as_str().unwrap_or_default().to_owned(),
                    block.clone(),
                )),
                _ => {}
            }
        }
    }

    let names = calls
        .iter()
        .map(|call| call.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names, expected,
        "stock Claude Code must execute exactly the fixture's {description} calls: {calls:#?}"
    );
    for call in &calls {
        assert!(
            !call.id.is_empty(),
            "the gateway must stream a tool id for {:?}: {call:?}",
            call.name
        );
    }
    for (tool_use_id, block) in &results {
        assert!(
            calls.iter().any(|call| &call.id == tool_use_id),
            "a tool result refers to unknown tool id {tool_use_id:?}: {block}"
        );
        assert_ne!(
            block["is_error"], true,
            "tool {tool_use_id:?} failed: {block}"
        );
    }
    CodingStream { calls, results }
}

fn assert_read_edit_check(stream: &CodingStream) {
    assert!(
        stream.result_for("Read")["content"]
            .to_string()
            .contains("broken"),
        "the Read tool must return the disposable file: {:#?}",
        stream.result_for("Read")
    );
    assert!(
        stream.result_for("Edit")["content"]
            .to_string()
            .contains("has been updated"),
        "the Edit tool must report success: {:#?}",
        stream.result_for("Edit")
    );
    assert!(
        stream.result_for("Bash")["content"]
            .to_string()
            .contains(CHECK_OUTPUT),
        "the Bash check must run and print {CHECK_OUTPUT}: {:#?}",
        stream.result_for("Bash")
    );
    let read = stream.call("Read");
    let edit = stream.call("Edit");
    let read_path = read.input["file_path"].as_str().unwrap_or_default();
    assert!(
        read_path.ends_with("fixture.txt"),
        "Claude Code's Read call must target the disposable file: {read_path:?}"
    );
    assert_eq!(
        edit.input["old_string"], "broken",
        "the Edit call must replace the fixture content: {:#?}",
        edit.input
    );
    assert_eq!(
        edit.input["new_string"], "fixed",
        "the Edit call must replace the fixture content: {:#?}",
        edit.input
    );
}

fn assert_final_result(events: &[Value], expected: &str) {
    let result = events
        .iter()
        .find(|event| event["type"] == "result")
        .unwrap_or_else(|| panic!("stock Claude Code did not emit a result event: {events:#?}"));
    assert_eq!(result["subtype"], "success", "{result}");
    assert_eq!(
        result["is_error"], false,
        "the Claude Code run reported an error: {result}"
    );
    assert_eq!(
        result["result"], expected,
        "the final assistant text must come from the fixture: {result}"
    );
}

// ---------------------------------------------------------------------------
// Loopback Brave fixture
// ---------------------------------------------------------------------------

#[cfg(feature = "test-brave")]
#[derive(Clone, Debug)]
struct BraveHit {
    path: String,
    query: Vec<(String, String)>,
    token: Option<String>,
}

#[cfg(feature = "test-brave")]
struct BraveFixture {
    port: u16,
    hits: Arc<Mutex<Vec<BraveHit>>>,
    violations: Arc<Mutex<Vec<String>>>,
}

#[cfg(feature = "test-brave")]
impl BraveFixture {
    fn hits(&self) -> Vec<BraveHit> {
        self.hits.lock().expect("Brave hit log").clone()
    }

    fn violations(&self) -> Vec<String> {
        self.violations.lock().expect("Brave violation log").clone()
    }
}

/// Serves Brave's LLM Context shape on loopback. Only the `test-brave` relay
/// seam can reach it; production always uses Brave's real HTTPS origin.
#[cfg(feature = "test-brave")]
fn spawn_brave_fixture() -> BraveFixture {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback Brave fixture");
    let port = listener.local_addr().expect("Brave fixture address").port();
    let hits = Arc::new(Mutex::new(Vec::new()));
    let violations = Arc::new(Mutex::new(Vec::new()));
    let thread_hits = Arc::clone(&hits);
    let thread_violations = Arc::clone(&violations);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            if let Err(error) = handle_brave_connection(stream, &thread_hits, &thread_violations) {
                thread_violations
                    .lock()
                    .expect("Brave violation log")
                    .push(format!("Brave fixture connection error: {error}"));
            }
        }
    });
    BraveFixture {
        port,
        hits,
        violations,
    }
}

#[cfg(feature = "test-brave")]
fn handle_brave_connection(
    mut stream: TcpStream,
    hits: &Mutex<Vec<BraveHit>>,
    violations: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut words = request_line.split_whitespace();
    let method = words.next().unwrap_or_default().to_owned();
    let target = words.next().unwrap_or_default().to_owned();

    let mut token = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("x-subscription-token")
        {
            token = Some(value.trim().to_owned());
        }
    }

    let (path, raw_query) = target.split_once('?').unwrap_or((&target, ""));
    hits.lock().expect("Brave hit log").push(BraveHit {
        path: path.to_owned(),
        query: query_pairs(raw_query),
        token,
    });
    if method != "GET" {
        violations
            .lock()
            .expect("Brave violation log")
            .push(format!("Brave received {method} {target}, expected GET"));
        return respond(&mut stream, 405, "text/plain", b"method not allowed");
    }
    if path != "/res/v1/llm/context" {
        violations
            .lock()
            .expect("Brave violation log")
            .push(format!(
                "Brave received {path}, expected /res/v1/llm/context"
            ));
        return respond(&mut stream, 404, "text/plain", b"not found");
    }
    respond(
        &mut stream,
        200,
        "application/json",
        include_str!("fixtures/brave/llm_context.json").as_bytes(),
    )
}

#[cfg(feature = "test-brave")]
fn query_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(name), percent_decode(value))
        })
        .collect()
}

#[cfg(feature = "test-brave")]
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                match high.zip(low) {
                    Some((high, low)) => {
                        decoded.push((high * 16 + low) as u8);
                        index += 3;
                    }
                    None => {
                        decoded.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

// ---------------------------------------------------------------------------
// Loopback upstream fixture
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RecordedRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
    stage: usize,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

#[derive(Clone)]
struct FixtureSpec {
    wire: Wire,
    upstream_model: &'static str,
    expected_key: &'static str,
    anthropic_version: Option<&'static str>,
    fixture_path: PathBuf,
    /// When true, the fixture's first coding call is stock Claude Code's
    /// `WebSearch`, executed locally by the relay against the Brave fixture.
    search: bool,
    /// When true, the fixture first calls stock Claude Code's `ToolSearch`
    /// with `select:WebSearch` and only then calls `WebSearch`, which is how a
    /// real model obtains the deferred tool's schema.
    discover: bool,
}

struct Fixture {
    port: u16,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    violations: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("request log").clone()
    }

    fn violations(&self) -> Vec<String> {
        self.violations.lock().expect("violation log").clone()
    }
}

fn spawn_fixture(spec: FixtureSpec) -> Fixture {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let violations = Arc::new(Mutex::new(Vec::new()));
    let thread_requests = Arc::clone(&requests);
    let thread_violations = Arc::clone(&violations);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if let Err(error) =
                handle_connection(stream, spec.clone(), &thread_requests, &thread_violations)
            {
                thread_violations
                    .lock()
                    .expect("violation log")
                    .push(format!("fixture connection error: {error}"));
            }
        }
    });
    Fixture {
        port,
        requests,
        violations,
    }
}

fn handle_connection(
    mut stream: TcpStream,
    spec: FixtureSpec,
    requests: &Mutex<Vec<RecordedRequest>>,
    violations: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut words = request_line.split_whitespace();
    let method = words.next().unwrap_or_default().to_owned();
    let path = words.next().unwrap_or_default().to_owned();

    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body_bytes = vec![0_u8; length];
    reader.read_exact(&mut body_bytes)?;

    if method == "GET" {
        let catalog = json!({
            "object": "list",
            "data": [{
                "id": spec.upstream_model,
                "object": "model",
                "created": 1_700_000_000_u64,
                "owned_by": "ox-fixture"
            }]
        });
        return respond(
            &mut stream,
            200,
            "application/json",
            catalog.to_string().as_bytes(),
        );
    }

    if method != "POST" {
        record_violation(
            violations,
            format!("unexpected {method} {path} from the gateway"),
        );
        return respond(&mut stream, 405, "text/plain", b"method not allowed");
    }

    let body: Value = match serde_json::from_slice(&body_bytes) {
        Ok(body) => body,
        Err(error) => {
            record_violation(violations, format!("request body is not JSON: {error}"));
            return respond(&mut stream, 400, "text/plain", b"invalid json");
        }
    };
    let stage = tool_output_count(spec.wire, &body);
    requests.lock().expect("request log").push(RecordedRequest {
        path: path.clone(),
        headers: headers.clone(),
        body: body.clone(),
        stage,
    });

    validate_request(&spec, &path, &headers, &body, violations);
    if spec.search && body.to_string().contains(DEDICATED_SEARCH_PROMPT) {
        record_violation(
            violations,
            format!(
                "stock Claude Code's dedicated WebSearch follow-up reached the vendor with \
                 tool_choice {:?}; the relay must answer it locally with Brave",
                body["tool_choice"]
            ),
        );
        // End the child's turn instead of feeding it another WebSearch call, so
        // a regression fails on the recorded violation and request count
        // rather than looping until the timeout.
        let events = text_events(spec.wire, spec.upstream_model, FINAL_TEXT);
        return respond(
            &mut stream,
            200,
            "text/event-stream",
            &encode_stream(spec.wire, &events),
        );
    }

    let events = response_events(&spec, &body, stage);
    respond(
        &mut stream,
        200,
        "text/event-stream",
        &encode_stream(spec.wire, &events),
    )
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    data: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    )?;
    stream.write_all(data)?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn record_violation(violations: &Mutex<Vec<String>>, message: String) {
    violations.lock().expect("violation log").push(message);
}

fn validate_request(
    spec: &FixtureSpec,
    path: &str,
    headers: &BTreeMap<String, String>,
    body: &Value,
    violations: &Mutex<Vec<String>>,
) {
    if path != spec.wire.endpoint() {
        record_violation(
            violations,
            format!(
                "{} request reached {path}, expected {}",
                spec.wire.as_str(),
                spec.wire.endpoint()
            ),
        );
    }
    if let Some(edge_query) = path.split_once('?').map(|(_, query)| query) {
        record_violation(
            violations,
            format!("the vendor must not receive the client's query string: {edge_query:?}"),
        );
    }
    let model = body["model"].as_str().unwrap_or_default();
    if model != spec.upstream_model {
        record_violation(
            violations,
            format!(
                "upstream model was {model:?}, expected {:?}; the gateway must rewrite the \
                 provider-qualified alias",
                spec.upstream_model
            ),
        );
    }
    match spec.wire {
        Wire::Responses | Wire::Chat => {
            let expected = format!("Bearer {}", spec.expected_key);
            if headers.get("authorization").map(String::as_str) != Some(expected.as_str()) {
                record_violation(
                    violations,
                    format!(
                        "authorization was {:?}, expected {expected:?}",
                        headers.get("authorization")
                    ),
                );
            }
        }
        Wire::Anthropic => {
            if headers.get("x-api-key").map(String::as_str) != Some(spec.expected_key) {
                record_violation(
                    violations,
                    format!(
                        "x-api-key was {:?}, expected {:?}",
                        headers.get("x-api-key"),
                        spec.expected_key
                    ),
                );
            }
            if headers.contains_key("authorization") {
                record_violation(
                    violations,
                    "Anthropic Messages must not send an Authorization header".to_owned(),
                );
            }
            let version = spec.anthropic_version.unwrap_or("2023-06-01");
            if headers.get("anthropic-version").map(String::as_str) != Some(version) {
                record_violation(
                    violations,
                    format!(
                        "anthropic-version was {:?}, expected {version:?}",
                        headers.get("anthropic-version")
                    ),
                );
            }
        }
    }
}

fn tool_output_count(wire: Wire, body: &Value) -> usize {
    match wire {
        Wire::Responses => body["input"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        matches!(
                            item["type"].as_str(),
                            Some("function_call_output" | "custom_tool_call_output")
                        )
                    })
                    .count()
            })
            .unwrap_or(0),
        Wire::Chat => body["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter(|message| message["role"] == "tool")
                    .count()
            })
            .unwrap_or(0),
        Wire::Anthropic => body["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| message["content"].as_array())
                    .flatten()
                    .filter(|block| block["type"] == "tool_result")
                    .count()
            })
            .unwrap_or(0),
    }
}

fn response_events(spec: &FixtureSpec, body: &Value, stage: usize) -> Vec<Value> {
    if body.to_string().contains(PROMPT_RESUME) {
        return text_events(spec.wire, spec.upstream_model, RESUME_TEXT);
    }
    let fixture_path = spec.fixture_path.display().to_string();
    let read_arguments = json!({ "file_path": fixture_path }).to_string();
    let edit_arguments = json!({
        "file_path": fixture_path,
        "old_string": "broken",
        "new_string": "fixed"
    })
    .to_string();
    let check_arguments = json!({ "command": BASH_CMD }).to_string();
    let search_arguments = json!({ "query": SEARCH_QUERY }).to_string();
    let coding_stage = if spec.search {
        if spec.discover {
            match stage {
                0 => {
                    let discover_arguments =
                        json!({ "query": "select:WebSearch", "max_results": 5 }).to_string();
                    return tool_events(spec, "call_discover", "ToolSearch", &discover_arguments);
                }
                1 => return tool_events(spec, "call_search", "WebSearch", &search_arguments),
                other => other - 2,
            }
        } else {
            match stage {
                0 => return tool_events(spec, "call_search", "WebSearch", &search_arguments),
                other => other - 1,
            }
        }
    } else {
        stage
    };
    match coding_stage {
        0 => tool_events(spec, "call_read", "Read", &read_arguments),
        1 => tool_events(spec, "call_edit", "Edit", &edit_arguments),
        2 => tool_events(spec, "call_check", "Bash", &check_arguments),
        3 => text_events(spec.wire, spec.upstream_model, FINAL_TEXT),
        // The driver asserts the exact stage sequence, so this is only a
        // fallback that lets the child exit instead of hanging. It is not
        // evidence of success: the stage assertion fails the test.
        other => {
            eprintln!(
                "warning: fixture saw unexpected stage {other} for {}",
                spec.wire.as_str()
            );
            text_events(spec.wire, spec.upstream_model, FINAL_TEXT)
        }
    }
}

fn tool_events(spec: &FixtureSpec, call_id: &str, name: &str, arguments: &str) -> Vec<Value> {
    match spec.wire {
        Wire::Responses => responses_tool_call(spec.upstream_model, call_id, name, arguments),
        Wire::Chat => chat_tool_call(spec.upstream_model, call_id, name, arguments),
        Wire::Anthropic => anthropic_tool_call(spec.upstream_model, call_id, name, arguments),
    }
}

fn text_events(wire: Wire, model: &str, text: &str) -> Vec<Value> {
    match wire {
        Wire::Responses => responses_text(model, text),
        Wire::Chat => chat_text(model, text),
        Wire::Anthropic => anthropic_text(model, text),
    }
}

fn encode_stream(wire: Wire, events: &[Value]) -> Vec<u8> {
    match wire {
        Wire::Responses | Wire::Anthropic => {
            let mut body = String::new();
            for event in events {
                let name = event["type"].as_str().expect("event type");
                body.push_str(&format!("event: {name}\ndata: {event}\n\n"));
            }
            body.into_bytes()
        }
        Wire::Chat => {
            let mut body = String::new();
            for event in events {
                body.push_str(&format!("data: {event}\n\n"));
            }
            body.push_str("data: [DONE]\n\n");
            body.into_bytes()
        }
    }
}

// ---------------------------------------------------------------------------
// Stock 2.1.330 upstream wire shapes
// ---------------------------------------------------------------------------

fn responses_object(model: &str, status: &str, output: Vec<Value>) -> Value {
    json!({
        "id": "resp_stock_claude",
        "object": "response",
        "created_at": 1_700_000_000_u64,
        "status": status,
        "model": model,
        "output": output,
        "parallel_tool_calls": false,
        "tool_choice": "auto",
        "tools": []
    })
}

fn responses_usage(input: u64, output: u64) -> Value {
    json!({
        "input_tokens": input,
        "input_tokens_details": { "cached_tokens": 0 },
        "output_tokens": output,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": input + output
    })
}

fn responses_tool_call(model: &str, call_id: &str, name: &str, arguments: &str) -> Vec<Value> {
    let item_id = format!("fc_{call_id}");
    let item = json!({
        "id": item_id,
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "arguments": arguments
    });
    vec![
        json!({ "type": "response.created", "response": responses_object(model, "in_progress", vec![]) }),
        json!({ "type": "response.in_progress", "response": responses_object(model, "in_progress", vec![]) }),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": item_id,
                "type": "function_call",
                "status": "in_progress",
                "call_id": call_id,
                "name": name,
                "arguments": ""
            }
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": item_id,
            "output_index": 0,
            "delta": arguments
        }),
        json!({
            "type": "response.function_call_arguments.done",
            "item_id": item_id,
            "output_index": 0,
            "arguments": arguments
        }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_stock_claude",
                "object": "response",
                "created_at": 1_700_000_000_u64,
                "status": "completed",
                "model": model,
                "output": [item],
                "usage": responses_usage(11, 7)
            }
        }),
    ]
}

fn responses_text(model: &str, text: &str) -> Vec<Value> {
    let item = json!({
        "id": "msg_stock_claude",
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{ "type": "output_text", "text": text, "annotations": [] }]
    });
    vec![
        json!({ "type": "response.created", "response": responses_object(model, "in_progress", vec![]) }),
        json!({ "type": "response.in_progress", "response": responses_object(model, "in_progress", vec![]) }),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "id": "msg_stock_claude", "type": "message", "role": "assistant", "status": "in_progress", "content": [] }
        }),
        json!({
            "type": "response.content_part.added",
            "item_id": "msg_stock_claude",
            "output_index": 0,
            "content_index": 0,
            "part": { "type": "output_text", "text": "", "annotations": [] }
        }),
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_stock_claude",
            "output_index": 0,
            "content_index": 0,
            "delta": text
        }),
        json!({
            "type": "response.output_text.done",
            "item_id": "msg_stock_claude",
            "output_index": 0,
            "content_index": 0,
            "text": text
        }),
        json!({
            "type": "response.content_part.done",
            "item_id": "msg_stock_claude",
            "output_index": 0,
            "content_index": 0,
            "part": { "type": "output_text", "text": text, "annotations": [] }
        }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_stock_claude",
                "object": "response",
                "created_at": 1_700_000_000_u64,
                "status": "completed",
                "model": model,
                "output": [item],
                "usage": responses_usage(11, 3)
            }
        }),
    ]
}

fn chat_chunk(model: &str, delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl_stock_claude",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000_u64,
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
    })
}

fn chat_usage(model: &str) -> Value {
    json!({
        "id": "chatcmpl_stock_claude",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000_u64,
        "model": model,
        "choices": [],
        "usage": { "prompt_tokens": 11, "completion_tokens": 4, "total_tokens": 15 }
    })
}

fn chat_tool_call(model: &str, call_id: &str, name: &str, arguments: &str) -> Vec<Value> {
    vec![
        chat_chunk(
            model,
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": "" }
                }]
            }),
            None,
        ),
        chat_chunk(
            model,
            json!({
                "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": arguments }
                }]
            }),
            None,
        ),
        chat_chunk(model, json!({}), Some("tool_calls")),
        chat_usage(model),
    ]
}

fn chat_text(model: &str, text: &str) -> Vec<Value> {
    vec![
        chat_chunk(model, json!({ "role": "assistant", "content": "" }), None),
        chat_chunk(model, json!({ "content": text }), None),
        chat_chunk(model, json!({}), Some("stop")),
        chat_usage(model),
    ]
}

fn anthropic_message_start(model: &str) -> Value {
    json!({
        "type": "message_start",
        "message": {
            "id": "msg_stock_claude",
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": model,
            "stop_reason": null,
            "stop_sequence": null,
            "usage": { "input_tokens": 11, "output_tokens": 1 }
        }
    })
}

fn anthropic_tool_call(model: &str, call_id: &str, name: &str, arguments: &str) -> Vec<Value> {
    vec![
        anthropic_message_start(model),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "tool_use", "id": call_id, "name": name, "input": {} }
        }),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": arguments }
        }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": "tool_use", "stop_sequence": null },
            "usage": { "output_tokens": 7 }
        }),
        json!({ "type": "message_stop" }),
    ]
}

fn anthropic_text(model: &str, text: &str) -> Vec<Value> {
    vec![
        anthropic_message_start(model),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": text }
        }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 3 }
        }),
        json!({ "type": "message_stop" }),
    ]
}

// ---------------------------------------------------------------------------
// Upstream request contract assertions
// ---------------------------------------------------------------------------

fn assert_request_contract(
    endpoint: Endpoint,
    request: &RecordedRequest,
    upstream_model: &str,
    expected_key: &str,
    expected_stage: usize,
    index: usize,
) {
    let label = format!("{} request {index}", endpoint.wire.as_str());
    assert_eq!(
        request.path,
        endpoint.wire.endpoint(),
        "{label}: upstream endpoint"
    );
    assert_eq!(
        request.body["model"], upstream_model,
        "{label}: the provider-qualified alias must be rewritten to the vendor model id"
    );
    assert_eq!(request.stage, expected_stage, "{label}: tool-output stage");
    match endpoint.wire {
        Wire::Responses | Wire::Chat => assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {expected_key}").as_str()),
            "{label}: only the vendor's own bearer key is used"
        ),
        Wire::Anthropic => {
            assert_eq!(
                request.header("x-api-key"),
                Some(expected_key),
                "{label}: only the vendor's own x-api-key is used"
            );
            assert!(
                request.header("authorization").is_none(),
                "{label}: Anthropic Messages must not send Authorization"
            );
            let version = endpoint.anthropic_version.unwrap_or("2023-06-01");
            assert_eq!(
                request.header("anthropic-version"),
                Some(version),
                "{label}: the configured Messages version reaches the vendor"
            );
        }
    }
}

fn assert_first_request_tools(wire: Wire, body: &Value) {
    let names = body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| match wire {
                    Wire::Chat => tool["function"]["name"].as_str(),
                    Wire::Responses | Wire::Anthropic => tool["name"].as_str(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for name in ["Read", "Edit", "Bash"] {
        assert!(
            names.contains(&name),
            "{wire:?}: the upstream request must advertise Claude Code's {name} tool; tools: {names:?}"
        );
    }
}

/// The vendor-facing JSON Schema for one advertised tool, in the wire's shape.
fn wire_tool_schema<'a>(wire: Wire, body: &'a Value, name: &str) -> Option<&'a Value> {
    body["tools"].as_array()?.iter().find_map(|tool| {
        let (tool_name, schema) = match wire {
            Wire::Chat => (
                tool["function"]["name"].as_str(),
                &tool["function"]["parameters"],
            ),
            Wire::Responses => (tool["name"].as_str(), &tool["parameters"]),
            Wire::Anthropic => (tool["name"].as_str(), &tool["input_schema"]),
        };
        (tool_name == Some(name) && schema.is_object()).then_some(schema)
    })
}

/// The translated-wire proof that stock `ToolSearch` is what surfaced
/// `WebSearch`: the first vendor request offers a callable `ToolSearch` but no
/// `WebSearch` schema, and the next one carries both the discovery history and
/// a complete `WebSearch` function the model can now invoke.
#[cfg(feature = "test-brave")]
fn assert_toolsearch_discovery(wire: Wire, requests: &[RecordedRequest]) {
    let search_tool =
        wire_tool_schema(wire, &requests[0].body, "ToolSearch").unwrap_or_else(|| {
            panic!(
                "{wire:?}: the first request must advertise a callable ToolSearch: {}",
                requests[0].body
            )
        });
    assert!(
        search_tool["properties"]["query"].is_object(),
        "{wire:?}: ToolSearch must advertise its query parameter: {search_tool}"
    );
    assert!(
        wire_tool_schema(wire, &requests[0].body, "WebSearch").is_none(),
        "{wire:?}: WebSearch must stay deferred until ToolSearch surfaces it: {}",
        requests[0].body
    );

    let web = wire_tool_schema(wire, &requests[1].body, "WebSearch").unwrap_or_else(|| {
        panic!(
            "{wire:?}: the request after ToolSearch must advertise WebSearch, or the model \
             cannot see the schema it just discovered: {}",
            requests[1].body
        )
    });
    for parameter in ["query", "allowed_domains", "blocked_domains"] {
        assert!(
            web["properties"][parameter].is_object(),
            "{wire:?}: the surfaced WebSearch schema must describe {parameter}: {web}"
        );
    }
    assert_eq!(
        web["properties"]["query"]["minLength"], 2,
        "{wire:?}: the surfaced WebSearch schema must be the deferred one, not a stub: {web}"
    );
    assert!(
        web["required"]
            .as_array()
            .is_some_and(|required| required.iter().any(|name| name == "query")),
        "{wire:?}: the surfaced WebSearch schema must require query: {web}"
    );

    let history = requests[1].body.to_string();
    assert!(
        history.contains("call_discover"),
        "{wire:?}: the request with WebSearch must replay the ToolSearch call: {}",
        requests[1].body
    );
    assert!(
        history.contains("[tool_reference: WebSearch]"),
        "{wire:?}: the request with WebSearch must replay stock ToolSearch's tool_reference \
         result: {}",
        requests[1].body
    );
}

fn assert_history(wire: Wire, body: &Value, calls: &CodingStream, search: bool, discover: bool) {
    let text = body.to_string();
    let mut names = vec!["Read", "Edit", "Bash"];
    if search {
        names.insert(0, "WebSearch");
    }
    if discover {
        names.insert(0, "ToolSearch");
    }
    match wire {
        Wire::Responses => {
            let input = body["input"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let has_call = |name: &str| {
                input.iter().any(|item| {
                    item["type"] == "function_call" && item["name"].as_str() == Some(name)
                })
            };
            for name in &names {
                assert!(
                    has_call(name),
                    "the Responses resume must replay the {name} call: {body}"
                );
            }
            let has_output = |needle: &str| {
                input.iter().any(|item| {
                    matches!(
                        item["type"].as_str(),
                        Some("function_call_output" | "custom_tool_call_output")
                    ) && item["output"].to_string().contains(needle)
                })
            };
            assert!(
                has_output("broken"),
                "the Responses resume must replay the Read result: {body}"
            );
            assert!(
                has_output(CHECK_OUTPUT),
                "the Responses resume must replay the Bash result: {body}"
            );
            if search {
                assert!(
                    has_output(SEARCH_URL),
                    "the Responses resume must replay the WebSearch citation: {body}"
                );
            }
            for call in &calls.calls {
                assert!(
                    text.contains(&call.id),
                    "the Responses resume must replay tool id {:?}: {body}",
                    call.id
                );
            }
        }
        Wire::Chat => {
            let messages = body["messages"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let has_call = |name: &str| {
                messages.iter().any(|message| {
                    message["tool_calls"]
                        .as_array()
                        .map(|calls| {
                            calls
                                .iter()
                                .any(|call| call["function"]["name"].as_str() == Some(name))
                        })
                        .unwrap_or(false)
                })
            };
            for name in &names {
                assert!(
                    has_call(name),
                    "the Chat resume must replay the {name} call: {body}"
                );
            }
            let has_output = |needle: &str| {
                messages.iter().any(|message| {
                    message["role"] == "tool" && message["content"].to_string().contains(needle)
                })
            };
            assert!(
                has_output("broken"),
                "the Chat resume must replay the Read result: {body}"
            );
            assert!(
                has_output(CHECK_OUTPUT),
                "the Chat resume must replay the Bash result: {body}"
            );
            if search {
                assert!(
                    has_output(SEARCH_URL),
                    "the Chat resume must replay the WebSearch citation: {body}"
                );
            }
            for call in &calls.calls {
                assert!(
                    text.contains(&call.id),
                    "the Chat resume must replay tool id {:?}: {body}",
                    call.id
                );
            }
        }
        Wire::Anthropic => {
            let blocks = body["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter_map(|message| message["content"].as_array())
                        .flatten()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let has_use = |name: &str| {
                blocks
                    .iter()
                    .any(|block| block["type"] == "tool_use" && block["name"] == name)
            };
            for name in &names {
                assert!(
                    has_use(name),
                    "the Messages resume must replay the {name} tool_use: {body}"
                );
            }
            let has_result = |needle: &str| {
                blocks.iter().any(|block| {
                    block["type"] == "tool_result" && block.to_string().contains(needle)
                })
            };
            assert!(
                has_result("broken"),
                "the Messages resume must replay the Read result: {body}"
            );
            assert!(
                has_result(CHECK_OUTPUT),
                "the Messages resume must replay the Bash result: {body}"
            );
            if search {
                assert!(
                    has_result(SEARCH_URL),
                    "the Messages resume must replay the WebSearch citation: {body}"
                );
            }
            for call in &calls.calls {
                assert!(
                    blocks.iter().any(|block| {
                        block["type"] == "tool_use" && block["id"] == call.id.as_str()
                    }),
                    "the Messages resume must replay tool_use id {:?}: {body}",
                    call.id
                );
            }
        }
    }
}

/// Fails if the relay forwarded stock Claude Code's dedicated search follow-up
/// to the vendor instead of answering it locally.
#[cfg(feature = "test-brave")]
fn assert_no_dedicated_search_reached_vendor(requests: &[RecordedRequest]) {
    for (index, request) in requests.iter().enumerate() {
        let text = request.body.to_string();
        assert!(
            !text.contains(DEDICATED_SEARCH_PROMPT),
            "request {index} forwarded stock Claude Code's dedicated WebSearch follow-up to the \
             vendor (tool_choice {:?}); the relay must answer it locally with Brave: {}",
            request.body["tool_choice"],
            request.body
        );
        assert!(
            !text.contains("web_search_20250305"),
            "request {index} forwarded the Anthropic web_search server tool to the vendor: {}",
            request.body
        );
    }
}

/// The tool-result payload (wire shape only) that carried Brave's citations.
#[cfg(feature = "test-brave")]
fn search_tool_output(wire: Wire, body: &Value) -> String {
    match wire {
        Wire::Responses => body["input"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        matches!(
                            item["type"].as_str(),
                            Some("function_call_output" | "custom_tool_call_output")
                        )
                    })
                    .map(|item| item["output"].to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        Wire::Chat => body["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter(|message| message["role"] == "tool")
                    .map(|message| message["content"].to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        Wire::Anthropic => body["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| message["content"].as_array())
                    .flatten()
                    .filter(|block| block["type"] == "tool_result")
                    .map(|block| block["content"].to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
    }
}

#[cfg(feature = "test-brave")]
fn assert_search_citation_replayed(wire: Wire, body: &Value, label: &str) {
    let output = search_tool_output(wire, body);
    for needle in [
        "Links: [",
        SEARCH_URL,
        SEARCH_TITLE,
        SEARCH_SNIPPET,
        SEARCH_SECOND_URL,
    ] {
        assert!(
            output.contains(needle),
            "{label}: the tool result must replay Brave's {needle:?}; output:\n{output}"
        );
    }
}
