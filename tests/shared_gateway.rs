#![cfg(unix)]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

const GATEWAY_KEY: &str = "fixture-gateway-key-used-only-for-local-requests";
const RESPONSES_SSE: &str = include_str!("fixtures/shared_gateway/responses.sse");
const MESSAGES_SSE: &str = include_str!("fixtures/shared_gateway/messages.sse");
const DESKTOP_MESSAGES_SSE: &str = include_str!("fixtures/shared_gateway/desktop_messages.sse");
const CONTEXT_ERROR: &str = r#"{ "type": "error", "error": { "type": "invalid_request_error", "message": "prompt is too long: 1000001 tokens > 1000000 maximum" } }"#;
const BILLING_ERROR: &str = r#"{ "type": "error", "error": { "type": "billing_error", "message": "Request blocked by the provider budget." } }"#;
const CHAT_SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n";

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).unwrap();
        let path = std::env::temp_dir().join(format!("ox-shared-gateway-{}", hex::encode(bytes)));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ox"));
        command
            .env_clear()
            .env("CODEX_HOME", &self.0)
            .env("RESPONSE_KEY", "responses-vendor-fixture")
            .env("CLAUDE_KEY", "messages-vendor-fixture")
            .env("CHAT_KEY", "chat-vendor-fixture")
            .kill_on_drop(true);
        command
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct Hit {
    path: String,
    headers: hyper::HeaderMap,
    body: Value,
}

async fn mock(home: &Home) -> (Arc<Mutex<Vec<Hit>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = String::new();
    for (name, wire, key) in [
        ("responses", "responses", "RESPONSE_KEY"),
        ("claude", "anthropic_messages", "CLAUDE_KEY"),
        ("chat", "chat_completions", "CHAT_KEY"),
    ] {
        config.push_str(&format!(
            "[providers.{name}]\nbase_url = \"{base}\"\nwire_api = \"{wire}\"\n\
             env_key = \"{key}\"\ndiscovery = false\n\
             [[providers.{name}.models]]\nid = \"{name}-model\"\n"
        ));
    }
    config.push_str(
        "\n[[providers.responses.models]]\nid = \"claude-alternate\"\nname = \"Claude via Responses\"\n",
    );
    config.push_str(
        "\n[[providers.claude.models]]\nid = \"claude-sonnet-5\"\ncontext_window = 1000000\n",
    );
    std::fs::write(home.0.join("codeox.toml"), config).unwrap();

    let hits = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&hits);
    let task = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let recorded = Arc::clone(&recorded);
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = body.collect().await.unwrap().to_bytes();
                        let body: Value = serde_json::from_slice(&body).unwrap();
                        let (status, content_type, fixture) = if body["model"] == "rejected-model" {
                            (
                                401,
                                "application/json",
                                r#"{"error":{"message":"bad key responses-vendor-fixture"}}"#,
                            )
                        } else if parts.uri.path() == "/v1/messages/count_tokens" {
                            (200, "application/json", r#"{"input_tokens":17}"#)
                        } else if parts.uri.path() == "/v1/responses/input_tokens" {
                            (
                                200,
                                "application/json",
                                r#"{"object":"response.input_tokens","input_tokens":21}"#,
                            )
                        } else if parts.uri.path() == "/v1/messages"
                            && body["model"] == "claude-sonnet-5"
                        {
                            match body["metadata"]["user_id"].as_str() {
                                Some("fixture-context-limit") => {
                                    (400, "application/json", CONTEXT_ERROR)
                                }
                                Some("fixture-budget") => (402, "application/json", BILLING_ERROR),
                                _ => (200, "text/event-stream", DESKTOP_MESSAGES_SSE),
                            }
                        } else {
                            let stream = match parts.uri.path() {
                                "/v1/responses" => RESPONSES_SSE,
                                "/v1/messages" => MESSAGES_SSE,
                                "/v1/chat/completions" => CHAT_SSE,
                                path => panic!("unexpected upstream endpoint {path}"),
                            };
                            (200, "text/event-stream", stream)
                        };
                        recorded.lock().unwrap().push(Hit {
                            path: parts.uri.to_string(),
                            headers: parts.headers,
                            body,
                        });
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(status)
                                .header("content-type", content_type)
                                .header("request-id", "req_fixture_desktop")
                                .body(Full::new(Bytes::from_static(fixture.as_bytes())))
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    (hits, task)
}

