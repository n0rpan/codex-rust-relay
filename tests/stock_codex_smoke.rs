//! Opt-in end-to-end tests against the **installed stock Codex CLI**.
//!
//! These tests reproduce the loopback proof that exercised the relay with real
//! Codex 0.171.0: read a disposable file, edit it, run a check, resume after a
//! relay restart, and explicitly switch to a second vendor while preserving the
//! whole coding history.
//!
//! Twelve scenarios are covered, all opt-in. Eight switch vendors; four add
//! search. Four switch between two vendors on the same wire:
//!
//! * Responses wire, two Responses vendors.
//! * Chat Completions wire, translated by the relay, two vendors.
//! * Anthropic Messages wire with the old `2023-06-01` API version.
//! * Anthropic Messages wire with the current `2026-10-21` API version.
//!
//! Four switch wires, which is where the replay contracts differ:
//!
//! * Anthropic Messages to Chat Completions.
//! * Anthropic Messages to Responses.
//! * Responses to Anthropic Messages.
//! * Chat Completions to Anthropic Messages.
//!
//! `apply_patch` is exercised end to end through stock Codex itself wherever
//! the relay advertises it: the translated Chat/Anthropic catalogs do. The
//! Responses catalog deliberately keeps Codex's previous tool surface, so that
//! scenario edits with a plain `exec_command` (the shape used by the original
//! orchestrator proof) and delegates every file action to Codex.
//!
//! The cross-wire mocks also carry provider-native reasoning state: the
//! Anthropic vendor returns signed `thinking` blocks, and the Responses vendor
//! returns its own `encrypted_content`. Each scenario first proves that state is
//! replayed to the vendor that produced it, then asserts the other vendor never
//! receives it. The Anthropic-to-Responses case is covered too: the Responses
//! pass-through now strips a provider-tagged Anthropic thinking envelope before
//! forwarding the replayed history.
//!
//! Four search scenarios (built only with `--features test-brave`) add Codex
//! 0.171.0's standalone `web.run` tool to the same coding task on each wire:
//! Responses, Chat Completions, and both Anthropic Messages versions. Codex
//! calls the relay's loopback `/v1/alpha/search`, the relay queries a loopback
//! Brave fixture, and Codex reports the returned `turn0searchN` citation
//! results. Those scenarios also read, edit, test, restart the relay and resume
//! the same thread.
//!
//! Safety properties:
//!
//! * A fresh `HOME`, `CODEX_HOME`, TMPDIR and XDG tree per scenario; no user
//!   `auth.json` or `config.toml` is read.
//! * The stock CLI path is explicit, `CODEOX_TEST_CODEX_BIN`. Running the
//!   ignored tests without it fails with a clear message instead of silently
//!   skipping the integration.
//! * Only loopback sockets are used, and every proxy variable points at a dead
//!   loopback port so accidental public egress fails closed.
//! * Codex runs with `approval_policy = "never"`, `sandbox_mode =
//!   "workspace-write"`, `network_access = false`, `/tmp` excluded from the
//!   sandbox, and an empty inherited shell environment with only PATH set.
//!
//! The search scenarios need the relay binary built with the `test-brave`
//! feature: that feature is a test-only seam which lets the loopback test point
//! `BraveSearch` at its local Brave fixture via `CODEOX_TEST_BRAVE_BASE_URL`.
//! It is never enabled for a release build, and production configuration has no
//! Brave endpoint override.
//!
//! Run the offline portion with plain `cargo test --test stock_codex_smoke`.
//! Run the real integration manually; add `--features test-brave` to include
//! the four search scenarios:
//!
//! ```sh
//! CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 \
//!   cargo test --features test-brave --test stock_codex_smoke \
//!   -- --ignored --nocapture
//! ```
#![allow(dead_code)]

#[path = "../src/catalog.rs"]
mod catalog;
#[path = "../src/provider.rs"]
mod provider;
#[path = "../src/worker_placement.rs"]
mod worker_placement;

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

const PROMPT_INITIAL: &str = "Run the coding fixture.";
const PROMPT_RESUME: &str = "Resume the fixture.";
const PROMPT_SWITCH: &str = "Switch vendors explicitly.";
const FINAL_TEXT: &str = "Coding fixture passed.";
const RESUME_TEXT: &str = "Resumed with coding history.";
const SWITCH_TEXT: &str = "Switched providers with coding history.";
const CHECK_OUTPUT: &str = "CODEOX_CHECK_PASSED";
const SEARCH_PROMPT: &str = "Search the web for codeox, then run the coding fixture.";
const SEARCH_QUERY: &str = "codeox";
const SEARCH_REF: &str = "turn0search0";
const SEARCH_URL: &str = "https://www.rust-lang.org/learn";
const SEARCH_TITLE: &str = "Learn Rust";
const SEARCH_SNIPPET: &str = "The Rust Book is the primary learning resource.";
const BRAVE_ENV_KEY: &str = "BRAVE_SEARCH_API_KEY";
const BRAVE_FIXTURE_KEY: &str = "brave-fixture-key";
const TEST_BRAVE_BASE_ENV: &str = "CODEOX_TEST_BRAVE_BASE_URL";
const READ_CMD: &str = "cat fixture.txt";
const EDIT_CMD: &str = "printf '%s\\n' fixed > fixture.txt";
const CHECK_CMD: &str = "test \"$(cat fixture.txt)\" = fixed && printf '%s' CODEOX_CHECK_PASSED";
const PATCH: &str =
    "*** Begin Patch\n*** Update File: fixture.txt\n@@\n-broken\n+fixed\n*** End Patch\n";
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

// Provider-native reasoning state. The Anthropic fixture signs its thinking
// blocks and the Responses fixture returns encrypted reasoning; neither is
// valid at the other vendor, so a cross-wire switch must not replay it.
const ANTHROPIC_THINKING_TEXT: &str = "anthropic fixture private reasoning";
const ANTHROPIC_SIGNATURE: &str = "sig-fixture-anthropic-vendor-one";
const ANTHROPIC_ENVELOPE_PREFIX: &str = "codeox.anthropic.thinking.v1:";
const RESPONSES_REASONING_TEXT: &str = "responses fixture private reasoning";
const RESPONSES_CIPHERTEXT: &str = "enc-fixture-responses-vendor-one";

static CLI_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Offline tests (safe when the stock CLI is not installed)
// ---------------------------------------------------------------------------

#[test]
fn every_scenario_config_is_accepted_offline() {
    let scenarios = [
        same_wire(Wire::Responses, None, EditTool::ExecCommand),
        same_wire(Wire::Chat, None, EditTool::ApplyPatch),
        same_wire(Wire::Anthropic, Some("2023-06-01"), EditTool::ApplyPatch),
        same_wire(Wire::Anthropic, Some("2026-10-21"), EditTool::ApplyPatch),
        Scenario {
            primary: Endpoint {
                wire: Wire::Anthropic,
                anthropic_version: Some("2023-06-01"),
                edit_tool: EditTool::ApplyPatch,
                state: ProviderState::AnthropicThinking,
            },
            secondary: Endpoint {
                wire: Wire::Chat,
                anthropic_version: None,
                edit_tool: EditTool::ApplyPatch,
                state: ProviderState::None,
            },
        },
        Scenario {
            primary: Endpoint {
                wire: Wire::Anthropic,
                anthropic_version: Some("2023-06-01"),
                edit_tool: EditTool::ApplyPatch,
                state: ProviderState::AnthropicThinking,
            },
            secondary: Endpoint {
                wire: Wire::Responses,
                anthropic_version: None,
                edit_tool: EditTool::ApplyPatch,
                state: ProviderState::None,
            },
        },
        Scenario {
            primary: Endpoint {
                wire: Wire::Responses,
                anthropic_version: None,
                edit_tool: EditTool::ExecCommand,
                state: ProviderState::ResponsesNativeReasoning,
            },
            secondary: Endpoint {
                wire: Wire::Anthropic,
                anthropic_version: Some("2023-06-01"),
                edit_tool: EditTool::ExecCommand,
                state: ProviderState::None,
            },
        },
        Scenario {
            primary: Endpoint {
                wire: Wire::Chat,
                anthropic_version: None,
                edit_tool: EditTool::ApplyPatch,
                state: ProviderState::None,
            },
            secondary: Endpoint {
                wire: Wire::Anthropic,
                anthropic_version: Some("2023-06-01"),
                edit_tool: EditTool::ApplyPatch,
                state: ProviderState::None,
            },
        },
    ];
    for scenario in scenarios {
        let label = format!(
            "{} -> {}",
            scenario.primary.wire.as_str(),
            scenario.secondary.wire.as_str()
        );
        let raw = scenario_config(scenario, 41001, 41002);
        let config = provider::CodeoxConfig::load_from_str(&raw)
            .unwrap_or_else(|error| panic!("{label} config rejected: {error}"));
        assert_eq!(config.vendors.len(), 2, "{label}");
        for (vendor, endpoint) in config
            .vendors
            .iter()
            .zip([scenario.primary, scenario.secondary])
        {
            assert_eq!(vendor.wire.as_str(), endpoint.wire.as_str(), "{label}");
            if let Some(version) = endpoint.anthropic_version {
                assert_eq!(vendor.anthropic.version, version, "{label}");
            }
        }
        if scenario.primary.state == ProviderState::AnthropicThinking {
            assert_eq!(
                config.vendors[0].anthropic.thinking,
                provider::ThinkingMode::Manual,
                "{label}"
            );
            assert_eq!(config.vendors[0].max_output_tokens, Some(4096), "{label}");
        }
        assert_eq!(
            config.env_refs(),
            ["FIRST_KEY".to_owned(), "SECOND_KEY".to_owned()].into()
        );
    }
}

