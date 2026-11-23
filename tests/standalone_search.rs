//! Offline tests for the Codex standalone-search endpoint bridge.
//!
//! Every Brave request goes to a loopback mock. The relay's POST body is
//! captured so tests can prove that Codex's `input` conversation tail is never
//! forwarded. `src/search.rs` and `src/standalone_search.rs` are included
//! directly, so this slice is testable before the launcher wires the modules.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[path = "../src/search.rs"]
mod search;
#[path = "../src/standalone_search.rs"]
mod standalone_search;

use search::{BraveSearch, SearchConfig};
use serde_json::{Value, json};
use standalone_search::{
    ENDPOINT_PATH, EndpointReply, MAX_OUTPUT_BYTES, REF_ID_PREFIX, SearchEndpoint,
};

const LLM_CONTEXT: &str = include_str!("fixtures/brave/llm_context.json");
const WEB_SEARCH: &str = include_str!("fixtures/brave/web_search.json");
const TEST_KEY: &str = "test-subscription-key";
const SECRET_INPUT: &str = "SECRET conversation tail must never reach Brave";

/// Mirror of `codex_api::SearchResponse` at rust-v0.171.0, used to prove the
/// bridge reply decodes with the pinned official DTO.
#[derive(Debug, serde::Deserialize, PartialEq)]
struct CodexSearchResponse {
    encrypted_output: Option<String>,
    output: String,
    #[serde(default)]
    results: Option<Vec<Value>>,
}

#[derive(Clone, Debug)]
struct Recorded {
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: String,
}

enum Reply {
    Json(u16, String),
    Stall(Duration),
}

struct Mock {
    addr: SocketAddr,
    state: Arc<State>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct State {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Recorded>>,
}

impl Mock {
    fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
        let addr = listener.local_addr().expect("mock address");
        let state = Arc::new(State {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            for incoming in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match incoming {
                    Ok(stream) => handle(stream, &thread_state),
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle(mut stream: TcpStream, state: &State) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some((head, body)) = read_request(&mut stream) else {
        return;
    };
    let mut lines = head.split("\r\n");
    let target = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut query = HashMap::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(key), percent_decode(value));
    }
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    state.requests.lock().unwrap().push(Recorded {
        path: path.to_string(),
        query,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    });

    let reply = state
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(Reply::Json(
            500,
            r#"{"error":{"code":"MOCK_EXHAUSTED"}}"#.to_string(),
        ));
    match reply {
        Reply::Json(status, body) => write_response(&mut stream, status, body.as_bytes()),
        Reply::Stall(delay) => {
            thread::sleep(delay);
            let _ = stream.write_all(b"HTTP/1.1 504 Mock Timeout\r\nContent-Length: 0\r\n\r\n");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut body = buffer[header_end + 4..].to_vec();
    let length = head
        .split("\r\n")
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while body.len() < length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Some((head, body))
}

fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn client(mock: &Mock) -> BraveSearch {
    BraveSearch::new(
        TEST_KEY,
        SearchConfig {
            base_url: mock.base_url(),
            timeout: Duration::from_secs(2),
            ..SearchConfig::default()
        },
    )
    .expect("test client")
}

fn request_body(queries: Value, settings: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "search-session",
        "model": "test-model",
        "reasoning": {"effort": "low"},
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": SECRET_INPUT}]
        }],
        "commands": {"search_query": queries},
        "settings": settings,
        "max_output_tokens": 2048,
    }))
    .expect("request JSON")
}

fn live_settings() -> Value {
    json!({"external_web_access": true})
}

fn error_code(reply: &EndpointReply) -> String {
    reply.json().expect("error JSON")["error"]["code"]
        .as_str()
        .expect("error code")
        .to_string()
}

fn error_message(reply: &EndpointReply) -> String {
    reply.json().expect("error JSON")["error"]["message"]
        .as_str()
        .expect("error message")
        .to_string()
}