// The CLI serves a fixed port. A released test port can be taken by another parallel
// test before the child binds it. Wait for this child's own readiness announcement so
// another listener on the candidate port cannot answer its health check.
async fn launch_gateway(home: &Home, port: u16) -> Option<Child> {
    let mut child = home
        .command()
        .args(["serve", "--port", &port.to_string()])
        .env("OX_GATEWAY_KEY", GATEWAY_KEY)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let ready = format!("ox: serving on http://127.0.0.1:{port}");
    let busy = format!("ox: port {port} is already in use");
    let mut lines = BufReader::new(child.stderr.as_mut().unwrap()).lines();
    let mut diagnostic = String::new();
    let listening = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line == ready {
                return true;
            }
            diagnostic.push_str(&line);
            diagnostic.push('\n');
        }
        false
    })
    .await
    .expect("timed out waiting for ox serve to bind");
    if listening {
        return Some(child);
    }
    let status = child.wait().await.unwrap();
    assert!(
        diagnostic.lines().any(|line| line == busy),
        "ox serve exited with {status}: {diagnostic}"
    );
    None
}

async fn start_gateway(home: &Home) -> (u16, Child) {
    for _ in 0..10 {
        let port = loopback_port().await;
        if let Some(child) = launch_gateway(home, port).await {
            return (port, child);
        }
    }
    panic!("could not obtain an unused gateway port");
}
#[tokio::test]
async fn a_fixed_endpoint_serves_both_client_protocols_and_survives_restart() {
    let home = Home::new();
    let (hits, mock) = mock(&home).await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let (port, mut child) = start_gateway(&home).await;
    let base = format!("http://127.0.0.1:{port}/v1");

    for path in ["catalog", "models", "responses", "messages"] {
        let reply = client.get(format!("{base}/{path}")).send().await.unwrap();
        assert_eq!(reply.status(), 401, "{path}");
    }
    let codex: Value = client
        .get(format!("{base}/catalog"))
        .bearer_auth(GATEWAY_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        codex["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["slug"] == "responses/responses-model")
    );
    let claude: Value = client
        .get(format!("{base}/models?limit=1000"))
        .header("x-api-key", GATEWAY_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let models = claude["data"].as_array().unwrap();
    for alias in [
        "claude/claude-model",
        "responses/claude-alternate",
        "chat/chat-model",
    ] {
        assert!(models.iter().any(|model| model["id"] == alias), "{alias}");
    }

    let responses = client
        .post(format!("{base}/responses?internal=private"))
        .bearer_auth(GATEWAY_KEY)
        .header("x-api-key", "do-not-forward")
        .header("tailscale-user-login", "account-fixture")
        .json(&json!({"model":"responses/responses-model","input":"hello","stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(responses.status(), 200);
    assert_eq!(responses.text().await.unwrap(), RESPONSES_SSE);

    let messages = client.post(format!("{base}/messages?beta=true"))
        .header("x-api-key", GATEWAY_KEY).header("anthropic-beta", "context-1m-2025-08-07")
        .json(&json!({"model":"claude/claude-model","messages":[{"role":"user","content":"hello"}],"max_tokens":32,"stream":true}))
        .send().await.unwrap();
    assert_eq!(messages.status(), 200);
    assert_eq!(messages.text().await.unwrap(), MESSAGES_SSE);

    child.kill().await.unwrap();
    let mut child = launch_gateway(&home, port)
        .await
        .expect("rebind the same port");
    let reply = client
        .get(format!("{base}/models"))
        .bearer_auth(GATEWAY_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    child.kill().await.unwrap();
    mock.abort();

    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].path, "/v1/responses");
    assert_eq!(
        hits[0].headers["authorization"],
        "Bearer responses-vendor-fixture"
    );
    assert!(!hits[0].headers.contains_key("tailscale-user-login"));
    assert!(!hits[0].headers.contains_key("x-api-key"));
    assert_eq!(hits[0].body["model"], "responses-model");
    assert_eq!(hits[1].path, "/v1/messages");
    assert_eq!(hits[1].headers["anthropic-beta"], "context-1m-2025-08-07");
    assert_eq!(hits[1].headers["x-api-key"], "messages-vendor-fixture");
    assert!(!hits[1].headers.contains_key("authorization"));
    assert_eq!(hits[1].body["model"], "claude-model");
}

#[tokio::test]
async fn token_counts_follow_the_selected_inference_route() {
    let home = Home::new();
    let (hits, mock) = mock(&home).await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let (port, mut child) = start_gateway(&home).await;
    let base = format!("http://127.0.0.1:{port}");
    let endpoint = format!("{base}/v1/messages/count_tokens?beta=true");
    let warmup = client
        .head(format!("{base}/api/hello"))
        .send()
        .await
        .unwrap();
    assert_eq!(warmup.status(), 204);

    let mut payload = json!({
        "model": "claude/claude-model",
        "system": [{"type":"text","text":"fixture"}],
        "messages": [{"role":"user","content":"hello"}],
    });
    let rejected = client.post(&endpoint).json(&payload).send().await.unwrap();
    assert_eq!(rejected.status(), 401);

    let response = client
        .post(&endpoint)
        .bearer_auth(GATEWAY_KEY)
        .header("x-api-key", "caller-credential")
        .header("anthropic-version", "2026-10-21")
        .header("anthropic-beta", "context-1m-2025-08-07")
        .header("anthropic-fixture-feature", "future-capability")
        .header("anthropic-auth-token", "caller-credential")
        .header("anthropic-workspace-id", "caller-workspace")
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"input_tokens":17})
    );

    payload["model"] = json!("responses/responses-model");
    let response = client
        .post(&endpoint)
        .bearer_auth(GATEWAY_KEY)
        .header("anthropic-beta", "context-1m-2025-08-07")
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"input_tokens":21})
    );

    payload["model"] = json!("chat/chat-model");
    let translated = client
        .post(&endpoint)
        .header("x-api-key", GATEWAY_KEY)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(translated.status(), 404);
    child.kill().await.unwrap();
    mock.abort();

    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 2);
    let hit = &hits[0];
    assert_eq!(hit.path, "/v1/messages/count_tokens");
    assert_eq!(hit.headers["x-api-key"], "messages-vendor-fixture");
    assert!(!hit.headers.contains_key("authorization"));
    assert!(!hit.headers.contains_key("anthropic-auth-token"));
    assert!(!hit.headers.contains_key("anthropic-workspace-id"));
    assert_eq!(hit.headers["anthropic-beta"], "context-1m-2025-08-07");
    assert_eq!(hit.headers["anthropic-version"], "2026-10-21");
    assert_eq!(
        hit.headers["anthropic-fixture-feature"],
        "future-capability"
    );
    payload["model"] = json!("claude-model");
    assert_eq!(hit.body, payload, "counting needs no inference-only fields");

    let hit = &hits[1];
    assert_eq!(hit.path, "/v1/responses/input_tokens");
    assert_eq!(
        hit.headers["authorization"],
        "Bearer responses-vendor-fixture"
    );
    assert!(!hit.headers.contains_key("anthropic-beta"));
    assert_eq!(
        hit.body,
        json!({
            "model": "responses-model",
            "instructions": "fixture",
            "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}],
        })
    );
}

