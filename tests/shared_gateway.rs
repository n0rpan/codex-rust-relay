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

async fn start_gateway(home: &Home, port: u16, client: &Client) -> Child {
    let child = home
        .command()
        .args(["serve", "--port", &port.to_string()])
        .env("OX_GATEWAY_KEY", GATEWAY_KEY)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}/v1/catalog");
    for _ in 0..100 {
        if let Ok(reply) = client.get(&url).bearer_auth(GATEWAY_KEY).send().await
            && reply.status().is_success()
        {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("persistent relay did not start");
}

#[tokio::test]
async fn a_fixed_endpoint_serves_both_client_protocols_and_survives_restart() {
    let home = Home::new();
    let (hits, mock) = mock(&home).await;
    let port = loopback_port().await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}/v1");
    let mut child = start_gateway(&home, port, &client).await;

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
    let mut child = start_gateway(&home, port, &client).await;
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
    let port = loopback_port().await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let mut gateway = start_gateway(&home, port, &client).await;
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
