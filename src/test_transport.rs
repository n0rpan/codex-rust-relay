//! Loopback transport-failure tests.
//!
//! These exercise the real `App`/router over a real loopback socket against a
//! mock vendor, and differ from the request-shape tests by concentrating on
//! connection lifecycle:
//!
//! * dropping the downstream response must promptly release the upstream
//!   request (the relay must not keep a vendor stream alive for a client that
//!   is gone), and
//! * a vendor that goes silent mid-stream must fail within a bounded deadline
//!   instead of hanging the Codex request forever.
//!
//! The short upstream read timeout is injected the way production injects its
//! client: through `App::codeox`. Only tests construct a client with a
//! non-production timeout; no production default changes.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{self, Stream, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::catalog::BundledModels;
use crate::provider::CodeoxConfig;
use crate::{App, serve};

const RELAY_TOKEN: &str = "relay-test-token";
const DUMMY_KEY: &str = "dummy-alpha-key";
/// Longest any single test phase may take. Everything here is loopback, so a
/// healthy run finishes in well under a second; this only stops a regression
/// from wedging the suite.
const TEST_DEADLINE: Duration = Duration::from_secs(5);
/// How long the upstream read timeout (injected into the relay client) gets to
/// fire. Kept short so the stall tests finish quickly.
const STALL_TIMEOUT: Duration = Duration::from_millis(250);
/// The whole point of the disconnect tests: releasing the upstream request
/// must take about as long as one downstream write, not linger for the vendor.
const RELEASE_DEADLINE: Duration = Duration::from_secs(3);

// The mock streams roughly this much text per chunk; large enough that a
// disconnected downstream socket is noticed on the next write or two instead
// of after the kernel send buffer drains byte by byte.
const CHUNK_TEXT_BYTES: usize = 24 * 1024;
const CHUNK_INTERVAL: Duration = Duration::from_millis(15);

// ---------------------------------------------------------------------------
// Relay under test (real App + real serve loop, loopback only)
// ---------------------------------------------------------------------------

struct Relay {
    base: String,
    task: JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A relay client shaped like production's (no redirects) but with a short,
/// test-configurable read timeout. This is the only transport knob the tests
/// add, and it lives entirely in test code.
fn relay_client(read_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(read_timeout)
        .build()
        .unwrap()
}

fn bundled_fixture() -> BundledModels {
    serde_json::from_value(json!({
        "models": [{"slug": "bundled-default", "visibility": "list"}]
    }))
    .unwrap()
}

fn config(base_url: &str, wire: Option<&str>) -> String {
    let wire_line = wire
        .map(|wire| format!("wire_api = \"{wire}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
[providers.alpha]
name = "Alpha"
base_url = "{base_url}"
{wire_line}env_key = "ALPHA_KEY"
discovery = false

[[providers.alpha.models]]
id = "upstream-model"
name = "Upstream Model"
context_window = 200000
"#
    )
}

async fn start_relay(config: &str, read_timeout: Duration) -> Relay {
    let environment = |name: &str| (name == "ALPHA_KEY").then(|| DUMMY_KEY.to_owned());
    let app = App::codeox(
        CodeoxConfig::load_from_str(config).unwrap(),
        bundled_fixture(),
        RELAY_TOKEN,
        relay_client(read_timeout),
        crate::MAX_REQUEST_BYTES,
        &environment,
        None,
    )
    .await
    .unwrap();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = serve(listener, Arc::new(app)).await;
    });
    Relay {
        base: format!("http://{addr}/v1"),
        task,
    }
}

/// Downstream (Codex-side) client. The request timeout is a backstop so a hung
/// relay fails the test instead of the harness.
fn downstream_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

fn request_body(model: &str) -> Value {
    json!({
        "model": model,
        "instructions": "You are a coding agent.",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "cache",
        "reasoning": {"effort": "medium", "summary": "auto"},
        "parallel_tool_calls": false,
        "tool_choice": "auto"
    })
}