#[tokio::test]
async fn live_search_returns_codex_shape_and_ignores_conversation_input() {
    let mock = Mock::new(vec![
        Reply::Json(200, LLM_CONTEXT.to_string()),
        Reply::Json(200, LLM_CONTEXT.to_string()),
    ]);
    let brave = client(&mock);
    let endpoint = SearchEndpoint::new(&brave);
    let body = request_body(
        json!([{"q": "  rust   language "}]),
        json!({
            "external_web_access": true,
            "user_location": {"type": "approximate", "country": "NO", "city": "Oslo"},
            "search_context_size": "medium",
            "allowed_callers": ["direct"],
            "filters": {"blocked_domains": []},
        }),
    );

    let first = endpoint.handle(&body).await;
    assert_eq!(first.status, 200);
    assert!(first.is_success());

    // Same request, same deterministic reply.
    let second = endpoint.handle(&body).await;
    assert_eq!(first, second);

    let pinned: CodexSearchResponse =
        serde_json::from_slice(&first.body).expect("pinned Codex DTO");
    assert_eq!(pinned.encrypted_output, None);
    assert_eq!(pinned.results.as_ref().map(Vec::len), Some(2));

    let value = first.json().expect("success JSON");
    assert_eq!(pinned.output, value["output"].as_str().unwrap());
    assert_eq!(value["encrypted_output"], Value::Null);
    assert_eq!(
        value["output"],
        concat!(
            "Web search results for: rust language\n\n",
            "[turn0search0] Learn Rust\n",
            "https://www.rust-lang.org/learn\n",
            "Published: 2024-01-15\n",
            "Rust is a fast, reliable programming language.\n",
            "The Rust Book is the primary learning resource.\n\n",
            "[turn0search1] The Rust Programming Language\n",
            "https://doc.rust-lang.org/book/\n",
            "The Rust Programming Language book."
        )
    );
    assert_eq!(
        value["results"],
        json!([
            {
                "type": "text_result",
                "ref_id": "turn0search0",
                "url": "https://www.rust-lang.org/learn",
                "title": "Learn Rust",
                "snippet": "Rust is a fast, reliable programming language. The Rust Book is the primary learning resource.",
            },
            {
                "type": "text_result",
                "ref_id": "turn0search1",
                "url": "https://doc.rust-lang.org/book/",
                "title": "The Rust Programming Language",
                "snippet": "The Rust Programming Language book.",
            },
        ])
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let request = &requests[0];
    assert_eq!(request.path, "/res/v1/llm/context");
    assert_eq!(
        request
            .headers
            .get("x-subscription-token")
            .map(String::as_str),
        Some(TEST_KEY)
    );
    assert_eq!(
        request.query.get("q").map(String::as_str),
        Some("rust language")
    );
    assert_eq!(request.query.get("country").map(String::as_str), Some("no"));
    assert_eq!(request.query.get("count").map(String::as_str), Some("8"));
    assert!(
        !request
            .query
            .get("q")
            .is_some_and(|q| q.contains(SECRET_INPUT)),
        "conversation input must not be forwarded"
    );
    // The relay received the conversation tail but never re-emitted it.
    assert!(String::from_utf8_lossy(&body).contains(SECRET_INPUT));
    assert!(!request.body.contains(SECRET_INPUT));

    assert_eq!(ENDPOINT_PATH, "/v1/alpha/search");
    assert_eq!(REF_ID_PREFIX, "turn0search");
}

#[tokio::test]
async fn endpoint_accepts_live_mode_string_and_falls_back_once() {
    let mock = Mock::new(vec![
        Reply::Json(
            400,
            r#"{"error":{"code":"OPTION_NOT_IN_PLAN"}}"#.to_string(),
        ),
        Reply::Json(200, WEB_SEARCH.to_string()),
    ]);
    let brave = client(&mock);
    let reply = SearchEndpoint::new(&brave)
        .handle(&request_body(
            json!([{"q": "rust"}]),
            json!({"external_web_access": "live"}),
        ))
        .await;

    assert_eq!(reply.status, 200);
    let value = reply.json().unwrap();
    assert!(
        value["output"]
            .as_str()
            .unwrap()
            .contains("[turn0search0] Rust Programming Language")
    );
    assert!(
        value["output"]
            .as_str()
            .unwrap()
            .contains("Published: 2024-02-03")
    );

    let requests = mock.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        ["/res/v1/llm/context", "/res/v1/web/search"]
    );
    assert_eq!(
        requests[1].query.get("extra_snippets").map(String::as_str),
        Some("true")
    );
    assert_eq!(brave.take_fallback_notice(), Some(search::FALLBACK_NOTICE));
}