#[test]
fn search_scenario_config_is_accepted_offline() {
    for (port, endpoint) in [
        (
            41003,
            same_wire_endpoint(Wire::Responses, None, EditTool::ExecCommand),
        ),
        (
            41004,
            same_wire_endpoint(Wire::Chat, None, EditTool::ApplyPatch),
        ),
        (
            41005,
            same_wire_endpoint(Wire::Anthropic, Some("2023-06-01"), EditTool::ApplyPatch),
        ),
        (
            41006,
            same_wire_endpoint(Wire::Anthropic, Some("2026-10-21"), EditTool::ApplyPatch),
        ),
    ] {
        let raw = search_scenario_config(endpoint, port);
        let config = provider::CodeoxConfig::load_from_str(&raw).unwrap_or_else(|error| {
            panic!("{} search config rejected: {error}", endpoint.wire.as_str())
        });
        let search = config.search.as_ref().expect("search settings survive");
        assert_eq!(search.env_key, BRAVE_ENV_KEY);
        assert_eq!(
            config.env_refs(),
            [BRAVE_ENV_KEY.to_owned(), "FIRST_KEY".to_owned()].into()
        );
        assert!(
            !raw.contains(TEST_BRAVE_BASE_ENV),
            "the loopback Brave seam is an environment variable, never saved config"
        );
    }
}

#[test]
fn manual_suite_requires_an_explicit_cli_path() {
    let missing = stock_codex_binary_from(None).unwrap_err();
    assert!(
        missing.contains("CODEOX_TEST_CODEX_BIN"),
        "missing-binary error must name the variable: {missing}"
    );
    let absent =
        stock_codex_binary_from(Some(OsString::from("/nonexistent/stock/codex"))).unwrap_err();
    assert!(
        absent.contains("/nonexistent/stock/codex"),
        "missing file must be named: {absent}"
    );
}

// ---------------------------------------------------------------------------
// Opt-in end-to-end scenarios
// ---------------------------------------------------------------------------

#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
fn stock_codex_responses_edit_check_resume_and_switch() {
    run_scenario(same_wire(Wire::Responses, None, EditTool::ExecCommand));
}

#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
fn stock_codex_chat_completions_apply_patch_resume_and_switch() {
    run_scenario(same_wire(Wire::Chat, None, EditTool::ApplyPatch));
}

#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
fn stock_codex_anthropic_old_messages_apply_patch_resume_and_switch() {
    run_scenario(same_wire(
        Wire::Anthropic,
        Some("2023-06-01"),
        EditTool::ApplyPatch,
    ));
}

#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
fn stock_codex_anthropic_current_messages_apply_patch_resume_and_switch() {
    run_scenario(same_wire(
        Wire::Anthropic,
        Some("2026-10-21"),
        EditTool::ApplyPatch,
    ));
}

// Cross-wire switches. Stock Codex always speaks Responses to the relay; the
// relay rewrites the same replayed history into the second vendor's wire. The
// first vendor's native reasoning state must not be forwarded to the second.

#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
fn stock_codex_anthropic_to_chat_completions_explicit_switch() {
    run_scenario(Scenario {
        primary: Endpoint {
            wire: Wire::Anthropic,
            anthropic_version: Some("2023-06-01"),
            edit_tool: EditTool::ApplyPatch,
            state: ProviderState::AnthropicThinking,
        },
        secondary: Endpoint {
            wire: Wire::Chat,
            anthropic_version: None,
            edit_tool: EditTool::ApplyPatch,
            state: ProviderState::None,
        },
    });
}

#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
fn stock_codex_anthropic_to_responses_explicit_switch() {
    run_scenario(Scenario {
        primary: Endpoint {
            wire: Wire::Anthropic,
            anthropic_version: Some("2023-06-01"),
            edit_tool: EditTool::ApplyPatch,
            state: ProviderState::AnthropicThinking,
        },
        secondary: Endpoint {
            wire: Wire::Responses,
            anthropic_version: None,
            edit_tool: EditTool::ApplyPatch,
            state: ProviderState::None,
        },
    });
}

#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
fn stock_codex_responses_to_anthropic_explicit_switch() {
    run_scenario(Scenario {
        primary: Endpoint {
            wire: Wire::Responses,
            anthropic_version: None,
            edit_tool: EditTool::ExecCommand,
            state: ProviderState::ResponsesNativeReasoning,
        },
        secondary: Endpoint {
            wire: Wire::Anthropic,
            anthropic_version: Some("2023-06-01"),
            edit_tool: EditTool::ExecCommand,
            state: ProviderState::None,
        },
    });
}

#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
fn stock_codex_chat_completions_to_anthropic_explicit_switch() {
    run_scenario(Scenario {
        primary: Endpoint {
            wire: Wire::Chat,
            anthropic_version: None,
            edit_tool: EditTool::ApplyPatch,
            state: ProviderState::None,
        },
        secondary: Endpoint {
            wire: Wire::Anthropic,
            anthropic_version: Some("2023-06-01"),
            edit_tool: EditTool::ApplyPatch,
            state: ProviderState::None,
        },
    });
}

// Search + coding scenarios. Each one proves that stock Codex's own standalone
// `web.run` tool reaches the loopback Brave fixture, that Brave's citation
// results come back through Codex, and that the same thread still reads, edits,
// tests, restarts and resumes. The `test-brave` feature is the test-only seam
// that makes `BraveSearch` accept the fixture's loopback origin; these tests
// only exist when that feature is compiled in.

#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN and build with --features test-brave"]
fn stock_codex_responses_search_coding_restart_resume() {
    run_search_scenario(same_wire_endpoint(
        Wire::Responses,
        None,
        EditTool::ExecCommand,
    ));
}

#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN and build with --features test-brave"]
fn stock_codex_chat_completions_search_coding_restart_resume() {
    run_search_scenario(same_wire_endpoint(Wire::Chat, None, EditTool::ApplyPatch));
}

#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN and build with --features test-brave"]
fn stock_codex_anthropic_old_messages_search_coding_restart_resume() {
    run_search_scenario(same_wire_endpoint(
        Wire::Anthropic,
        Some("2023-06-01"),
        EditTool::ApplyPatch,
    ));
}

#[cfg(feature = "test-brave")]
#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN and build with --features test-brave"]
fn stock_codex_anthropic_current_messages_search_coding_restart_resume() {
    run_search_scenario(same_wire_endpoint(
        Wire::Anthropic,
        Some("2026-10-21"),
        EditTool::ApplyPatch,
    ));
}

// ---------------------------------------------------------------------------
// Scenario driver
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderState {
    None,
    /// Anthropic Messages vendor that asks for extended thinking and returns
    /// signed thinking blocks alongside its tool calls.
    AnthropicThinking,
    /// Responses vendor that returns its own encrypted reasoning item.
    ResponsesNativeReasoning,
}