#[tokio::test]
async fn desktop_gateway_preserves_cache_tool_review_and_recovery_contracts() {
    let home = Home::new();
    let (hits, mock) = mock(&home).await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let (port, mut child) = start_gateway(&home).await;
    let base = format!("http://127.0.0.1:{port}/v1");

    let models: Value = client
        .get(format!("{base}/models"))
        .bearer_auth(GATEWAY_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let model = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == "claude/claude-sonnet-5")
        .unwrap();
    assert_eq!(model["anthropic_family_tier"], "sonnet");
    assert_eq!(model["max_input_tokens"], 1_000_000);
    assert_eq!(model["supports_1m"], true);

    let mut payload = json!({
        "model":"claude/claude-sonnet-5", "max_tokens":1024, "stream":true,
        "system":[
            {"type":"text","text":"x-anthropic-billing-header: fixture"},
            {"type":"text","text":"Desktop task context", "cache_control":{"type":"ephemeral","ttl":"1h"}}
        ],
        "tools":[{"name":"read_fixture","input_schema":{"type":"object"},"defer_loading":true}],
        "messages":[
            {"role":"user","content":"Read the fixture"},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"fixture reasoning","signature":"fixture-signature"},
                {"type":"tool_use","id":"toolu_previous","name":"read_fixture","input":{}}
            ]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_previous","content":[
                {"type":"tool_reference","tool_name":"read_fixture"}
            ]}]}
        ],
        "safeguards":{"fixture":"review this action"}
    });
    let send = |payload: &Value| {
        client
            .post(format!("{base}/messages?beta=true"))
            .bearer_auth(GATEWAY_KEY)
            .header(
                "anthropic-beta",
                "context-1m-2025-08-07,tool-search-tool-2025-10-19,dangerous-tool-use-2026-09-03",
            )
            .json(payload)
    };

    let response = send(&payload).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.text().await.unwrap(), DESKTOP_MESSAGES_SSE);

    for (status, scenario, body) in [
        (400, "fixture-context-limit", CONTEXT_ERROR),
        (402, "fixture-budget", BILLING_ERROR),
    ] {
        payload["metadata"] = json!({"user_id": scenario});
        let response = send(&payload).send().await.unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["request-id"], "req_fixture_desktop");
        assert_eq!(response.text().await.unwrap(), body);
    }
    child.kill().await.unwrap();
    mock.abort();
    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].headers["x-api-key"], "messages-vendor-fixture");
    assert_eq!(
        hits[0].headers["anthropic-beta"],
        "context-1m-2025-08-07,tool-search-tool-2025-10-19,dangerous-tool-use-2026-09-03"
    );
    payload["model"] = json!("claude-sonnet-5");
    payload.as_object_mut().unwrap().remove("metadata");
    assert_eq!(hits[0].body, payload);
}