#[tokio::test]
async fn only_search_query_is_executed() {
    for operation in [
        "literature_search_query",
        "image_query",
        "open",
        "click",
        "find",
        "screenshot",
        "finance",
        "weather",
        "sports",
        "time",
        "future_operation",
    ] {
        let mock = Mock::new(Vec::new());
        let brave = client(&mock);
        let body = serde_json::to_vec(&json!({
            "commands": {operation: [{"q": "rust"}]},
            "settings": live_settings(),
        }))
        .unwrap();

        let reply = SearchEndpoint::new(&brave).handle(&body).await;
        assert_eq!(reply.status, 400, "{operation}");
        assert_eq!(error_code(&reply), "unsupported_operation", "{operation}");
        assert!(
            error_message(&reply).contains(operation),
            "message should name the operation: {}",
            error_message(&reply)
        );
        assert!(
            mock.requests().is_empty(),
            "{operation} must not reach Brave"
        );
    }
}

#[tokio::test]
async fn non_live_policies_and_missing_settings_fail_closed() {
    for (settings, expected_status, expected_code) in [
        (
            json!({"external_web_access": false}),
            403,
            "unsupported_policy",
        ),
        (
            json!({"external_web_access": "cached"}),
            403,
            "unsupported_policy",
        ),
        (
            json!({"external_web_access": "indexed"}),
            403,
            "unsupported_policy",
        ),
        (json!({}), 400, "invalid_request"),
        (Value::Null, 400, "invalid_request"),
        (
            json!({"external_web_access": true, "allowed_callers": ["shell"]}),
            403,
            "unsupported_policy",
        ),
        (
            json!({"external_web_access": "unknown"}),
            400,
            "invalid_request",
        ),
    ] {
        let mock = Mock::new(Vec::new());
        let brave = client(&mock);
        let reply = SearchEndpoint::new(&brave)
            .handle(&request_body(json!([{"q": "rust"}]), settings))
            .await;
        assert_eq!(reply.status, expected_status);
        assert_eq!(error_code(&reply), expected_code);
        assert!(mock.requests().is_empty());
    }
}

#[tokio::test]
async fn recency_uses_exact_brave_buckets_or_fails_closed() {
    for (days, freshness) in [(1, "pd"), (7, "pw"), (30, "pm"), (365, "py")] {
        let mock = Mock::new(vec![Reply::Json(200, LLM_CONTEXT.to_string())]);
        let brave = client(&mock);
        let reply = SearchEndpoint::new(&brave)
            .handle(&request_body(
                json!([{"q": "rust", "recency": days}]),
                live_settings(),
            ))
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(
            mock.requests()[0]
                .query
                .get("freshness")
                .map(String::as_str),
            Some(freshness)
        );
    }

    for days in [0, 2, 3, 14, 90] {
        let mock = Mock::new(Vec::new());
        let brave = client(&mock);
        let reply = SearchEndpoint::new(&brave)
            .handle(&request_body(
                json!([{"q": "rust", "recency": days}]),
                live_settings(),
            ))
            .await;
        assert_eq!(reply.status, 400);
        assert_eq!(error_code(&reply), "unsupported_recency");
        assert!(mock.requests().is_empty());
    }
}