impl ProviderState {
    fn foreign_markers(self) -> &'static [&'static str] {
        match self {
            Self::None => &[],
            Self::AnthropicThinking => &[ANTHROPIC_SIGNATURE, ANTHROPIC_ENVELOPE_PREFIX],
            Self::ResponsesNativeReasoning => &[RESPONSES_CIPHERTEXT],
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Endpoint {
    wire: Wire,
    anthropic_version: Option<&'static str>,
    edit_tool: EditTool,
    state: ProviderState,
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    primary: Endpoint,
    secondary: Endpoint,
}

fn same_wire(wire: Wire, anthropic_version: Option<&'static str>, edit_tool: EditTool) -> Scenario {
    Scenario {
        primary: same_wire_endpoint(wire, anthropic_version, edit_tool),
        secondary: same_wire_endpoint(wire, anthropic_version, edit_tool),
    }
}

fn same_wire_endpoint(
    wire: Wire,
    anthropic_version: Option<&'static str>,
    edit_tool: EditTool,
) -> Endpoint {
    Endpoint {
        wire,
        anthropic_version,
        edit_tool,
        state: ProviderState::None,
    }
}

fn run_scenario(scenario: Scenario) {
    let _guard = cli_lock();
    let primary_endpoint = scenario.primary;
    let secondary_endpoint = scenario.secondary;
    assert_eq!(
        primary_endpoint.wire == Wire::Responses,
        primary_endpoint.edit_tool == EditTool::ExecCommand,
        "the Responses catalog keeps Codex's previous tool surface; translated catalogs advertise apply_patch"
    );
    for (role, endpoint) in [
        ("primary", primary_endpoint),
        ("secondary", secondary_endpoint),
    ] {
        match endpoint.state {
            ProviderState::None => {}
            ProviderState::AnthropicThinking => assert_eq!(
                endpoint.wire,
                Wire::Anthropic,
                "{role}: thinking blocks are an Anthropic fixture"
            ),
            ProviderState::ResponsesNativeReasoning => assert_eq!(
                endpoint.wire,
                Wire::Responses,
                "{role}: native encrypted reasoning is a Responses fixture"
            ),
        }
    }
    assert_eq!(
        secondary_endpoint.state,
        ProviderState::None,
        "only the first vendor emits provider-native reasoning state"
    );

    let codex = stock_codex_binary();
    let version = codex_version(&codex);
    println!("Testing stock Codex: {version}");
    let relay = PathBuf::from(env!("CARGO_BIN_EXE_ox"));

    let label = format!(
        "{}-to-{}-{}",
        primary_endpoint.wire.as_str(),
        secondary_endpoint.wire.as_str(),
        primary_endpoint.anthropic_version.unwrap_or("none")
    );
    let isolation = TempTree::new(&label);
    let home = isolation.path.join("home");
    let codex_home = isolation.path.join("codex-home");
    let project = isolation.path.join("project");
    let tmp = isolation.path.join("tmp");
    for directory in [
        &home,
        &codex_home,
        &project,
        &tmp,
        &isolation.path.join("cache"),
        &isolation.path.join("config"),
        &isolation.path.join("data"),
        &isolation.path.join("bin"),
    ] {
        fs::create_dir_all(directory).expect("create isolation directory");
    }
    write_codex_shim(&isolation.path.join("bin"), &codex);

    let primary = spawn_fixture(FixtureSpec {
        wire: primary_endpoint.wire,
        upstream_model: "one-model",
        expected_key: "first-fixture-key",
        anthropic_version: primary_endpoint.anthropic_version,
        role: Role::Primary,
        edit_tool: primary_endpoint.edit_tool,
        state: primary_endpoint.state,
        search: false,
    });
    let secondary = spawn_fixture(FixtureSpec {
        wire: secondary_endpoint.wire,
        upstream_model: "two-model",
        expected_key: "second-fixture-key",
        anthropic_version: secondary_endpoint.anthropic_version,
        role: Role::Switch,
        edit_tool: secondary_endpoint.edit_tool,
        state: secondary_endpoint.state,
        search: false,
    });

    fs::write(
        codex_home.join("codeox.toml"),
        scenario_config(scenario, primary.port, secondary.port),
    )
    .expect("write codeox.toml");
    fs::write(codex_home.join("config.toml"), strict_codex_config()).expect("write config.toml");
    let fixture_file = project.join("fixture.txt");
    fs::write(&fixture_file, "broken\n").expect("write disposable fixture");

    let environment = scenario_env(&isolation.path, &home, &codex_home, &tmp);

    let initial = run_codex(
        &relay,
        &project,
        &environment,
        &[
            "--skip-git-repo-check",
            "--json",
            "-m",
            "first/one-model",
            PROMPT_INITIAL,
        ],
    );
    let initial_events = json_lines(&initial);
    let thread = thread_id(&initial_events);
    assert_full_coding_turn(&initial_events, primary_endpoint.edit_tool, FINAL_TEXT);
    assert_eq!(
        fs::read_to_string(&fixture_file).expect("read fixture after initial turn"),
        "fixed\n",
        "only Codex may edit the disposable file"
    );

    let resumed = run_codex(
        &relay,
        &project,
        &environment,
        &[
            "resume",
            "--skip-git-repo-check",
            "--json",
            "-m",
            "first/one-model",
            &thread,
            PROMPT_RESUME,
        ],
    );
    assert_text_turn(&json_lines(&resumed), RESUME_TEXT);

    let switched = run_codex(
        &relay,
        &project,
        &environment,
        &[
            "resume",
            "--skip-git-repo-check",
            "--json",
            "-m",
            "second/two-model",
            &thread,
            PROMPT_SWITCH,
        ],
    );
    assert_text_turn(&json_lines(&switched), SWITCH_TEXT);
    assert_eq!(
        fs::read_to_string(&fixture_file).expect("read fixture after switch"),
        "fixed\n",
        "resume and switch must not touch the workspace"
    );

    let primary_requests = primary.requests();
    let secondary_requests = secondary.requests();
    assert_eq!(
        primary_requests.len(),
        5,
        "four coding turns plus one resume request"
    );
    assert_eq!(secondary_requests.len(), 1, "one explicit vendor switch");

    // Tool-output stages on the first vendor: read, edit, check, final text,
    // then the resume request replaying all three completed tool outputs.
    for (index, (request, expected_stage)) in primary_requests
        .iter()
        .zip([0_usize, 1, 2, 3, 3])
        .enumerate()
    {
        assert_request_contract(
            primary_endpoint.wire,
            primary_endpoint.anthropic_version,
            request,
            "one-model",
            "first-fixture-key",
            expected_stage,
            index,
        );
    }
    assert_first_request_tools(primary_endpoint.wire, &primary_requests[0].body);
    if primary_endpoint.state == ProviderState::AnthropicThinking {
        assert_eq!(
            primary_requests[0].body["thinking"],
            json!({ "type": "enabled", "budget_tokens": 1024 }),
            "the thinking fixture must be requested from the Anthropic vendor"
        );
    }
    assert_history(
        primary_endpoint.wire,
        primary_endpoint.edit_tool,
        &primary_requests[4].body,
        PROMPT_INITIAL,
        "resume on first vendor",
    );
    match primary_endpoint.state {
        ProviderState::None => {}
        ProviderState::AnthropicThinking => {
            let blocks = anthropic_blocks(&primary_requests[4].body);
            assert!(
                blocks.iter().any(|block| {
                    block["type"] == "thinking" && block["signature"] == ANTHROPIC_SIGNATURE
                }),
                "the Anthropic fixture's signed thinking must be replayed to its own vendor"
            );
        }
        ProviderState::ResponsesNativeReasoning => assert!(
            primary_requests[4]
                .body
                .to_string()
                .contains(RESPONSES_CIPHERTEXT),
            "the Responses fixture's encrypted reasoning must be replayed to its own vendor"
        ),
    }
    assert!(
        primary_requests[4].body.to_string().contains(PROMPT_RESUME),
        "the resume prompt itself must reach the first vendor"
    );
    assert_eq!(
        primary_requests[4].stage, 3,
        "resume must replay the completed coding history"
    );

    assert_request_contract(
        secondary_endpoint.wire,
        secondary_endpoint.anthropic_version,
        &secondary_requests[0],
        "two-model",
        "second-fixture-key",
        3,
        0,
    );
    assert_history(
        secondary_endpoint.wire,
        primary_endpoint.edit_tool,
        &secondary_requests[0].body,
        PROMPT_INITIAL,
        "switch to second vendor",
    );
    let switch_body = secondary_requests[0].body.to_string();
    for needle in [PROMPT_RESUME, RESUME_TEXT, PROMPT_SWITCH] {
        assert!(
            switch_body.contains(needle),
            "the switch vendor must replay the earlier resume exchange; missing {needle:?}"
        );
    }
    assert_eq!(
        secondary_requests[0].stage, 3,
        "the second vendor must receive the same completed history"
    );
    assert_no_foreign_provider_state(
        secondary_endpoint.wire,
        &secondary_requests[0].body,
        primary_endpoint.state,
        "switch to second vendor",
    );
    if secondary_endpoint.wire == Wire::Anthropic {
        assert!(
            secondary_requests[0].body.get("thinking").is_none(),
            "a vendor configured with thinking off must not receive a thinking field"
        );
    }
    assert_secondary_completed(&secondary_requests[0].body, "switch to second vendor");

    let violations = primary
        .violations()
        .into_iter()
        .chain(secondary.violations())
        .collect::<Vec<_>>();
    assert!(
        violations.is_empty(),
        "the upstream fixture observed relay contract violations: {violations:#?}"
    );

    println!(
        "PASS {} -> {}: stock Codex read, edited ({:?}), checked, resumed after relay restart, \
         and explicitly switched vendors with full translated history preserved \
         ({} + {} upstream requests)",
        primary_endpoint.wire.as_str(),
        secondary_endpoint.wire.as_str(),
        primary_endpoint.edit_tool,
        primary_requests.len(),
        secondary_requests.len()
    );
}

/// One vendor, one real coding task, plus stock Codex's standalone `web.run`
/// tool and the relay's loopback Brave fixture. The two `codex exec` calls each
/// launch a fresh relay process, so the second call is a restart plus resume of
/// the same thread.
#[cfg(feature = "test-brave")]
fn run_search_scenario(endpoint: Endpoint) {
    let _guard = cli_lock();
    assert_eq!(endpoint.state, ProviderState::None);
    assert_eq!(
        endpoint.wire == Wire::Responses,
        endpoint.edit_tool == EditTool::ExecCommand,
        "the Responses catalog keeps Codex's previous tool surface; translated catalogs advertise apply_patch"
    );

    let codex = stock_codex_binary();
    let version = codex_version(&codex);
    println!("Testing stock Codex: {version}");
    let relay = PathBuf::from(env!("CARGO_BIN_EXE_ox"));

    let label = format!(
        "{}-search-{}",
        endpoint.wire.as_str(),
        endpoint.anthropic_version.unwrap_or("none")
    );
    let isolation = TempTree::new(&label);
    let home = isolation.path.join("home");
    let codex_home = isolation.path.join("codex-home");
    let project = isolation.path.join("project");
    let tmp = isolation.path.join("tmp");
    for directory in [
        &home,
        &codex_home,
        &project,
        &tmp,
        &isolation.path.join("cache"),
        &isolation.path.join("config"),
        &isolation.path.join("data"),
        &isolation.path.join("bin"),
    ] {
        fs::create_dir_all(directory).expect("create isolation directory");
    }
    write_codex_shim(&isolation.path.join("bin"), &codex);

    let primary = spawn_fixture(FixtureSpec {
        wire: endpoint.wire,
        upstream_model: "one-model",
        expected_key: "first-fixture-key",
        anthropic_version: endpoint.anthropic_version,
        role: Role::Primary,
        edit_tool: endpoint.edit_tool,
        state: ProviderState::None,
        search: true,
    });
    let brave = spawn_brave_fixture();

    fs::write(
        codex_home.join("codeox.toml"),
        search_scenario_config(endpoint, primary.port),
    )
    .expect("write codeox.toml");
    fs::write(codex_home.join("config.toml"), strict_codex_config()).expect("write config.toml");
    let fixture_file = project.join("fixture.txt");
    fs::write(&fixture_file, "broken\n").expect("write disposable fixture");

    let mut environment = scenario_env(&isolation.path, &home, &codex_home, &tmp);
    environment.push((BRAVE_ENV_KEY.to_owned(), BRAVE_FIXTURE_KEY.to_owned()));
    environment.push((
        TEST_BRAVE_BASE_ENV.to_owned(),
        format!("http://127.0.0.1:{}", brave.port),
    ));

    let initial = run_codex_with_web_search(
        &relay,
        &project,
        &environment,
        "live",
        &[
            "--skip-git-repo-check",
            "--json",
            "-m",
            "first/one-model",
            SEARCH_PROMPT,
        ],
    );
    let initial_events = json_lines(&initial);
    let thread = thread_id(&initial_events);
    assert_full_coding_turn(&initial_events, endpoint.edit_tool, FINAL_TEXT);
    assert_search_completed(&initial_events);
    assert_eq!(
        fs::read_to_string(&fixture_file).expect("read fixture after initial turn"),
        "fixed\n",
        "only Codex may edit the disposable file"
    );

    // A new relay process, a new loopback token, and a resumed Codex thread.
    let resumed = run_codex_with_web_search(
        &relay,
        &project,
        &environment,
        "live",
        &[
            "resume",
            "--skip-git-repo-check",
            "--json",
            "-m",
            "first/one-model",
            &thread,
            PROMPT_RESUME,
        ],
    );
    assert_text_turn(&json_lines(&resumed), RESUME_TEXT);
    assert_eq!(
        fs::read_to_string(&fixture_file).expect("read fixture after resume"),
        "fixed\n",
        "resume must not touch the workspace"
    );

    let requests = primary.requests();
    assert_eq!(
        requests.len(),
        6,
        "web.run, read, edit, test and final text, then one resume request"
    );
    for (index, (request, expected_stage)) in
        requests.iter().zip([0_usize, 1, 2, 3, 4, 4]).enumerate()
    {
        assert_request_contract(
            endpoint.wire,
            endpoint.anthropic_version,
            request,
            "one-model",
            "first-fixture-key",
            expected_stage,
            index,
        );
    }
    assert_first_request_tools(endpoint.wire, &requests[0].body);
    assert_search_tools(endpoint.wire, &requests[0].body);
    assert_search_result_replayed(
        endpoint.wire,
        &requests[1].body,
        "the request after web.run",
    );
    assert!(
        requests[5].body.to_string().contains(PROMPT_RESUME),
        "the resume prompt itself must reach the vendor"
    );
    assert_eq!(
        requests[5].stage, 4,
        "resume must replay the completed coding history"
    );
    assert_search_call_in_history(endpoint.wire, &requests[5].body, "resume after restart");
    assert_search_result_replayed(endpoint.wire, &requests[5].body, "resume after restart");
    assert_history(
        endpoint.wire,
        endpoint.edit_tool,
        &requests[5].body,
        SEARCH_PROMPT,
        "resume after restart",
    );

    let hits = brave.hits();
    assert_eq!(
        hits.len(),
        1,
        "stock Codex must make exactly one direct Brave search: {hits:#?}"
    );
    assert_eq!(hits[0].path, "/res/v1/llm/context");
    assert_eq!(hits[0].token.as_deref(), Some(BRAVE_FIXTURE_KEY));
    assert!(
        hits[0]
            .query
            .contains(&("q".to_owned(), SEARCH_QUERY.to_owned())),
        "Brave must receive Codex's query: {:?}",
        hits[0].query
    );
    let violations = primary
        .violations()
        .into_iter()
        .chain(brave.violations())
        .collect::<Vec<_>>();
    assert!(
        violations.is_empty(),
        "the loopback fixtures observed relay contract violations: {violations:#?}"
    );

    println!(
        "PASS {} search coding: stock Codex called web.run, received Brave citations, read, \
         edited ({:?}), checked, restarted the relay and resumed ({} upstream requests, 1 Brave request)",
        endpoint.wire.as_str(),
        endpoint.edit_tool,
        requests.len()
    );
}

fn cli_lock() -> MutexGuard<'static, ()> {
    CLI_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stock_codex_binary() -> PathBuf {
    stock_codex_binary_from(env::var_os("CODEOX_TEST_CODEX_BIN"))
        .unwrap_or_else(|error| panic!("{error}"))
}

fn stock_codex_binary_from(raw: Option<OsString>) -> Result<PathBuf, String> {
    let Some(raw) = raw else {
        return Err(
            "CODEOX_TEST_CODEX_BIN is not set. These tests require the real installed stock \
             Codex CLI; set CODEOX_TEST_CODEX_BIN to e.g. \
             /home/$USER/.local/share/mise/installs/codex/0.171.0/bin/codex"
                .to_owned(),
        );
    };
    let path = PathBuf::from(raw);
    if !path.is_file() {
        return Err(format!(
            "CODEOX_TEST_CODEX_BIN points at {:?}, which is not a file",
            path
        ));
    }
    Ok(path)
}

fn codex_version(codex: &Path) -> String {
    let output = Command::new(codex)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("could not run {codex:?} --version: {error}"));
    assert!(
        output.status.success(),
        "{codex:?} --version failed: {}",
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

/// The relay launches `codex` by name. Put the explicitly configured stock
/// binary first on the child's PATH through a loopback shim directory so no
/// ambient Codex installation can be picked up by accident.
#[cfg(unix)]
fn write_codex_shim(bin: &Path, codex: &Path) {
    let shim = bin.join("codex");
    std::os::unix::fs::symlink(codex, &shim)
        .unwrap_or_else(|error| panic!("could not symlink {shim:?} -> {codex:?}: {error}"));
}

#[cfg(not(unix))]
compile_error!("the stock Codex smoke suite currently supports Unix hosts only");

// ---------------------------------------------------------------------------
// Isolation and configuration
// ---------------------------------------------------------------------------

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
            "codeox-stock-smoke-{label}-{}-{nonce}",
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

fn strict_codex_config() -> &'static str {
    r#"approval_policy = "never"
sandbox_mode = "workspace-write"

[sandbox_workspace_write]
network_access = false
exclude_slash_tmp = true
exclude_tmpdir_env_var = true

[shell_environment_policy]
inherit = "none"

[shell_environment_policy.set]
PATH = "/usr/bin:/bin"
"#
}

fn scenario_config(scenario: Scenario, first_port: u16, second_port: u16) -> String {
    format!(
        "{}{}",
        vendor_config(
            "first",
            "First Fixture Vendor",
            "FIRST_KEY",
            first_port,
            "one-model",
            "First Fixture Model",
            scenario.primary,
        ),
        vendor_config(
            "second",
            "Second Fixture Vendor",
            "SECOND_KEY",
            second_port,
            "two-model",
            "Second Fixture Model",
            scenario.secondary,
        )
    )
}

fn search_scenario_config(endpoint: Endpoint, port: u16) -> String {
    format!(
        "{}[search]\nenv_key = \"{BRAVE_ENV_KEY}\"\n",
        vendor_config(
            "first",
            "First Fixture Vendor",
            "FIRST_KEY",
            port,
            "one-model",
            "First Fixture Model",
            endpoint,
        )
    )
}

fn vendor_config(
    key: &str,
    name: &str,
    env_key: &str,
    port: u16,
    model_id: &str,
    model_name: &str,
    endpoint: Endpoint,
) -> String {
    let anthropic = endpoint
        .anthropic_version
        .map(|version| format!("anthropic_version = \"{version}\"\n"))
        .unwrap_or_default();
    let thinking = match endpoint.state {
        ProviderState::AnthropicThinking => {
            "anthropic_thinking = \"manual\"\nanthropic_thinking_budget_tokens = 1024\nmax_output_tokens = 4096\n"
        }
        ProviderState::None | ProviderState::ResponsesNativeReasoning => "",
    };
    format!(
        r#"[providers.{key}]
name = "{name}"
base_url = "http://127.0.0.1:{port}/v1"
env_key = "{env_key}"
wire_api = "{wire}"
discovery = false
{thinking}{anthropic}
[[providers.{key}.models]]
id = "{model_id}"
name = "{model_name}"
context_window = 131072
input_modalities = ["text"]
reasoning_efforts = ["low", "medium", "high"]

"#,
        wire = endpoint.wire.as_str(),
    )
}

/// A minimal environment. Nothing from the invoking user's shell is inherited:
/// no real provider keys, no CODEX_* state, no user auth or config discovery.
fn scenario_env(root: &Path, home: &Path, codex_home: &Path, tmp: &Path) -> Vec<(String, String)> {
    let dead_proxy = "http://127.0.0.1:1";
    [
        (
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        ),
        ("HOME", home.display().to_string()),
        ("CODEX_HOME", codex_home.display().to_string()),
        ("FIRST_KEY", "first-fixture-key".to_owned()),
        ("SECOND_KEY", "second-fixture-key".to_owned()),
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
        // Loopback only: everything else is sent to a closed loopback port.
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
// Running the relay with stock Codex
// ---------------------------------------------------------------------------

struct RunOutput {
    stdout: String,
}

fn run_codex(
    relay: &Path,
    project: &Path,
    environment: &[(String, String)],
    args: &[&str],
) -> RunOutput {
    run_codex_with_web_search(relay, project, environment, "disabled", args)
}

/// The coding scenarios pin hosted web search off. The search scenarios opt in
/// to Codex's live policy so the relay advertises its standalone `web.run`
/// tool, which the relay answers from its own loopback Brave client.
fn run_codex_with_web_search(
    relay: &Path,
    project: &Path,
    environment: &[(String, String)],
    web_search: &str,
    args: &[&str],
) -> RunOutput {
    let mut command = Command::new(relay);
    command
        .arg("codex")
        .args([
            "--no-daemon",
            "--disable",
            "plugins",
            "--disable",
            "hooks",
            "-c",
        ])
        .arg(format!("web_search={web_search:?}"))
        .arg("exec")
        .args(args)
        .current_dir(project)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    let label = format!("ox codex exec {args:?}");
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
        match child.try_wait().expect("wait for codex") {
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
    RunOutput { stdout }
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

// ---------------------------------------------------------------------------
// Stock CLI output assertions
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditTool {
    /// Responses vendors keep Codex's previous tool surface; the fixture asks
    /// for a plain `exec_command` write, exactly as the orchestrator proof did.
    ExecCommand,
    /// Translated vendors advertise the freeform `apply_patch` tool, which the
    /// adapter exposes as a function and maps back to `custom_tool_call`.
    ApplyPatch,
}

fn json_lines(output: &RunOutput) -> Vec<Value> {
    output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("non-JSON line {line:?}: {error}"))
        })
        .collect()
}

fn thread_id(events: &[Value]) -> String {
    events
        .iter()
        .find(|event| event["type"] == "thread.started")
        .and_then(|event| event["thread_id"].as_str())
        .expect("thread.started with a thread_id")
        .to_owned()
}

fn completed_items(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["type"] == "item.completed")
        .map(|event| &event["item"])
        .collect()
}

fn assert_full_coding_turn(events: &[Value], edit_tool: EditTool, final_text: &str) {
    assert!(
        events.iter().any(|event| event["type"] == "turn.completed"),
        "missing turn.completed: {events:#?}"
    );
    let items = completed_items(events);

    let read = items
        .iter()
        .find(|item| {
            item["type"] == "command_execution"
                && item["command"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(READ_CMD)
        })
        .unwrap_or_else(|| panic!("no read command in {items:#?}"));
    assert_eq!(read["exit_code"], 0, "read command failed: {read}");
    assert_eq!(
        read["aggregated_output"], "broken\n",
        "Codex must read the disposable file: {read}"
    );

    let check = items
        .iter()
        .find(|item| {
            item["type"] == "command_execution"
                && item["command"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(CHECK_OUTPUT)
        })
        .unwrap_or_else(|| panic!("no check command in {items:#?}"));
    assert_eq!(check["exit_code"], 0, "check command failed: {check}");
    assert!(
        check["aggregated_output"]
            .as_str()
            .unwrap_or_default()
            .contains(CHECK_OUTPUT),
        "check output must prove the edit: {check}"
    );

    match edit_tool {
        EditTool::ApplyPatch => {
            let changed = items
                .iter()
                .find(|item| item["type"] == "file_change")
                .unwrap_or_else(|| panic!("Codex did not report a file_change: {items:#?}"));
            let changes = changed["changes"].as_array().expect("changes array");
            assert!(
                changes.iter().any(|change| {
                    change["path"]
                        .as_str()
                        .unwrap_or_default()
                        .ends_with("fixture.txt")
                        && change["kind"] == "update"
                }),
                "apply_patch must update fixture.txt: {changed}"
            );
        }
        EditTool::ExecCommand => {
            let edit = items
                .iter()
                .find(|item| {
                    item["type"] == "command_execution"
                        && item["command"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("printf")
                        && item["command"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("> fixture.txt")
                })
                .unwrap_or_else(|| panic!("no edit command in {items:#?}"));
            assert_eq!(edit["exit_code"], 0, "edit command failed: {edit}");
        }
    }

    let message = items
        .iter()
        .rev()
        .find(|item| item["type"] == "agent_message")
        .unwrap_or_else(|| panic!("no agent message in {items:#?}"));
    assert_eq!(message["text"], final_text, "unexpected final message");
}

fn assert_text_turn(events: &[Value], expected: &str) {
    assert!(
        events.iter().any(|event| event["type"] == "turn.completed"),
        "missing turn.completed: {events:#?}"
    );
    let items = completed_items(events);
    assert!(
        items
            .iter()
            .any(|item| item["type"] == "agent_message" && item["text"] == expected),
        "missing agent message {expected:?}: {items:#?}"
    );
}

// ---------------------------------------------------------------------------
// Upstream fixture server: loopback only, records every request
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Primary,
    Switch,
}

#[derive(Clone, Copy)]
struct FixtureSpec {
    wire: Wire,
    upstream_model: &'static str,
    expected_key: &'static str,
    anthropic_version: Option<&'static str>,
    role: Role,
    edit_tool: EditTool,
    state: ProviderState,
    /// The fixture first asks Codex's standalone `web.run` tool to search,
    /// shifting read/edit/test/final by one stage.
    search: bool,
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

// ---------------------------------------------------------------------------
// Loopback Brave fixture
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct BraveHit {
    path: String,
    query: Vec<(String, String)>,
    token: Option<String>,
}

struct BraveFixture {
    port: u16,
    hits: Arc<Mutex<Vec<BraveHit>>>,
    violations: Arc<Mutex<Vec<String>>>,
}

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

fn query_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(name), percent_decode(value))
        })
        .collect()
}

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

fn spawn_fixture(spec: FixtureSpec) -> Fixture {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let violations = Arc::new(Mutex::new(Vec::new()));
    let thread_requests = Arc::clone(&requests);
    let thread_violations = Arc::clone(&violations);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            if let Err(error) =
                handle_connection(stream, spec, &thread_requests, &thread_violations)
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
                "owned_by": "codeox-fixture"
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
            format!("unexpected {method} {path} from the relay"),
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

    let events = response_events(&spec, &body, stage);
    let data = encode_stream(spec.wire, &events);
    respond(&mut stream, 200, "text/event-stream", &data)
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
    let model = body["model"].as_str().unwrap_or_default();
    if model != spec.upstream_model {
        record_violation(
            violations,
            format!(
                "upstream model was {model:?}, expected {:?}; the relay must rewrite the alias",
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
            if version >= "2026-10-21" {
                for field in ["temperature", "top_p", "top_k"] {
                    if body.get(field).is_some() {
                        record_violation(
                            violations,
                            format!("{version} removes sampling field {field}, but it was sent"),
                        );
                    }
                }
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
    if spec.role == Role::Switch {
        return text_events(spec.wire, spec.upstream_model, SWITCH_TEXT);
    }
    if body.to_string().contains(PROMPT_RESUME) {
        return text_events(spec.wire, spec.upstream_model, RESUME_TEXT);
    }
    // With search enabled the first request asks for `web.run`; Codex executes
    // it against the relay's own `/v1/alpha/search`, and the search result is
    // the first tool output counted in `stage`.
    let (read, edit, test, final_text) = if spec.search {
        if stage == 0 {
            return web_search_events(spec);
        }
        // Re-numbered by hand so the match below stays one table.
        (1, 2, 3, 4)
    } else {
        (0, 1, 2, 3)
    };
    if stage == read {
        return exec_events(spec, "call_read", READ_CMD, false);
    }
    if stage == edit {
        return match spec.edit_tool {
            EditTool::ApplyPatch => apply_patch_events(spec),
            EditTool::ExecCommand => exec_events(spec, "call_edit", EDIT_CMD, true),
        };
    }
    if stage == test {
        return exec_events(spec, "call_test", CHECK_CMD, false);
    }
    if stage == final_text {
        return text_events(spec.wire, spec.upstream_model, FINAL_TEXT);
    }
    // The driver asserts the exact stage sequence, so this is only a fallback
    // that lets the child exit instead of hanging. It is not evidence of
    // success: the stage assertion fails the test.
    eprintln!(
        "warning: fixture saw unexpected stage {stage} for {}",
        spec.wire.as_str()
    );
    text_events(spec.wire, spec.upstream_model, FINAL_TEXT)
}

fn exec_events(spec: &FixtureSpec, call_id: &str, command: &str, with_state: bool) -> Vec<Value> {
    let arguments = json!({ "cmd": command }).to_string();
    tool_events(spec, call_id, "exec_command", &arguments, with_state)
}

fn web_search_events(spec: &FixtureSpec) -> Vec<Value> {
    let arguments = json!({ "search_query": [{ "q": SEARCH_QUERY }] }).to_string();
    match spec.wire {
        // Codex's native tool is the `web` namespace's `run` function. A
        // translated adapter exposes it upstream as one flat function and maps
        // the streamed call back into that namespace.
        Wire::Responses => {
            responses_tool_call_namespaced(spec.upstream_model, "call_w1", "web", "run", &arguments)
        }
        Wire::Chat => chat_tool_call(spec.upstream_model, "call_w1", "web__run", &arguments),
        Wire::Anthropic => {
            anthropic_tool_call(spec.upstream_model, "call_w1", "web__run", &arguments)
        }
    }
}

fn apply_patch_events(spec: &FixtureSpec) -> Vec<Value> {
    let arguments = json!({ "input": PATCH }).to_string();
    // The translated catalog advertises apply_patch, so upstream sees a
    // normal function tool; the adapter maps it back to a custom item.
    if spec.wire == Wire::Responses {
        unreachable!("the Responses catalog does not advertise apply_patch");
    }
    tool_events(spec, "call_patch", "apply_patch", &arguments, true)
}

fn tool_events(
    spec: &FixtureSpec,
    call_id: &str,
    name: &str,
    arguments: &str,
    with_state: bool,
) -> Vec<Value> {
    match (spec.wire, with_state.then_some(spec.state)) {
        (Wire::Responses, Some(ProviderState::ResponsesNativeReasoning)) => {
            responses_tool_call_with_native_reasoning(spec.upstream_model, call_id, name, arguments)
        }
        (Wire::Responses, _) => responses_tool_call(spec.upstream_model, call_id, name, arguments),
        (Wire::Chat, _) => chat_tool_call(spec.upstream_model, call_id, name, arguments),
        (Wire::Anthropic, Some(ProviderState::AnthropicThinking)) => {
            anthropic_tool_call_with_thinking(spec.upstream_model, call_id, name, arguments)
        }
        (Wire::Anthropic, _) => anthropic_tool_call(spec.upstream_model, call_id, name, arguments),
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
// Stock 0.171.0 wire shapes observed from the installed CLI
// ---------------------------------------------------------------------------

fn responses_object(model: &str, status: &str, output: Vec<Value>) -> Value {
    json!({
        "id": "resp_stock_smoke",
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
    responses_tool_call_with(model, call_id, name, arguments, None, None)
}

fn responses_tool_call_namespaced(
    model: &str,
    call_id: &str,
    namespace: &str,
    name: &str,
    arguments: &str,
) -> Vec<Value> {
    responses_tool_call_with(model, call_id, name, arguments, None, Some(namespace))
}

/// A Responses vendor's own reasoning item, including opaque ciphertext. A
/// different vendor must never receive this item on a later switch.
fn responses_tool_call_with_native_reasoning(
    model: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Vec<Value> {
    let reasoning = json!({
        "id": "rs_native_fixture",
        "type": "reasoning",
        "status": "completed",
        "summary": [{ "type": "summary_text", "text": RESPONSES_REASONING_TEXT }],
        "encrypted_content": RESPONSES_CIPHERTEXT
    });
    responses_tool_call_with(model, call_id, name, arguments, Some(reasoning), None)
}

fn responses_tool_call_with(
    model: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
    reasoning: Option<Value>,
    namespace: Option<&str>,
) -> Vec<Value> {
    let item_id = format!("fc_{call_id}");
    let mut item = json!({
        "id": item_id,
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "arguments": arguments
    });
    if let Some(namespace) = namespace {
        item["namespace"] = json!(namespace);
    }
    let mut events = vec![
        json!({ "type": "response.created", "response": responses_object(model, "in_progress", vec![]) }),
        json!({ "type": "response.in_progress", "response": responses_object(model, "in_progress", vec![]) }),
    ];
    let mut output = Vec::new();
    let mut output_index = 0;
    if let Some(reasoning) = reasoning {
        let reasoning_id = reasoning["id"].as_str().expect("reasoning id").to_owned();
        events.push(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": reasoning_id,
                "type": "reasoning",
                "status": "in_progress",
                "summary": []
            }
        }));
        events.push(json!({
            "type": "response.reasoning_summary_part.added",
            "item_id": reasoning_id,
            "output_index": 0,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": "" }
        }));
        events.push(json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": reasoning_id,
            "output_index": 0,
            "summary_index": 0,
            "delta": RESPONSES_REASONING_TEXT
        }));
        events.push(json!({
            "type": "response.reasoning_summary_text.done",
            "item_id": reasoning_id,
            "output_index": 0,
            "summary_index": 0,
            "text": RESPONSES_REASONING_TEXT
        }));
        events.push(json!({
            "type": "response.reasoning_summary_part.done",
            "item_id": reasoning_id,
            "output_index": 0,
            "summary_index": 0,
            "part": { "type": "summary_text", "text": RESPONSES_REASONING_TEXT }
        }));
        events.push(json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": reasoning.clone()
        }));
        output.push(reasoning);
        output_index = 1;
    }
    let mut added = json!({
        "id": item_id,
        "type": "function_call",
        "status": "in_progress",
        "call_id": call_id,
        "name": name,
        "arguments": ""
    });
    if let Some(namespace) = namespace {
        added["namespace"] = json!(namespace);
    }
    events.push(json!({
        "type": "response.output_item.added",
        "output_index": output_index,
        "item": added
    }));
    events.push(json!({
        "type": "response.function_call_arguments.delta",
        "item_id": item_id,
        "output_index": output_index,
        "delta": arguments
    }));
    events.push(json!({
        "type": "response.function_call_arguments.done",
        "item_id": item_id,
        "output_index": output_index,
        "arguments": arguments
    }));
    events.push(json!({
        "type": "response.output_item.done",
        "output_index": output_index,
        "item": item
    }));
    output.push(item);
    events.push(json!({
        "type": "response.completed",
        "response": {
            "id": "resp_stock_smoke",
            "object": "response",
            "created_at": 1_700_000_000_u64,
            "status": "completed",
            "model": model,
            "output": output,
            "usage": responses_usage(11, 7)
        }
    }));
    events
}

fn responses_text(model: &str, text: &str) -> Vec<Value> {
    let item = json!({
        "id": "msg_stock_smoke",
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
            "item": { "id": "msg_stock_smoke", "type": "message", "role": "assistant", "status": "in_progress", "content": [] }
        }),
        json!({
            "type": "response.content_part.added",
            "item_id": "msg_stock_smoke",
            "output_index": 0,
            "content_index": 0,
            "part": { "type": "output_text", "text": "", "annotations": [] }
        }),
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_stock_smoke",
            "output_index": 0,
            "content_index": 0,
            "delta": text
        }),
        json!({
            "type": "response.output_text.done",
            "item_id": "msg_stock_smoke",
            "output_index": 0,
            "content_index": 0,
            "text": text
        }),
        json!({
            "type": "response.content_part.done",
            "item_id": "msg_stock_smoke",
            "output_index": 0,
            "content_index": 0,
            "part": { "type": "output_text", "text": text, "annotations": [] }
        }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_stock_smoke",
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
        "id": "chatcmpl_stock_smoke",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000_u64,
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
    })
}

fn chat_usage(model: &str) -> Value {
    json!({
        "id": "chatcmpl_stock_smoke",
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
            "id": "msg_stock_smoke",
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

/// A signed thinking block followed by the tool call. Anthropic binds the
/// signature to this exact assistant turn and rejects it at another vendor.
fn anthropic_tool_call_with_thinking(
    model: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Vec<Value> {
    vec![
        anthropic_message_start(model),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "thinking", "thinking": "", "signature": "" }
        }),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "thinking_delta", "thinking": ANTHROPIC_THINKING_TEXT }
        }),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "signature_delta", "signature": ANTHROPIC_SIGNATURE }
        }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": { "type": "tool_use", "id": call_id, "name": name, "input": {} }
        }),
        json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "input_json_delta", "partial_json": arguments }
        }),
        json!({ "type": "content_block_stop", "index": 1 }),
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
// Request contract and full-history assertions
// ---------------------------------------------------------------------------

