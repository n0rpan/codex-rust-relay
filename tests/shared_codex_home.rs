//! One scratch ox configuration, two stock Codex profiles.
//!
//! This opt-in test records the auth contract that makes `codeox` safe to use
//! beside an existing native ChatGPT login:
//!
//! 1. **Stock Codex via `ox codex --chatgpt`.** A disposable `CODEX_HOME` holds a fake
//!    `auth.json` (account tokens, not an API key) and a `config.toml` whose
//!    `chatgpt_base_url` points at a loopback HTTP discovery server. Codex
//!    0.171.0 discovers the selected workspace, then sends
//!    `POST /backend-api/codex/responses` to the routed **HTTPS** origin with
//!    `Authorization: Bearer <fake access token>` and
//!    `chatgpt-account-id: <fake account id>`. That is native ChatGPT account
//!    auth, not an API key.
//! 2. **The API relay through ox.** Codex is launched in a separate profile with
//!    `model_provider=codeox` and `CODEOX_TOKEN`, so the model request goes to
//!    the relay. The relay forwards to the fixture vendor with only that
//!    vendor's fake key. The ChatGPT access token never reaches the vendor.
//! 3. The native profile tree is byte-identical before and after the ox
//!    launch, even when the native config pins its SQLite and log paths.
//!
//! The fixture is deliberately awkward because stock Codex 0.171.0 requires
//! it: workspace routing discovery must return an **HTTPS origin**, so the
//! backend needs a local CA and leaf certificate. `CODEX_CA_CERTIFICATE`
//! (and `SSL_CERT_FILE`) trust the generated CA. Every socket binds to
//! `127.0.0.1`, every proxy variable points at a dead loopback port, and every
//! credential is a fabricated fixture value. No personal `CODEX_HOME`, auth
//! store, config file or public API is read or called.
//!
//! Prerequisites: stock Codex 0.171.0, `python3` (its stdlib HTTPS server) and
//! the `openssl` CLI for the throwaway PKI. Run manually:
//!
//! ```sh
//! CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 \
//!   cargo test --test shared_codex_home -- --ignored --nocapture
//! ```
#![cfg(unix)]

#[path = "common/profile_snapshot.rs"]
mod profile_snapshot;

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const ACCOUNT_ID: &str = "acct-shared-home-fixture";
const ACCOUNT_EMAIL: &str = "fixture@example.invalid";
const VENDOR_KEY: &str = "vendor-key-shared-home-fixture";
const VENDOR_MODEL: &str = "fixture-model";
const VENDOR_ALIAS: &str = "vendor/fixture-model";
const DISCOVERY_ROUTE: &str = "/backend-api/wham/accounts/check";
const INFERENCE_ROUTE: &str = "/backend-api/codex/responses";

/// The Python fixture is embedded so this test owns exactly one new file. It
/// serves three loopback endpoints and appends every request as one JSON line:
///
/// * `discovery` - HTTP; returns the accounts/check payload that routes Codex
///   to the HTTPS origin.
/// * `inference_tls` - HTTPS with the generated leaf certificate; the routed
///   native backend.
/// * `vendor` - plain HTTP; the provider that codeox forwards model requests
///   to.
const PYTHON_FIXTURE: &str = r##"
import json, ssl, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

state = Path(sys.argv[1])
cert = sys.argv[2]
key = sys.argv[3]
account = sys.argv[4]
requests_path = state / "requests.jsonl"
lock = threading.Lock()
tls = {"port": 0}