#[tokio::test]
async fn live_doctor_checks_all_three_wires_without_discovery() {
    let home = Home::new();
    let (hits, mock) = mock(&home).await;
    let offline = home.command().arg("doctor").output().await.unwrap();
    assert!(offline.status.success());
    assert!(hits.lock().unwrap().is_empty());
    for provider in ["responses", "claude", "chat"] {
        let output = home
            .command()
            .args(["doctor", "--live", "--provider", provider])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("streamed text"));
    }
    let denied = home
        .command()
        .args([
            "doctor",
            "--live",
            "--provider",
            "responses",
            "--model",
            "rejected-model",
        ])
        .output()
        .await
        .unwrap();
    let error = String::from_utf8_lossy(&denied.stderr);
    assert!(!denied.status.success());
    assert!(error.contains("HTTP 401"), "{error}");
    assert!(!error.contains("responses-vendor-fixture"), "{error}");
    mock.abort();
    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 4);
    assert_eq!(
        hits[0].headers["authorization"],
        "Bearer responses-vendor-fixture"
    );
    assert_eq!(hits[1].headers["x-api-key"], "messages-vendor-fixture");
    assert_eq!(
        hits[2].headers["authorization"],
        "Bearer chat-vendor-fixture"
    );
    for hit in hits.iter() {
        assert_eq!(hit.body["stream"], true);
        assert!(!hit.body.to_string().contains("fixture"));
    }
}

#[tokio::test]
async fn serve_requires_an_explicit_private_gateway_key() {
    let home = Home::new();
    for key in ["", "short key"] {
        let output = home
            .command()
            .arg("serve")
            .env("OX_GATEWAY_KEY", key)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("set OX_GATEWAY_KEY"));
    }
}