fn assert_request_contract(
    wire: Wire,
    anthropic_version: Option<&str>,
    request: &RecordedRequest,
    upstream_model: &str,
    expected_key: &str,
    expected_stage: usize,
    index: usize,
) {
    let label = format!("{} request {index}", wire.as_str());
    assert_eq!(request.path, wire.endpoint(), "{label}: upstream endpoint");
    assert_eq!(
        request.body["model"], upstream_model,
        "{label}: the alias must be rewritten to the vendor model id"
    );
    assert_eq!(request.stage, expected_stage, "{label}: tool-output stage");
    match wire {
        Wire::Responses | Wire::Chat => assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {expected_key}").as_str()),
            "{label}: the vendor's own bearer key is used"
        ),
        Wire::Anthropic => {
            assert_eq!(
                request.header("x-api-key"),
                Some(expected_key),
                "{label}: the vendor's own x-api-key is used"
            );
            assert!(
                request.header("authorization").is_none(),
                "{label}: Anthropic Messages must not send Authorization"
            );
            let version = anthropic_version.unwrap_or("2023-06-01");
            assert_eq!(
                request.header("anthropic-version"),
                Some(version),
                "{label}: configured Messages version reaches the vendor"
            );
            if version >= "2026-10-21" {
                for field in ["temperature", "top_p", "top_k"] {
                    assert!(
                        request.body.get(field).is_none(),
                        "{label}: {version} drops sampling field {field}"
                    );
                }
            }
        }
    }
}