def record(server, handler):
    length = int(handler.headers.get("content-length") or 0)
    body = handler.rfile.read(length) if length else b""
    entry = {
        "server": server,
        "method": handler.command,
        "path": handler.path,
        "headers": {name.lower(): value for name, value in handler.headers.items()},
        "body_text": body.decode("utf-8", "replace"),
    }
    with lock:
        with requests_path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry) + "\n")
    return body


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_tag = "unknown"

    def log_message(self, *args):
        pass

    def reply(self, status, body=b""):
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_GET(self):
        record(self.server_tag, self)
        if self.server_tag == "discovery" and self.path == "/backend-api/wham/accounts/check":
            payload = {
                "accounts": [{
                    "id": account,
                    "workspace_backend_origin": "https://127.0.0.1:%d" % tls["port"],
                    "account_routing_override": "NO_CONSTRAINT",
                }]
            }
            self.reply(200, json.dumps(payload).encode())
        else:
            self.reply(404, b'{"error":"fixture"}')

    def do_POST(self):
        body = record(self.server_tag, self)
        if self.server_tag == "inference_tls" and self.path == "/backend-api/codex/responses":
            if b"native worker fixture" not in body:
                self.inference_reply("gpt-5.1-codex", "native fixture reply")
            elif b"vendor fixture reply" in body:
                self.inference_reply("gpt-5.1-codex", "native worker completed")
            elif any(item.get("type") == "function_call_output" and item.get("call_id") == "call_worker_fixture"
                     for item in json.loads(body).get("input", [])):
                self.inference_reply("gpt-5.1-codex", "worker call failed")
            else:
                tools = json.loads(body).get("tools", [])
                match = next(((t["name"], {}) for t in tools if t.get("name", "").endswith("delegate_to_ox_model")), None)
                if match is None:
                    match = next(((child["name"], {"namespace": t["name"]})
                                  for t in tools for child in t.get("tools", [])
                                  if child.get("name") == "delegate_to_ox_model"), None)
                if match is None:
                    names = [t.get("name", t.get("type", "")) for t in tools]
                    self.inference_reply("gpt-5.1-codex", "worker tool missing: " + ", ".join(names))
                else:
                    name, namespace = match
                    self.inference_reply("gpt-5.1-codex", "", {
                        "id": "fc_worker_tool", "type": "function_call", "status": "completed",
                        "call_id": "call_worker_fixture", "name": name,
                        "arguments": json.dumps({"model": "vendor/fixture-model", "task": "delegated-only marker"}),
                        **namespace,
                    })
        elif self.server_tag == "vendor" and self.path == "/v1/responses":
            self.inference_reply("fixture-model", "vendor fixture reply")
        elif self.server_tag == "vendor" and self.path == "/v1/chat/completions":
            chunks = [
                {"index": 0, "delta": {"role": "assistant", "content": "vendor fixture reply"}, "finish_reason": None},
                {"index": 0, "delta": {}, "finish_reason": "stop"},
            ]
            events = [
                {"id": "chat_fixture", "object": "chat.completion.chunk", "created": 1700000000,
                 "model": "fixture-model", "choices": [chunk]} for chunk in chunks
            ]
            stream = "".join("data: " + json.dumps(event) + "\n\n" for event in events)
            self.send_sse((stream + "data: [DONE]\n\n").encode())
        elif self.server_tag == "claude" and self.path.split("?")[0] == "/v1/messages":
            self.claude_reply(json.loads(body))
        else:
            self.reply(404, b'{"error":"fixture"}')

    def inference_reply(self, model, text, item=None):
        item = item or {
            "id": "msg_shared_home",
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }
        response = {
            "id": "resp_shared_home",
            "object": "response",
            "created_at": 1700000000,
            "status": "completed",
            "model": model,
            "output": [item],
            "usage": {
                "input_tokens": 3, "output_tokens": 2, "total_tokens": 5,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0},
            },
        }
        events = [
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": response},
        ]
        self.send_events(events)

    def claude_reply(self, request):
        model = request.get("model", "claude-sonnet-5")
        payload = json.dumps(request["messages"])
        worker_tools = [t["name"] for t in request.get("tools", []) if t.get("name", "").startswith("mcp__ox_workers__")]
        if "native worker fixture" in payload and "vendor fixture reply" not in payload and worker_tools:
            if "toolu_list_worker_models" not in payload:
                tool_id = "toolu_list_worker_models"
                name = "mcp__ox_workers__list_ox_models"
                arguments = {"query": "vendor"}
            else:
                assert "vendor/fixture-model" in payload, "catalog should contain the provider-qualified alias"
                tool_id = "toolu_native_worker"
                name = "mcp__ox_workers__delegate_to_ox_model"
                arguments = {"model": "vendor/fixture-model", "task": "delegated-only marker"}
            assert name in worker_tools
            block = {"type": "tool_use", "id": tool_id, "name": name, "input": {}}
            delta = {"type": "input_json_delta", "partial_json": json.dumps(arguments)}
            stop = "tool_use"
        else:
            text = "native worker completed" if "vendor fixture reply" in payload else "worker tool missing"
            block = {"type": "text", "text": ""}
            delta = {"type": "text_delta", "text": text}
            stop = "end_turn"
        self.send_events([
            {"type": "message_start", "message": {
                "id": "msg_claude_fixture", "type": "message", "role": "assistant",
                "model": model, "content": [], "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 3, "output_tokens": 0},
            }},
            {"type": "content_block_start", "index": 0, "content_block": block},
            {"type": "content_block_delta", "index": 0, "delta": delta},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
             "usage": {"output_tokens": 2}},
            {"type": "message_stop"},
        ])

    def send_events(self, events):
        body = "".join(
            "event: " + event["type"] + "\ndata: " + json.dumps(event) + "\n\n"
            for event in events
        ).encode()
        self.send_sse(body)

    def send_sse(self, body):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def server(tag):
    class Tagged(Handler):
        server_tag = tag
    return Server(("127.0.0.1", 0), Tagged)