#[tokio::test]
async fn domain_constraints_become_site_filters() {
    let results = json!({"grounding": {"generic": [
        {"url": "https://docs.rust-lang.org/guide", "title": "Allowed", "snippets": ["one"]},
        {"url": "https://docs.rust-lang.org.example.net/", "title": "Outside", "snippets": ["two"]},
        {"url": "https://fakedocs.rust-lang.org/", "title": "Outside", "snippets": ["three"]},
        {"url": "https://www.rust-lang.org/", "title": "Global only", "snippets": ["four"]}
    ]}});
    let mock = Mock::new(vec![Reply::Json(200, results.to_string())]);
    let brave = client(&mock);
    let reply = SearchEndpoint::new(&brave)
        .handle(&request_body(
            json!([{"q": "rust", "domains": ["Docs.Rust-Lang.org"]}]),
            json!({
                "external_web_access": true,
                "filters": {"allowed_domains": ["Rust-Lang.org"]},
            }),
        ))
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        mock.requests()[0].query.get("q").map(String::as_str),
        Some("rust site:rust-lang.org site:docs.rust-lang.org")
    );
    let body = reply.json().unwrap();
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["url"], "https://docs.rust-lang.org/guide");
    assert!(!body["output"].as_str().unwrap().contains("Outside"));

    let mock = Mock::new(Vec::new());
    let brave = client(&mock);
    let reply = SearchEndpoint::new(&brave)
        .handle(&request_body(
            json!([{"q": "rust"}]),
            json!({
                "external_web_access": true,
                "filters": {"blocked_domains": ["example.com"]},
            }),
        ))
        .await;
    assert_eq!(reply.status, 403);
    assert_eq!(error_code(&reply), "unsupported_policy");
    assert!(mock.requests().is_empty());

    for (domains, expected) in [
        (vec!["https://rust-lang.org".to_string()], "invalid_request"),
        (vec!["rust lang.org".to_string()], "invalid_request"),
        (
            (0..9)
                .map(|index| format!("site{index}.example.com"))
                .collect::<Vec<_>>(),
            "too_many_domains",
        ),
        (vec![], "invalid_request"),
    ] {
        let mock = Mock::new(Vec::new());
        let brave = client(&mock);
        let reply = SearchEndpoint::new(&brave)
            .handle(&request_body(
                json!([{"q": "rust", "domains": domains}]),
                live_settings(),
            ))
            .await;
        assert_eq!(reply.status, 400);
        assert_eq!(error_code(&reply), expected);
        assert!(mock.requests().is_empty());
    }
}