fn assert_first_request_tools(wire: Wire, body: &Value) {
    if wire == Wire::Responses {
        // The Responses catalog intentionally keeps Codex's previous tool
        // surface, so apply_patch is not asserted there.
        return;
    }
    let names = body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| match wire {
                    Wire::Chat => tool["function"]["name"].as_str(),
                    Wire::Anthropic => tool["name"].as_str(),
                    Wire::Responses => tool["name"].as_str(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        names.contains(&"apply_patch"),
        "{wire:?}: translated catalogs must advertise apply_patch; upstream tools were {names:?}"
    );
}

fn responses_input(body: &Value) -> &[Value] {
    body["input"].as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn responses_output_contains(input: &[Value], needle: &str) -> bool {
    input.iter().any(|item| {
        matches!(
            item["type"].as_str(),
            Some("function_call_output" | "custom_tool_call_output")
        ) && item["output"].to_string().contains(needle)
    })
}

fn chat_messages(body: &Value) -> &[Value] {
    body["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn chat_tool_output_contains(messages: &[Value], needle: &str) -> bool {
    messages
        .iter()
        .any(|message| message["role"] == "tool" && message["content"].to_string().contains(needle))
}

fn chat_has_tool_call(messages: &[Value], name: &str) -> bool {
    messages.iter().any(|message| {
        message["tool_calls"]
            .as_array()
            .map(|calls| {
                calls.iter().any(|call| {
                    call["function"]["name"].as_str() == Some(name)
                        && call["function"]["arguments"]
                            .as_str()
                            .unwrap_or_default()
                            .contains(if name == "apply_patch" {
                                "*** Begin Patch"
                            } else {
                                ""
                            })
                })
            })
            .unwrap_or(false)
    })
}

fn anthropic_blocks(body: &Value) -> Vec<&Value> {
    body["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message["content"].as_array())
                .flatten()
                .collect()
        })
        .unwrap_or_default()
}

fn anthropic_has_tool_use(blocks: &[&Value], name: &str) -> bool {
    blocks
        .iter()
        .any(|block| block["type"] == "tool_use" && block["name"] == name)
}

fn anthropic_results_contain(blocks: &[&Value], needle: &str) -> bool {
    blocks
        .iter()
        .any(|block| block["type"] == "tool_result" && block.to_string().contains(needle))
}

fn anthropic_text_contains(blocks: &[&Value], needle: &str) -> bool {
    blocks
        .iter()
        .any(|block| block["type"] == "text" && block["text"].to_string().contains(needle))
}

/// A vendor may only see opaque reasoning state it produced. When Codex
/// explicitly switches wires, the second vendor must receive the visible
/// conversation (prompts, calls, results, final text) without the first
/// vendor's reasoning items, thinking signatures or ciphertext.
fn assert_no_foreign_provider_state(target: Wire, body: &Value, state: ProviderState, label: &str) {
    let text = body.to_string();
    for marker in state.foreign_markers() {
        assert!(
            !text.contains(marker),
            "{label}: {} received the first vendor's opaque state marker {marker:?}",
            target.as_str()
        );
    }
    if state == ProviderState::None {
        return;
    }
    assert!(
        !contains_encrypted_content(body),
        "{label}: {} received an encrypted_content field from another vendor",
        target.as_str()
    );
    match target {
        Wire::Responses => assert!(
            responses_input(body)
                .iter()
                .all(|item| item["type"] != "reasoning"),
            "{label}: the second Responses vendor received the first vendor's reasoning item"
        ),
        Wire::Chat => assert!(
            chat_messages(body)
                .iter()
                .all(|message| !message.to_string().contains("reasoning")),
            "{label}: the second Chat vendor received provider-native reasoning"
        ),
        Wire::Anthropic => {
            let blocks = anthropic_blocks(body);
            assert!(
                blocks.iter().all(|block| !matches!(
                    block["type"].as_str(),
                    Some("thinking" | "redacted_thinking")
                )),
                "{label}: the second Anthropic vendor received foreign thinking blocks"
            );
        }
    }
}

fn contains_encrypted_content(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields
                .get("encrypted_content")
                .is_some_and(|value| !value.is_null())
                || fields.values().any(contains_encrypted_content)
        }
        Value::Array(items) => items.iter().any(contains_encrypted_content),
        _ => false,
    }
}