inference = server("inference_tls")
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert, key)
inference.socket = context.wrap_socket(inference.socket, server_side=True)
tls["port"] = inference.server_address[1]
discovery = server("discovery")
vendor = server("vendor")
claude = server("claude")

for item in (inference, discovery, vendor, claude):
    threading.Thread(target=item.serve_forever, daemon=True).start()

ports = {
    "discovery": discovery.server_address[1],
    "inference": inference.server_address[1],
    "vendor": vendor.server_address[1],
    "claude": claude.server_address[1],
}
tmp = state / "ports.json.tmp"
tmp.write_text(json.dumps(ports))
tmp.replace(state / "ports.json")
threading.Event().wait()
"##;

// ---------------------------------------------------------------------------
// Offline checks (no stock CLI, no sockets)
// ---------------------------------------------------------------------------

#[test]
fn fixture_credentials_are_fabricated_and_loopback_only() {
    let (id_token, access_token) = fixture_tokens();
    for token in [&id_token, &access_token] {
        let parts: Vec<_> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "fixture token must be a three-part JWT");
    }
    assert_ne!(id_token, access_token, "fixture tokens must be distinct");
    assert!(
        id_token.len() > access_token.len(),
        "the id token carries the fake account claims"
    );
    for public in ["chatgpt.com", "openai.com/backend-api", "api.openai.com"] {
        assert!(
            !PYTHON_FIXTURE.contains(public),
            "the loopback fixture must not name the public host {public}"
        );
    }
    assert!(PYTHON_FIXTURE.contains("127.0.0.1"));
    assert_eq!(VENDOR_ALIAS, "vendor/fixture-model");
}

#[test]
#[ignore = "opt-in: set OX_TEST_CLAUDE_BIN to the stock Claude Code binary"]
fn stock_claude_calls_api_worker_without_changing_its_main_route() {
    let claude =
        fs::canonicalize(env::var_os("OX_TEST_CLAUDE_BIN").expect("set OX_TEST_CLAUDE_BIN"))
            .expect("find the stock Claude Code binary");
    let tree = TempTree::new("claude-worker");
    let home = tree.path.join("home");
    let codex_home = tree.path.join("codex-home");
    let project = tree.path.join("project");
    let tmp = tree.path.join("tmp");
    let bin = tree.path.join("bin");
    let pki = tree.path.join("pki");
    for directory in [&home, &codex_home, &project, &tmp, &bin, &pki] {
        fs::create_dir_all(directory).unwrap();
    }
    std::os::unix::fs::symlink(claude, bin.join("claude")).unwrap();
    let (_, cert, key) = generate_pki(&pki);
    let mut fixture = ChildGuard::new(start_fixture(&tree.path, &cert, &key));
    let ports = wait_for_ports(&tree.path, fixture.child());
    fs::write(
        codex_home.join("codeox.toml"),
        format!(
            r#"
[providers.vendor]
base_url = "http://127.0.0.1:{}/v1"
env_key = "VENDOR_KEY"
wire_api = "chat_completions"
discovery = false
[[providers.vendor.models]]
id = "fixture-model"
"#,
            ports.vendor
        ),
    )
    .unwrap();

    let mut worker = Command::new(env!("CARGO_BIN_EXE_ox"));
    worker.current_dir(&project).stdin(Stdio::null()).args([
        "claude",
        "--native",
        "native worker fixture",
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--allowedTools",
        "mcp__ox_workers__list_ox_models,mcp__ox_workers__delegate_to_ox_model",
    ]);
    apply_env(
        &mut worker,
        &loopback_env(
            &home,
            &codex_home,
            &tmp,
            &format!("{}:/usr/bin:/bin", bin.display()),
        ),
    );
    worker
        .env(
            "ANTHROPIC_BASE_URL",
            format!("http://127.0.0.1:{}", ports.claude),
        )
        .env("ANTHROPIC_AUTH_TOKEN", "fixture-main-claude-key")
        .env("ANTHROPIC_MODEL", "claude-sonnet-5")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("ENABLE_TOOL_SEARCH", "false")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("VENDOR_KEY", VENDOR_KEY);
    let log = tree.path.join("claude-worker.log");
    let mut worker = spawn_logged(worker, &log);
    wait_for_exit(&mut worker, &log);
    let output = fs::read_to_string(&log).unwrap();
    assert!(
        output
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["type"] == "result" && event["result"] == "native worker completed"),
        "Claude Code must return the MCP worker's answer to its main model:\n{}",
        tail(&log)
    );
    let requests = read_requests(&tree.path);
    let delegated = find_request(&requests, "vendor", "POST", "/v1/chat/completions").unwrap();
    assert_header(delegated, "authorization", &format!("Bearer {VENDOR_KEY}"));
    assert!(delegated.body_text.contains("delegated-only marker"));
    assert!(!delegated.body_text.contains("native worker fixture"));
    assert!(!delegated.body_text.contains("fixture-main-claude-key"));
    let returned = requests
        .iter()
        .find(|request| {
            request.server == "claude"
                && request.method == "POST"
                && request.body_text.contains("vendor fixture reply")
        })
        .unwrap();
    assert_header(returned, "authorization", "Bearer fixture-main-claude-key");
    assert!(!returned.body_text.contains(VENDOR_KEY));
    let mut plain = Command::new(bin.join("claude"));
    plain.args(["mcp", "list"]).current_dir(&project);
    apply_env(
        &mut plain,
        &loopback_env(
            &home,
            &codex_home,
            &tmp,
            &format!("{}:/usr/bin:/bin", bin.display()),
        ),
    );
    let plain = plain.output().unwrap();
    assert!(plain.status.success());
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("ox_workers"));
}