// ---------------------------------------------------------------------------
// Mock vendor
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Payload {
    /// Chat Completions chunks, translated by the relay into Responses events.
    Chat,
    /// Raw Responses SSE, forwarded unchanged by the relay.
    Responses,
}

#[derive(Clone, Copy, Debug)]
enum MockBehavior {
    /// Stream one chunk every `CHUNK_INTERVAL` until the connection dies.
    Endless(Payload),
    /// Send one chunk, then never write again (a stalled vendor).
    StallAfterFirst(Payload),
}

#[derive(Debug)]
struct MockHit {
    path: String,
    authorization: Option<String>,
    body: Option<Value>,
}

#[derive(Default)]
struct MockState {
    hits: Mutex<Vec<MockHit>>,
    /// Set when the server-side HTTP connection ends, i.e. when the relay
    /// released the upstream request and dropped its socket.
    closed_at: Mutex<Option<Instant>>,
}

impl MockState {
    fn record_close(&self) {
        self.closed_at
            .lock()
            .unwrap()
            .get_or_insert_with(Instant::now);
    }
}

struct MockVendor {
    base: String,
    state: Arc<MockState>,
    task: JoinHandle<()>,
}

impl Drop for MockVendor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockVendor {
    async fn spawn(behavior: MockBehavior) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(MockState::default());
        let task = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        break;
                    };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let service_state = Arc::clone(&state);
                        let service = service_fn(move |request| {
                            let state = Arc::clone(&service_state);
                            async move {
                                Ok::<_, Infallible>(mock_response(behavior, &state, request).await)
                            }
                        });
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(socket), service)
                            .await;
                        state.record_close();
                    });
                }
            }
        });
        Self {
            base: format!("http://{addr}/v1"),
            state,
            task,
        }
    }

    fn hits(&self) -> usize {
        self.state.hits.lock().unwrap().len()
    }

    fn authorization(&self) -> Option<String> {
        self.state
            .hits
            .lock()
            .unwrap()
            .first()
            .and_then(|hit| hit.authorization.clone())
    }

    fn first_hit(&self) -> (String, Option<Value>) {
        let hits = self.state.hits.lock().unwrap();
        let hit = hits.first().expect("an upstream request");
        (hit.path.clone(), hit.body.clone())
    }

    /// Wait, with a bounded deadline, for the relay to release the connection.
    async fn wait_released(&self, limit: Duration) -> Instant {
        tokio::time::timeout(limit, async {
            loop {
                if let Some(at) = *self.state.closed_at.lock().unwrap() {
                    return at;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the relay must release the upstream request")
    }
}

async fn mock_response(
    behavior: MockBehavior,
    state: &MockState,
    request: Request<Incoming>,
) -> Response<http_body_util::combinators::UnsyncBoxBody<Bytes, Infallible>> {
    let (parts, body) = request.into_parts();
    let bytes = body
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    state.hits.lock().unwrap().push(MockHit {
        path: parts.uri.path().to_owned(),
        authorization: parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body: serde_json::from_slice(&bytes).ok(),
    });

    let stream: Pin<Box<dyn Stream<Item = Result<Frame<Bytes>, Infallible>> + Send>> =
        match behavior {
            MockBehavior::Endless(payload) => Box::pin(endless(payload)),
            MockBehavior::StallAfterFirst(payload) => {
                Box::pin(once_then_silence(chunk(payload, 0)))
            }
        };
    let mut response = Response::new(StreamBody::new(stream).boxed_unsync());
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response
}

fn endless(payload: Payload) -> impl Stream<Item = Result<Frame<Bytes>, Infallible>> + Send {
    stream::unfold(0usize, move |index| async move {
        if index > 0 {
            tokio::time::sleep(CHUNK_INTERVAL).await;
        }
        Some((Ok(Frame::data(chunk(payload, index))), index + 1))
    })
}

fn once_then_silence(first: Bytes) -> impl Stream<Item = Result<Frame<Bytes>, Infallible>> + Send {
    stream::once(async move { Ok(Frame::data(first)) })
        .chain(stream::pending::<Result<Frame<Bytes>, Infallible>>())
}

fn chunk(payload: Payload, index: usize) -> Bytes {
    let text = "x".repeat(CHUNK_TEXT_BYTES);
    match payload {
        Payload::Chat => {
            let value = json!({
                "id": format!("chatcmpl-mock-{index}"),
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "upstream-model",
                "choices": [{
                    "index": 0,
                    "delta": {"content": text},
                    "finish_reason": null
                }]
            });
            Bytes::from(format!("data: {value}\n\n"))
        }
        Payload::Responses => {
            let value = json!({
                "type": "response.output_text.delta",
                "sequence_number": index,
                "delta": text,
            });
            Bytes::from(format!(
                "event: response.output_text.delta\ndata: {value}\n\n"
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

async fn post_streaming(relay: &Relay, client: &reqwest::Client, model: &str) -> reqwest::Response {
    let response = tokio::time::timeout(
        TEST_DEADLINE,
        client
            .post(format!("{}/responses", relay.base))
            .bearer_auth(RELAY_TOKEN)
            .json(&request_body(model))
            .send(),
    )
    .await
    .expect("the relay must answer within the test deadline")
    .expect("the relay must accept the request");
    assert_eq!(response.status(), StatusCode::OK);
    response
}

/// Drop the downstream body after one chunk and require the relay to release
/// the vendor request promptly.
async fn assert_disconnect_releases(
    behavior: MockBehavior,
    wire: Option<&str>,
    upstream_path: &str,
) {
    let mock = MockVendor::spawn(behavior).await;
    // A long read timeout keeps the stall path out of this test entirely: the
    // only thing that can end the vendor connection is the downstream drop.
    let relay = start_relay(&config(&mock.base, wire), Duration::from_secs(60)).await;
    let client = downstream_client();

    let response = post_streaming(&relay, &client, "alpha/upstream-model").await;
    let mut body = response.bytes_stream();
    let first = tokio::time::timeout(TEST_DEADLINE, body.next())
        .await
        .expect("the first downstream chunk must arrive")
        .expect("the body must yield a chunk")
        .expect("the first downstream chunk must be readable");
    assert!(!first.is_empty());
    assert_eq!(
        mock.authorization().as_deref(),
        Some("Bearer dummy-alpha-key")
    );
    assert_eq!(mock.hits(), 1, "exactly one upstream request was made");
    let (path, upstream_body) = mock.first_hit();
    assert_eq!(path, upstream_path, "the vendor wire path");
    assert_eq!(
        upstream_body
            .as_ref()
            .and_then(|body| body.get("model").cloned()),
        Some(json!("upstream-model")),
        "the alias must be rewritten to the upstream model before sending"
    );

    // The client goes away mid-stream, exactly like Codex cancelling a turn.
    let dropped = Instant::now();
    drop(body);

    let closed = mock.wait_released(TEST_DEADLINE).await;
    let released = closed.duration_since(dropped);
    assert!(
        released < RELEASE_DEADLINE,
        "the upstream request was still alive {released:?} after the downstream disconnect"
    );
    eprintln!("downstream disconnect released the upstream request in {released:?}");
}

#[tokio::test]
async fn downstream_disconnect_releases_a_responses_upstream() {
    assert_disconnect_releases(
        MockBehavior::Endless(Payload::Responses),
        None,
        "/v1/responses",
    )
    .await;
}

#[tokio::test]
async fn downstream_disconnect_releases_a_translated_upstream() {
    assert_disconnect_releases(
        MockBehavior::Endless(Payload::Chat),
        Some("chat_completions"),
        "/v1/chat/completions",
    )
    .await;
}

#[tokio::test]
async fn a_stalled_translated_upstream_ends_with_response_failed() {
    let mock = MockVendor::spawn(MockBehavior::StallAfterFirst(Payload::Chat)).await;
    let relay = start_relay(&config(&mock.base, Some("chat_completions")), STALL_TIMEOUT).await;
    let client = downstream_client();

    let started = Instant::now();
    let response = post_streaming(&relay, &client, "alpha/upstream-model").await;
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "the relay must return SSE for a translated stream, got {content_type:?}"
    );

    let body = tokio::time::timeout(TEST_DEADLINE, response.text())
        .await
        .expect("a stalled upstream must not hang the downstream request")
        .expect("the relay must end the translated stream cleanly");
    let elapsed = started.elapsed();

    assert!(
        elapsed < TEST_DEADLINE,
        "the stalled stream took {elapsed:?} to fail"
    );
    assert!(
        body.contains("event: response.failed"),
        "a stalled translated stream must end in response.failed, got: {body}"
    );
    assert!(
        body.contains("\"code\":\"relay_stream_error\"") && body.contains("transport error"),
        "the failure must name the transport error, got: {body}"
    );
    assert!(
        !body.contains("event: response.completed"),
        "a stalled stream must never be reported as completed"
    );
    // The failed request must not leave the vendor socket open behind it.
    let released = mock.wait_released(TEST_DEADLINE).await;
    assert!(
        released.duration_since(started) < TEST_DEADLINE,
        "the timed-out upstream socket was not released"
    );
    eprintln!("stalled translated stream failed cleanly after {elapsed:?}");
}

#[tokio::test]
async fn a_stalled_responses_upstream_does_not_hang_the_downstream() {
    let mock = MockVendor::spawn(MockBehavior::StallAfterFirst(Payload::Responses)).await;
    let relay = start_relay(&config(&mock.base, None), STALL_TIMEOUT).await;
    let client = downstream_client();

    let started = Instant::now();
    let response = post_streaming(&relay, &client, "alpha/upstream-model").await;

    // The Responses wire is forwarded opaquely, so the relay cannot synthesize
    // a `response.failed` event. What it must guarantee is that the read
    // timeout terminates the downstream body instead of leaving Codex waiting
    // forever.
    let mut body = response.bytes_stream();
    let mut received = Vec::new();
    let outcome = tokio::time::timeout(TEST_DEADLINE, async {
        loop {
            match body.next().await {
                Some(Ok(chunk)) => received.extend_from_slice(&chunk),
                Some(Err(error)) => return Err(error.to_string()),
                None => return Ok(()),
            }
        }
    })
    .await
    .expect("a stalled pass-through upstream must not hang the downstream request");
    let elapsed = started.elapsed();

    assert!(
        elapsed < TEST_DEADLINE,
        "the stalled pass-through took {elapsed:?} to end"
    );
    assert!(
        !received.is_empty(),
        "the first forwarded event should have reached the client before the stall"
    );
    let text = String::from_utf8_lossy(&received);
    assert!(
        !text.contains("event: response.completed") && !text.contains("event: response.failed"),
        "the relay must not invent a terminal event for an opaque stream"
    );
    // Unlike the translated path there is no protocol-level failure here: the
    // relay forwards opaque bytes, so the read timeout aborts the downstream
    // body mid-chunk. Requiring the error (rather than a clean EOF) keeps a
    // regression from silently presenting a truncated answer as success.
    let error = outcome.expect_err(
        "a stalled pass-through must end in a downstream transport error, not a clean EOF",
    );
    assert!(!error.is_empty());
    // The timed-out vendor socket must be closed, not parked in the pool.
    let released = mock.wait_released(TEST_DEADLINE).await;
    assert!(
        released.duration_since(started) < TEST_DEADLINE,
        "the timed-out upstream socket was not released"
    );
    eprintln!(
        "stalled pass-through surfaced a downstream transport error after {elapsed:?}: {error}"
    );
}