fn assert_secondary_completed(body: &Value, label: &str) {
    let text = body.to_string();
    assert!(
        text.contains(FINAL_TEXT),
        "{label}: the completed assistant answer is missing from the replayed history"
    );
    assert!(
        !text.contains("\"status\":\"incomplete\""),
        "{label}: the replayed history contains an incomplete item"
    );
}

/// Codex's own `--json` reports the completed search action. Citation content
/// is checked separately in the tool output Codex replays upstream.
fn assert_search_completed(events: &[Value]) {
    let items = completed_items(events);
    let search = items
        .iter()
        .find(|item| item["type"] == "web_search" && item["query"] == SEARCH_QUERY)
        .unwrap_or_else(|| panic!("stock Codex did not report the web.run search: {items:#?}"));
    assert_eq!(
        search["action"]["type"], "search",
        "web.run must be a direct search action: {search}"
    );
}

fn assert_search_tools(wire: Wire, body: &Value) {
    match wire {
        Wire::Responses => {
            let tools = body["tools"].as_array().expect("Responses tools array");
            let web = tools
                .iter()
                .find(|tool| tool["type"] == "namespace" && tool["name"] == "web")
                .unwrap_or_else(|| panic!("Codex must emit the web namespace: {tools:#?}"));
            let nested = web["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| tool["name"].as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert!(
                nested.contains(&"run"),
                "the web namespace must expose run: {web}"
            );
        }
        Wire::Chat | Wire::Anthropic => {
            let names = body["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| match wire {
                            Wire::Chat => tool["function"]["name"].as_str(),
                            Wire::Anthropic | Wire::Responses => tool["name"].as_str(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert!(
                names.contains(&"web__run"),
                "{wire:?}: the translated wire must expose web.run as one function; upstream tools were {names:?}"
            );
        }
    }
}

fn assert_search_call_in_history(wire: Wire, body: &Value, label: &str) {
    match wire {
        Wire::Responses => assert!(
            responses_input(body).iter().any(|item| {
                item["type"] == "function_call"
                    && item["namespace"] == "web"
                    && item["name"] == "run"
                    && item["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("search_query")
            }),
            "{label}: the web.run call is missing from history"
        ),
        Wire::Chat => assert!(
            chat_messages(body).iter().any(|message| {
                message["tool_calls"].as_array().is_some_and(|calls| {
                    calls.iter().any(|call| {
                        call["function"]["name"] == "web__run"
                            && call["function"]["arguments"]
                                .as_str()
                                .unwrap_or_default()
                                .contains("search_query")
                    })
                })
            }),
            "{label}: the web.run call is missing from history"
        ),
        Wire::Anthropic => {
            let blocks = anthropic_blocks(body);
            assert!(
                blocks.iter().any(|block| {
                    block["type"] == "tool_use"
                        && block["name"] == "web__run"
                        && block["input"].to_string().contains("search_query")
                }),
                "{label}: the web.run call is missing from history"
            );
        }
    }
}

fn assert_search_result_replayed(wire: Wire, body: &Value, label: &str) {
    let has_citation = |text: &str| {
        [
            SEARCH_REF,
            SEARCH_URL,
            SEARCH_TITLE,
            SEARCH_SNIPPET,
            "turn0search1",
        ]
        .iter()
        .all(|value| text.contains(value))
    };
    match wire {
        Wire::Responses => assert!(
            responses_input(body).iter().any(|item| {
                item["type"] == "function_call_output"
                    && item["call_id"] == "call_w1"
                    && has_citation(&item["output"].to_string())
            }),
            "{label}: the Brave citation output is missing from history"
        ),
        Wire::Chat => assert!(
            chat_messages(body).iter().any(|message| {
                message["role"] == "tool" && has_citation(&message["content"].to_string())
            }),
            "{label}: the Brave citation output is missing from history"
        ),
        Wire::Anthropic => {
            let blocks = anthropic_blocks(body);
            assert!(
                blocks
                    .iter()
                    .any(|block| block["type"] == "tool_result"
                        && has_citation(&block.to_string())),
                "{label}: the Brave citation output is missing from history"
            );
        }
    }
}

fn assert_history(
    wire: Wire,
    edit_tool: EditTool,
    body: &Value,
    initial_prompt: &str,
    label: &str,
) {
    match wire {
        Wire::Responses => {
            let input = responses_input(body);
            assert!(
                input.iter().any(|item| {
                    item["type"] == "message"
                        && item["role"] == "user"
                        && item.to_string().contains(initial_prompt)
                }),
                "{label}: the original user prompt is missing from history"
            );
            match edit_tool {
                EditTool::ExecCommand => assert!(
                    input.iter().any(|item| {
                        item["type"] == "function_call"
                            && item["name"] == "exec_command"
                            && item["arguments"]
                                .as_str()
                                .unwrap_or_default()
                                .contains("> fixture.txt")
                    }),
                    "{label}: the edit command is missing from history"
                ),
                EditTool::ApplyPatch => assert!(
                    input.iter().any(|item| {
                        item["type"] == "custom_tool_call"
                            && item["name"] == "apply_patch"
                            && item["input"]
                                .as_str()
                                .unwrap_or_default()
                                .contains("*** Begin Patch")
                    }),
                    "{label}: the apply_patch call is missing from history"
                ),
            }
            assert!(
                responses_output_contains(input, "broken"),
                "{label}: the read output is missing from history"
            );
            assert!(
                responses_output_contains(input, CHECK_OUTPUT),
                "{label}: the check output is missing from history"
            );
            assert!(
                input.iter().any(|item| {
                    item["type"] == "message"
                        && item["role"] == "assistant"
                        && item.to_string().contains(FINAL_TEXT)
                }),
                "{label}: the assistant's final message is missing from history"
            );
        }
        Wire::Chat => {
            let messages = chat_messages(body);
            assert!(
                messages.iter().any(|message| {
                    message["role"] == "user"
                        && message["content"].to_string().contains(initial_prompt)
                }),
                "{label}: the original user prompt is missing from history"
            );
            assert!(
                chat_has_tool_call(messages, "exec_command"),
                "{label}: the read/check tool calls are missing from history"
            );
            match edit_tool {
                EditTool::ApplyPatch => {
                    assert!(
                        chat_has_tool_call(messages, "apply_patch"),
                        "{label}: the apply_patch call is missing from history"
                    );
                    assert!(
                        chat_tool_output_contains(messages, "Success"),
                        "{label}: the apply_patch output is missing from history"
                    );
                }
                EditTool::ExecCommand => assert!(
                    messages.iter().any(|message| {
                        message["tool_calls"].as_array().is_some_and(|calls| {
                            calls.iter().any(|call| {
                                call["function"]["name"] == "exec_command"
                                    && call["function"]["arguments"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .contains("> fixture.txt")
                            })
                        })
                    }),
                    "{label}: the exec_command edit is missing from history"
                ),
            }
            assert!(
                chat_tool_output_contains(messages, "broken"),
                "{label}: the read output is missing from history"
            );
            assert!(
                chat_tool_output_contains(messages, CHECK_OUTPUT),
                "{label}: the check output is missing from history"
            );
            assert!(
                messages.iter().any(|message| {
                    message["role"] == "assistant"
                        && message["content"].to_string().contains(FINAL_TEXT)
                }),
                "{label}: the assistant's final message is missing from history"
            );
        }
        Wire::Anthropic => {
            let blocks = anthropic_blocks(body);
            assert!(
                anthropic_text_contains(&blocks, initial_prompt),
                "{label}: the original user prompt is missing from history"
            );
            assert!(
                anthropic_has_tool_use(&blocks, "exec_command"),
                "{label}: the read/check tool calls are missing from history"
            );
            match edit_tool {
                EditTool::ApplyPatch => {
                    assert!(
                        anthropic_has_tool_use(&blocks, "apply_patch"),
                        "{label}: the apply_patch call is missing from history"
                    );
                    assert!(
                        anthropic_results_contain(&blocks, "Success"),
                        "{label}: the apply_patch output is missing from history"
                    );
                }
                EditTool::ExecCommand => assert!(
                    blocks.iter().any(|block| {
                        block["type"] == "tool_use"
                            && block["name"] == "exec_command"
                            && block["input"].to_string().contains("> fixture.txt")
                    }),
                    "{label}: the exec_command edit is missing from history"
                ),
            }
            assert!(
                anthropic_results_contain(&blocks, "broken"),
                "{label}: the read output is missing from history"
            );
            assert!(
                anthropic_results_contain(&blocks, CHECK_OUTPUT),
                "{label}: the check output is missing from history"
            );
            assert!(
                anthropic_text_contains(&blocks, FINAL_TEXT),
                "{label}: the assistant's final message is missing from history"
            );
        }
    }
}