#[test]
fn manual_suite_requires_an_explicit_cli_path() {
    let error = stock_binary_from(None).unwrap_err();
    assert!(error.contains("CODEOX_TEST_CODEX_BIN"), "{error}");
}

// ---------------------------------------------------------------------------
// Opt-in end-to-end proof
// ---------------------------------------------------------------------------

#[test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN to the stock Codex binary"]
fn stock_codex_uses_native_credentials_and_an_isolated_relay_profile() {
    let codex = stock_binary();
    let version = codex_version(&codex);
    println!("Testing stock Codex: {version}");
    require_tool("python3", "--version");
    require_tool("openssl", "version");

    let tree = TempTree::new("shared-home");
    let home = tree.path.join("home");
    let codex_home = tree.path.join("codex-home");
    let project = tree.path.join("project");
    let tmp = tree.path.join("tmp");
    let bin = tree.path.join("bin");
    let pki = tree.path.join("pki");
    for directory in [&home, &codex_home, &project, &tmp, &bin, &pki] {
        fs::create_dir_all(directory).expect("create isolation directory");
    }
    let codex_shim = bin.join("codex");
    std::os::unix::fs::symlink(&codex, &codex_shim)
        .unwrap_or_else(|error| panic!("could not symlink {codex_shim:?} -> {codex:?}: {error}"));

    let (ca_cert, leaf_cert, leaf_key) = generate_pki(&pki);
    let mut fixture = ChildGuard::new(start_fixture(&tree.path, &leaf_cert, &leaf_key));
    let ports = wait_for_ports(&tree.path, fixture.child());

    let (id_token, access_token) = fixture_tokens();
    let auth_path = codex_home.join("auth.json");
    let config_path = codex_home.join("config.toml");
    write_auth(&auth_path, &id_token, &access_token);
    fs::write(
        &config_path,
        format!(
            "model = \"gpt-5.1-codex\"\n\
             chatgpt_base_url = \"http://127.0.0.1:{}/backend-api\"\n\
             approval_policy = \"never\"\n\
             sandbox_mode = \"read-only\"\n\
             sqlite_home = {}\n\
             log_dir = {}\n",
            ports.discovery,
            json!(codex_home.join("native-state")),
            json!(codex_home.join("native-logs"))
        ),
    )
    .expect("write config.toml");
    fs::write(project.join("fixture.txt"), "disposable\n").expect("write project fixture");

    // ---- 1. Stock Codex with native ChatGPT account auth -------------------
    let phase1_log = tree.path.join("phase1.log");
    let mut direct = Command::new(env!("CARGO_BIN_EXE_ox"));
    direct
        .args([
            "codex",
            "--chatgpt",
            "--no-daemon",
            "--disable",
            "plugins",
            "--disable",
            "hooks",
            "exec",
            "--skip-git-repo-check",
            "--json",
            "native auth fixture",
        ])
        .current_dir(&project)
        .stdin(Stdio::null());
    let mut environment = loopback_env(
        &home,
        &codex_home,
        &tmp,
        &format!("{}:/usr/bin:/bin", bin.display()),
    );
    environment.push((
        "OPENAI_API_KEY".to_owned(),
        "unused-fixture-api-key".to_owned(),
    ));
    environment.push((
        "CODEX_CA_CERTIFICATE".to_owned(),
        ca_cert.display().to_string(),
    ));
    environment.push(("SSL_CERT_FILE".to_owned(), ca_cert.display().to_string()));
    apply_env(&mut direct, &environment);
    let mut direct = spawn_logged(direct, &phase1_log);
    // Let Codex finish the native turn and let ox reap it before starting
    // phase two. Killing only ox here leaves Codex free to retry in the background.
    wait_for_turn(&mut direct, &phase1_log, "native fixture reply");

    let requests = read_requests(&tree.path);
    let discovery = find_request(&requests, "discovery", "GET", DISCOVERY_ROUTE)
        .unwrap_or_else(|| panic!("missing native discovery request; captured {requests:#?}"));
    assert_header(
        discovery,
        "authorization",
        &format!("Bearer {access_token}"),
    );
    assert_header(discovery, "chatgpt-account-id", ACCOUNT_ID);
    let inference = find_request(&requests, "inference_tls", "POST", INFERENCE_ROUTE)
        .unwrap_or_else(|| panic!("missing native inference request; captured {requests:#?}"));
    assert_header(
        inference,
        "authorization",
        &format!("Bearer {access_token}"),
    );
    assert_header(inference, "chatgpt-account-id", ACCOUNT_ID);
    assert_header(inference, "originator", "codex_exec");
    assert!(
        !inference.headers.contains_key("x-api-key"),
        "native ChatGPT auth must not be downgraded to an API key"
    );

    // ---- 2. Same ox configuration, separate runtime profile --------------
    let codeox_toml = codex_home.join("codeox.toml");
    fs::write(
        &codeox_toml,
        format!(
            "[providers.vendor]\n\
             name = \"Vendor Fixture\"\n\
             base_url = \"http://127.0.0.1:{}/v1\"\n\
             env_key = \"VENDOR_KEY\"\n\
             wire_api = \"responses\"\n\
             discovery = false\n\
             \n\
             [[providers.vendor.models]]\n\
             id = \"{VENDOR_MODEL}\"\n\
             name = \"Fixture Model\"\n\
             context_window = 131072\n\
             input_modalities = [\"text\"]\n\
             reasoning_efforts = [\"low\", \"medium\", \"high\"]\n",
            ports.vendor
        ),
    )
    .expect("write codeox.toml");

    let phase2_start = read_requests(&tree.path).len();
    let native_before = profile_snapshot::snapshot(&codex_home, Some("ox-clients"));

    let mut status = Command::new(env!("CARGO_BIN_EXE_ox"));
    status
        .args(["login", "chatgpt", "status"])
        .current_dir(&project)
        .stdin(Stdio::null());
    apply_env(&mut status, &environment);
    let status = status.output().expect("run Codex account check");
    assert!(status.status.success());
    assert_eq!(
        profile_snapshot::snapshot(&codex_home, Some("ox-clients")),
        native_before,
        "checking for an existing Codex sign-in must leave its profile untouched"
    );

    let phase2_log = tree.path.join("phase2.log");
    let mut codeox = Command::new(env!("CARGO_BIN_EXE_ox"));
    codeox
        .args([
            "--no-daemon",
            "--disable",
            "plugins",
            "--disable",
            "hooks",
            "exec",
            "--skip-git-repo-check",
            "--json",
            "-m",
            VENDOR_ALIAS,
            "vendor key fixture",
        ])
        .current_dir(&project)
        .stdin(Stdio::null());
    let mut environment = loopback_env(
        &home,
        &codex_home,
        &tmp,
        &format!("{}:/usr/bin:/bin", bin.display()),
    );
    environment.push(("VENDOR_KEY".to_owned(), VENDOR_KEY.to_owned()));
    environment.push((
        "CODEX_SQLITE_HOME".to_owned(),
        codex_home.join("inherited-state").display().to_string(),
    ));
    apply_env(&mut codeox, &environment);
    let mut codeox = spawn_logged(codeox, &phase2_log);
    wait_for_turn(&mut codeox, &phase2_log, "vendor fixture reply");

    let requests = read_requests(&tree.path);
    let phase2 = &requests[phase2_start..];
    assert!(
        !phase2.iter().any(|request| request.path == INFERENCE_ROUTE),
        "codeox must not send model traffic to the native ChatGPT backend"
    );
    let vendor = find_request(phase2, "vendor", "POST", "/v1/responses")
        .unwrap_or_else(|| panic!("missing vendor request; captured {requests:#?}"));
    assert_header(vendor, "authorization", &format!("Bearer {VENDOR_KEY}"));
    assert!(
        !vendor.headers.contains_key("chatgpt-account-id"),
        "the vendor must not receive the ChatGPT account id"
    );
    for (name, value) in &vendor.headers {
        assert!(
            !value.contains(&access_token),
            "the ChatGPT access token leaked to the vendor header {name:?}"
        );
    }
    assert!(
        !vendor.body_text.contains(&access_token),
        "the ChatGPT access token leaked into the vendor request body"
    );
    assert!(
        vendor.body_text.contains(VENDOR_MODEL),
        "the relay must rewrite the alias to the vendor model id"
    );

    assert_eq!(
        profile_snapshot::snapshot(&codex_home, Some("ox-clients")),
        native_before,
        "relay must leave the native Codex profile and ox provider registry untouched"
    );
    let relay_profile = codex_home.join("ox-clients/codex");
    assert!(relay_profile.join("config.toml").is_file());
    assert!(!relay_profile.join("auth.json").exists());

    // ---- 3. Real native Codex calls the launch-scoped MCP worker ----------
    let phase3_start = requests.len();
    let phase3_log = tree.path.join("phase3.log");
    let mut worker = Command::new(env!("CARGO_BIN_EXE_ox"));
    worker
        .args([
            "codex",
            "--chatgpt",
            "-c",
            "mcp_servers.ox_workers.tools.delegate_to_ox_model.approval_mode=\"approve\"",
            "--no-daemon",
            "--disable",
            "enable_request_compression",
            "--disable",
            "tool_search",
            "--disable",
            "plugins",
            "--disable",
            "hooks",
            "exec",
            "--skip-git-repo-check",
            "--json",
            "native worker fixture",
        ])
        .current_dir(&project)
        .stdin(Stdio::null());
    environment.push((
        "CODEX_CA_CERTIFICATE".to_owned(),
        ca_cert.display().to_string(),
    ));
    environment.push(("SSL_CERT_FILE".to_owned(), ca_cert.display().to_string()));
    apply_env(&mut worker, &environment);
    let mut worker = spawn_logged(worker, &phase3_log);
    wait_for_turn(&mut worker, &phase3_log, "native worker completed");
    let requests = read_requests(&tree.path);
    let phase3 = &requests[phase3_start..];
    let worker_call = find_request(phase3, "vendor", "POST", "/v1/responses")
        .expect("native Codex should call the ox worker");
    assert_header(
        worker_call,
        "authorization",
        &format!("Bearer {VENDOR_KEY}"),
    );
    assert!(worker_call.body_text.contains("delegated-only marker"));
    assert!(!worker_call.body_text.contains("native worker fixture"));
    assert!(!worker_call.body_text.contains(&access_token));
    assert!(!worker_call.headers.contains_key("chatgpt-account-id"));
    let returned = phase3
        .iter()
        .find(|request| {
            request.path == INFERENCE_ROUTE && request.body_text.contains("vendor fixture reply")
        })
        .expect("worker answer should return to the native model");
    assert_header(returned, "authorization", &format!("Bearer {access_token}"));
    assert!(!returned.body_text.contains(VENDOR_KEY));
    assert!(
        !fs::read_to_string(config_path)
            .unwrap()
            .contains("ox_workers")
    );
    let mut plain = Command::new(&codex);
    plain.args(["mcp", "list", "--json"]).current_dir(&project);
    apply_env(&mut plain, &environment);
    let plain = plain.output().unwrap();
    assert!(plain.status.success());
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("ox_workers"));
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fixture_tokens() -> (String, String) {
    let id_payload = json!({
        "iss": "https://auth.openai.com",
        "aud": "fixture",
        "exp": 2_000_000_000_u64,
        "iat": 1_700_000_000_u64,
        "email": ACCOUNT_EMAIL,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": ACCOUNT_ID,
            "chatgpt_plan_type": "plus",
            "chatgpt_user_id": "user-shared-home-fixture",
        },
    });
    let access_payload = json!({
        "exp": 2_000_000_000_u64,
        "iat": 1_700_000_000_u64,
    });
    (jwt(&id_payload), jwt(&access_payload))
}