async fn loopback_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
#[ignore = "opt-in: set CODEOX_TEST_CODEX_BIN and OX_TEST_CLAUDE_BIN to stock clients"]
async fn stock_app_server_and_claude_use_the_same_fixed_gateway() {
    let home = Home::new();
    let (hits, mock) = mock(&home).await;
    let (port, mut gateway) = start_gateway(&home).await;
    let origin = format!("http://127.0.0.1:{port}");

    let codex_home = Home::new();
    let config = include_str!("../examples/codex-gateway.toml")
        .replace("https://ox.example-tailnet.ts.net", &origin)
        .replace(
            "openrouter/openai/gpt-5.4-mini",
            "responses/responses-model",
        );
    std::fs::write(codex_home.0.join("config.toml"), config).unwrap();
    let project = Home::new();
    let mut codex = stock_client("CODEOX_TEST_CODEX_BIN", &codex_home)
        .arg("app-server")
        .env("CODEX_HOME", &codex_home.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = codex.stdin.take().unwrap();
    let mut events = BufReader::new(codex.stdout.take().unwrap()).lines();
    let mut send = async |message: Value| {
        input
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap()
    };
    send(json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"ox-gateway-test","version":"0.1.0"}}})).await;
    rpc_result(&mut events, 1).await;
    send(json!({"method":"initialized"})).await;
    send(json!({"id":2,"method":"model/list","params":{"limit":100}})).await;
    let catalog = rpc_result(&mut events, 2).await;
    assert!(
        catalog["data"].as_array().is_some_and(|models| {
            models.iter().any(|model| {
                model["id"] == "responses/responses-model"
                    || model["model"] == "responses/responses-model"
            })
        }),
        "the app server did not discover ox's model: {catalog}"
    );
    send(json!({"id":3,"method":"thread/start",
        "params":{"cwd":project.0,"model":"responses/responses-model","modelProvider":"ox","ephemeral":true}})).await;
    let thread = rpc_result(&mut events, 3).await;
    let thread_id = thread["thread"]["id"].as_str().unwrap();
    send(
        json!({"id":4,"method":"turn/start","params":{"threadId":thread_id,
        "input":[{"type":"text","text":"Reply OK","text_elements":[]}]}}),
    )
    .await;
    let mut received = false;
    loop {
        let event = next_rpc(&mut events).await;
        match event["method"].as_str() {
            Some("item/agentMessage/delta") => received |= event["params"]["delta"] == "OK",
            Some("turn/completed") => {
                assert_eq!(event["params"]["turn"]["status"], "completed", "{event}");
                break;
            }
            _ => assert!(event.get("error").is_none(), "{event}"),
        }
    }
    assert!(received, "the app server did not stream the vendor's text");
    codex.kill().await.unwrap();

    let claude_home = Home::new();
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        stock_client("OX_TEST_CLAUDE_BIN", &claude_home)
            .env("CLAUDE_CONFIG_DIR", &claude_home.0)
            .env("ANTHROPIC_BASE_URL", &origin)
            .env("ANTHROPIC_AUTH_TOKEN", GATEWAY_KEY)
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1")
            .current_dir(&project.0)
            .args(["-p", "Reply OK", "--model", "claude/claude-model"])
            .output(),
    )
    .await
    .expect("Claude Code did not finish")
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "OK");
    let cache_path = claude_home.0.join("cache/gateway-models.json");
    let cache: Value = serde_json::from_slice(
        &std::fs::read(&cache_path).expect("stock Claude Code did not cache the ox catalog"),
    )
    .unwrap();
    assert_eq!(cache["baseUrl"], origin);
    let discovered = cache["models"].as_array().unwrap();
    for alias in ["claude/claude-model", "responses/claude-alternate"] {
        assert!(
            discovered.iter().any(|model| model["id"] == alias),
            "missing discovered model {alias}: {discovered:?}"
        );
    }
    gateway.kill().await.unwrap();
    mock.abort();

    let hits = hits.lock().unwrap();
    assert!(
        hits.iter()
            .any(|hit| hit.path == "/v1/responses" && hit.body["model"] == "responses-model")
    );
    assert!(
        hits.iter()
            .any(|hit| hit.path == "/v1/messages" && hit.body["model"] == "claude-model")
    );
}

fn stock_client(variable: &str, home: &Home) -> Command {
    let mut command = Command::new(std::env::var_os(variable).expect(variable));
    command
        .env_clear()
        .env("HOME", &home.0)
        .env("PATH", "/usr/bin:/bin")
        .env("OX_GATEWAY_KEY", GATEWAY_KEY)
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .kill_on_drop(true);
    command
}

async fn next_rpc(lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(25), lines.next_line())
        .await
        .expect("timed out waiting for the Codex app server")
        .unwrap()
        .expect("Codex app server closed its output");
    serde_json::from_str(&line).unwrap()
}

async fn rpc_result(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> Value {
    loop {
        let reply = next_rpc(lines).await;
        if reply["id"] == id {
            assert!(reply.get("error").is_none(), "{reply}");
            return reply["result"].clone();
        }
    }
}