#[tokio::test]
async fn query_count_is_capped_and_reference_ids_stay_unique() {
    let replies: Vec<Reply> = (0..4)
        .map(|_| Reply::Json(200, LLM_CONTEXT.to_string()))
        .collect();
    let mock = Mock::new(replies);
    let brave = client(&mock);
    let reply = SearchEndpoint::new(&brave)
        .handle(&request_body(
            json!([
                {"q": "one"},
                {"q": "two"},
                {"q": "three"},
                {"q": "four"},
            ]),
            live_settings(),
        ))
        .await;
    assert_eq!(reply.status, 200);
    let value = reply.json().unwrap();
    let results = value["results"].as_array().unwrap();
    assert_eq!(results.len(), 8);
    let refs: Vec<&str> = results
        .iter()
        .map(|result| result["ref_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        refs,
        [
            "turn0search0",
            "turn0search1",
            "turn0search2",
            "turn0search3",
            "turn0search4",
            "turn0search5",
            "turn0search6",
            "turn0search7"
        ]
    );
    let output = value["output"].as_str().unwrap();
    for query in ["one", "two", "three", "four"] {
        assert!(output.contains(&format!("Web search results for: {query}")));
    }
    assert!(output.len() <= MAX_OUTPUT_BYTES);
    assert_eq!(mock.requests().len(), 4);

    let mock = Mock::new(Vec::new());
    let brave = client(&mock);
    let reply = SearchEndpoint::new(&brave)
        .handle(&request_body(
            json!([
                {"q": "1"},
                {"q": "2"},
                {"q": "3"},
                {"q": "4"},
                {"q": "5"},
            ]),
            live_settings(),
        ))
        .await;
    assert_eq!(reply.status, 400);
    assert_eq!(error_code(&reply), "too_many_queries");
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn invalid_requests_are_explicit_and_never_reach_brave() {
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (b"not json".to_vec(), "invalid_json"),
        (
            serde_json::to_vec(&json!({
                "commands": {"search_query": [{"q": "rust", "freshness": "pd"}]},
                "settings": live_settings(),
            }))
            .unwrap(),
            "invalid_request",
        ),
        (
            serde_json::to_vec(&json!({
                "commands": {"search_query": [{"q": "   "}]},
                "settings": live_settings(),
            }))
            .unwrap(),
            "invalid_request",
        ),
        (
            serde_json::to_vec(&json!({
                "commands": {"search_query": []},
                "settings": live_settings(),
            }))
            .unwrap(),
            "invalid_request",
        ),
        (
            serde_json::to_vec(&json!({
                "commands": {"response_length": "long"},
                "settings": live_settings(),
            }))
            .unwrap(),
            "invalid_request",
        ),
        (
            serde_json::to_vec(&json!({
                "commands": {"search_query": [{"q": "rust"}]},
                "settings": {"external_web_access": true, "future_setting": 1},
            }))
            .unwrap(),
            "unsupported_setting",
        ),
    ];

    for (body, expected) in cases {
        let mock = Mock::new(Vec::new());
        let brave = client(&mock);
        let reply = SearchEndpoint::new(&brave).handle(&body).await;
        assert_eq!(reply.status, 400, "{expected}");
        assert_eq!(error_code(&reply), expected);
        assert!(mock.requests().is_empty());
    }
}

#[tokio::test]
async fn upstream_errors_are_safe_tool_errors() {
    for (status, body, expected_status, expected_code) in [
        (
            429,
            r#"{"error":{"code":"RATE_LIMITED"}}"#.to_string(),
            429,
            "upstream_rate_limited",
        ),
        (
            401,
            r#"{"error":{"code":"UNAUTHORIZED"}}"#.to_string(),
            502,
            "upstream_auth",
        ),
        (
            500,
            r#"{"error":{"code":"INTERNAL"}}"#.to_string(),
            502,
            "upstream_error",
        ),
        (
            200,
            "not a search response".to_string(),
            502,
            "upstream_bad_response",
        ),
    ] {
        let mock = Mock::new(vec![Reply::Json(status, body)]);
        let brave = client(&mock);
        let reply = SearchEndpoint::new(&brave)
            .handle(&request_body(json!([{"q": "rust"}]), live_settings()))
            .await;
        assert_eq!(reply.status, expected_status, "{expected_code}");
        assert_eq!(error_code(&reply), expected_code);
        let text = String::from_utf8_lossy(&reply.body);
        assert!(!text.contains(TEST_KEY), "errors must not leak the key");
        assert!(
            !text.contains("RATE_LIMITED") && !text.contains("UNAUTHORIZED"),
            "errors must not echo provider bodies: {text}"
        );
    }
}

#[tokio::test]
async fn output_and_result_sets_are_bounded() {
    let mut generic = Vec::new();
    let mut sources = serde_json::Map::new();
    for index in 0..20 {
        let url = format!("https://example.com/{index}");
        generic.push(json!({
            "url": url,
            "title": format!("Result {index}"),
            "snippets": [
                "x".repeat(900),
                "y".repeat(900),
            ],
        }));
        sources.insert(url, json!({"title": format!("Result {index}"), "age": []}));
    }
    let body = json!({
        "grounding": {"generic": generic},
        "sources": sources,
    })
    .to_string();

    let mock = Mock::new(vec![Reply::Json(200, body)]);
    let brave = client(&mock);
    let reply = SearchEndpoint::new(&brave)
        .handle(&request_body(json!([{"q": "rust"}]), live_settings()))
        .await;
    assert_eq!(reply.status, 200);
    let value = reply.json().unwrap();
    let output = value["output"].as_str().unwrap();
    assert!(output.len() <= MAX_OUTPUT_BYTES);
    assert!(output.contains("more sources omitted"));
    let results = value["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert!(results.len() < 20);
    let refs: std::collections::HashSet<&str> = results
        .iter()
        .map(|result| result["ref_id"].as_str().unwrap())
        .collect();
    assert_eq!(refs.len(), results.len());
}

#[tokio::test]
async fn request_timeout_is_a_bounded_tool_error() {
    let mock = Mock::new(vec![Reply::Stall(Duration::from_millis(400))]);
    let brave = BraveSearch::new(
        TEST_KEY,
        SearchConfig {
            base_url: mock.base_url(),
            timeout: Duration::from_millis(80),
            ..SearchConfig::default()
        },
    )
    .expect("test client");

    let started = Instant::now();
    let reply = SearchEndpoint::new(&brave)
        .handle(&request_body(json!([{"q": "rust"}]), live_settings()))
        .await;
    assert_eq!(reply.status, 504);
    assert_eq!(error_code(&reply), "upstream_timeout");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn production_path_matches_codex_provider_relative_endpoint() {
    assert_eq!(ENDPOINT_PATH, "/v1/alpha/search");
    assert!(ENDPOINT_PATH.ends_with(standalone_search::PROVIDER_RELATIVE_PATH));
}