fn jwt(payload: &Value) -> String {
    format!(
        "{}.{}.sig",
        base64url(br#"{"alg":"none","typ":"JWT"}"#),
        base64url(payload.to_string().as_bytes())
    )
}

fn base64url(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity((input.len() * 4).div_ceil(3));
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let bits = ((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | bytes[2] as u32;
        output.push(TABLE[((bits >> 18) & 63) as usize] as char);
        output.push(TABLE[((bits >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[((bits >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            output.push(TABLE[(bits & 63) as usize] as char);
        }
    }
    output
}

fn write_auth(path: &Path, id_token: &str, access_token: &str) {
    let document = json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": "fake-refresh-token",
            "account_id": ACCOUNT_ID,
        },
        "last_refresh": "2026-11-01T00:00:00Z",
    });
    fs::write(
        path,
        serde_json::to_vec_pretty(&document).expect("serialize auth.json"),
    )
    .expect("write auth.json");
}

// ---------------------------------------------------------------------------
// Process, PKI and request helpers
// ---------------------------------------------------------------------------

struct Ports {
    discovery: u16,
    vendor: u16,
    claude: u16,
}

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
            "codeox-shared-home-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp tree");
        Self { path }
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("fixture child is live")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn stock_binary() -> PathBuf {
    stock_binary_from(env::var_os("CODEOX_TEST_CODEX_BIN"))
        .unwrap_or_else(|error| panic!("{error}"))
}

fn stock_binary_from(raw: Option<OsString>) -> Result<PathBuf, String> {
    let Some(raw) = raw else {
        return Err(
            "CODEOX_TEST_CODEX_BIN is not set. This opt-in test requires the real installed \
             stock Codex CLI; set CODEOX_TEST_CODEX_BIN to e.g. \
             /home/$USER/.local/share/mise/installs/codex/0.171.0/bin/codex"
                .to_owned(),
        );
    };
    let path = PathBuf::from(raw);
    if !path.is_file() {
        return Err(format!(
            "CODEOX_TEST_CODEX_BIN points at {path:?}, which is not a file"
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

fn require_tool(tool: &str, version_arg: &str) {
    let status = Command::new(tool)
        .arg(version_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap_or_else(|error| panic!("this opt-in test requires {tool}: {error}"));
    assert!(
        status.success(),
        "this opt-in test requires a working {tool}"
    );
}

fn loopback_env(home: &Path, codex_home: &Path, tmp: &Path, path: &str) -> Vec<(String, String)> {
    vec![
        ("HOME".to_owned(), home.display().to_string()),
        ("CODEX_HOME".to_owned(), codex_home.display().to_string()),
        ("TMPDIR".to_owned(), tmp.display().to_string()),
        ("PATH".to_owned(), path.to_owned()),
        ("HTTP_PROXY".to_owned(), "http://127.0.0.1:9".to_owned()),
        ("HTTPS_PROXY".to_owned(), "http://127.0.0.1:9".to_owned()),
        ("ALL_PROXY".to_owned(), "http://127.0.0.1:9".to_owned()),
        ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
        ("no_proxy".to_owned(), "127.0.0.1,localhost".to_owned()),
    ]
}

fn apply_env(command: &mut Command, environment: &[(String, String)]) {
    command.env_clear();
    for (name, value) in environment {
        command.env(name, value);
    }
}

fn spawn_logged(mut command: Command, log: &Path) -> Child {
    let stdout = File::create(log).expect("create child log");
    let stderr = stdout.try_clone().expect("clone child log");
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command
        .spawn()
        .unwrap_or_else(|error| panic!("could not start {command:?}: {error}"))
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let start = text.len().saturating_sub(6000);
    text[start..].to_owned()
}

fn generate_pki(directory: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let ca_cert = directory.join("ca.pem");
    let leaf_cert = directory.join("server.pem");
    let leaf_key = directory.join("server.key");
    fs::write(
        directory.join("server.ext"),
        "subjectAltName=IP:127.0.0.1\n\
         basicConstraints=critical,CA:FALSE\n\
         keyUsage=critical,digitalSignature,keyEncipherment\n\
         extendedKeyUsage=serverAuth\n",
    )
    .expect("write certificate extensions");
    openssl(
        directory,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
            "-days",
            "2",
            "-subj",
            "/CN=Codex Shared Home Test CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
        ],
    );
    openssl(
        directory,
        &[
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            "server.key",
            "-out",
            "server.csr",
            "-subj",
            "/CN=127.0.0.1",
        ],
    );
    openssl(
        directory,
        &[
            "x509",
            "-req",
            "-in",
            "server.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-out",
            "server.pem",
            "-days",
            "2",
            "-extfile",
            "server.ext",
        ],
    );
    (ca_cert, leaf_cert, leaf_key)
}

fn openssl(directory: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .args(args)
        .current_dir(directory)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("could not run openssl {args:?}: {error}"));
    assert!(
        output.status.success(),
        "openssl {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn start_fixture(state: &Path, leaf_cert: &Path, leaf_key: &Path) -> Child {
    let script = state.join("fixture.py");
    fs::write(&script, PYTHON_FIXTURE).expect("write python fixture");
    let stdout = File::create(state.join("fixture.out")).expect("create fixture stdout");
    let stderr = File::create(state.join("fixture.err")).expect("create fixture stderr");
    let mut command = Command::new("python3");
    command
        .arg(&script)
        .arg(state)
        .arg(leaf_cert)
        .arg(leaf_key)
        .arg(ACCOUNT_ID)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command
        .spawn()
        .unwrap_or_else(|error| panic!("could not start python3 fixture: {error}"))
}

fn wait_for_ports(state: &Path, fixture: &mut Child) -> Ports {
    let path = state.join("ports.json");
    let start = Instant::now();
    loop {
        if let Ok(raw) = fs::read_to_string(&path)
            && let Ok(document) = serde_json::from_str::<Value>(&raw)
            && let (Some(discovery), Some(vendor), Some(claude)) = (
                document["discovery"].as_u64(),
                document["vendor"].as_u64(),
                document["claude"].as_u64(),
            )
        {
            return Ports {
                discovery: discovery as u16,
                vendor: vendor as u16,
                claude: claude as u16,
            };
        }
        if let Some(status) = fixture.try_wait().expect("wait for fixture") {
            panic!(
                "python fixture exited with {status} before binding ports:\n{}",
                tail(&state.join("fixture.err"))
            );
        }
        if start.elapsed() > Duration::from_secs(30) {
            stop(fixture);
            panic!(
                "python fixture did not report ports within 30s:\n{}",
                tail(&state.join("fixture.err"))
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit(child: &mut Child, log: &Path) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().expect("wait for ox") {
            assert!(status.success(), "ox exited with {status}:\n{}", tail(log));
            break;
        }
        if Instant::now() >= deadline {
            stop(child);
            panic!(
                "ox did not finish the turn within 90 seconds:\n{}",
                tail(log)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_turn(child: &mut Child, log: &Path, expected: &str) {
    wait_for_exit(child, log);
    let raw = fs::read_to_string(log).expect("read Codex output");
    let events: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert!(
        events.iter().any(|event| event["type"] == "turn.completed")
            && events.iter().any(|event| {
                event["type"] == "item.completed"
                    && event["item"]["type"] == "agent_message"
                    && event["item"]["text"] == expected
            }),
        "expected a completed Codex turn with {expected:?}:\n{}",
        tail(log)
    );
}

#[derive(Debug, Clone)]
struct Recorded {
    server: String,
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body_text: String,
}

fn read_requests(state: &Path) -> Vec<Recorded> {
    let Ok(raw) = fs::read_to_string(state.join("requests.jsonl")) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            let headers = value
                .get("headers")?
                .as_object()?
                .iter()
                .filter_map(|(name, value)| {
                    Some((name.to_ascii_lowercase(), value.as_str()?.to_owned()))
                })
                .collect();
            Some(Recorded {
                server: value.get("server")?.as_str()?.to_owned(),
                method: value.get("method")?.as_str()?.to_owned(),
                path: value.get("path")?.as_str()?.to_owned(),
                headers,
                body_text: value
                    .get("body_text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect()
}

fn find_request<'a>(
    requests: &'a [Recorded],
    server: &str,
    method: &str,
    path: &str,
) -> Option<&'a Recorded> {
    requests.iter().find(|request| {
        request.server == server && request.method == method && request.path == path
    })
}

fn assert_header(request: &Recorded, name: &str, expected: &str) {
    let actual = request.headers.get(name).map(String::as_str);
    assert_eq!(
        actual,
        Some(expected),
        "{} {} {}: header {name:?}",
        request.server,
        request.method,
        request.path
    );
}
