//! Offline tests for the direct Brave Search client.
//!
//! Every request goes to a loopback mock; nothing here reads credentials or
//! talks to Brave. `src/search.rs` is included directly so this slice can be
//! tested before the launcher wires `mod search;`.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[path = "../src/search.rs"]
mod search;

use search::{
    BraveSearch, Endpoint, FALLBACK_NOTICE, SearchConfig, SearchError, SearchRequest,
    SearchResponse,
};

const LLM_CONTEXT: &str = include_str!("fixtures/brave/llm_context.json");
const WEB_SEARCH: &str = include_str!("fixtures/brave/web_search.json");
const TEST_KEY: &str = "test-subscription-key";

#[derive(Clone, Debug)]
struct Recorded {
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
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
    let mut head = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                head.extend_from_slice(&chunk[..read]);
                if head.windows(4).any(|window| window == b"\r\n\r\n") || head.len() > 64 * 1024 {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let head = String::from_utf8_lossy(&head);
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

fn search_titles(response: &SearchResponse) -> Vec<&str> {
    response
        .results
        .iter()
        .map(|result| result.title.as_str())
        .collect()
}

#[tokio::test]
async fn llm_context_is_ordered_citation_ready_and_bounded() {
    let mock = Mock::new(vec![Reply::Json(200, LLM_CONTEXT.to_string())]);
    let brave = client(&mock);

    let response = brave
        .search(&SearchRequest::new("  rust   language "))
        .await
        .expect("grounding search");

    assert_eq!(response.endpoint, Endpoint::LlmContext);
    assert_eq!(
        search_titles(&response),
        ["Learn Rust", "The Rust Programming Language"]
    );
    assert_eq!(response.results[0].url, "https://www.rust-lang.org/learn");
    assert_eq!(response.results[0].date.as_deref(), Some("2024-01-15"));
    assert_eq!(response.results[0].snippets.len(), 2);
    assert_eq!(response.results[1].date, None);
    assert!(!response.is_empty());
    assert!(!response.truncated());

    let rendered = response.render();
    assert!(rendered.starts_with("Web search results for: rust language"));
    assert!(rendered.contains("[1] Learn Rust"));
    assert!(rendered.contains("https://doc.rust-lang.org/book/"));
    assert!(rendered.contains("Published: 2024-01-15"));

    let request = &mock.requests()[0];
    assert_eq!(request.path, "/res/v1/llm/context");
    assert_eq!(
        request
            .headers
            .get("x-subscription-token")
            .map(String::as_str),
        Some(TEST_KEY)
    );
    assert_eq!(
        request.headers.get("accept").map(String::as_str),
        Some("application/json")
    );
    assert_eq!(
        request.query.get("q").map(String::as_str),
        Some("rust language")
    );
    assert_eq!(
        request
            .query
            .get("maximum_number_of_tokens")
            .map(String::as_str),
        Some("2048")
    );
    assert_eq!(
        request
            .query
            .get("maximum_number_of_urls")
            .map(String::as_str),
        Some("8")
    );
    assert_eq!(request.query.get("count").map(String::as_str), Some("8"));
}

#[tokio::test]
async fn falls_back_only_on_option_not_in_plan_and_remembers_it() {
    let mock = Mock::new(vec![
        Reply::Json(
            400,
            r#"{"error":{"code":"OPTION_NOT_IN_PLAN","detail":"LLM Context is not in this plan"}}"#
                .to_string(),
        ),
        Reply::Json(200, WEB_SEARCH.to_string()),
        Reply::Json(200, WEB_SEARCH.to_string()),
    ]);
    let brave = client(&mock);

    let first = brave
        .search(&SearchRequest::new("rust"))
        .await
        .expect("web fallback");
    assert_eq!(first.endpoint, Endpoint::WebSearch);
    assert_eq!(
        search_titles(&first),
        ["Rust Programming Language", "Rust Documentation"]
    );
    assert_eq!(first.results[0].date.as_deref(), Some("2024-02-03"));
    assert_eq!(
        first.results[0].snippets[0],
        "A language empowering everyone & more."
    );
    assert!(brave.fallback_active());
    assert_eq!(brave.take_fallback_notice(), Some(FALLBACK_NOTICE));
    assert_eq!(brave.take_fallback_notice(), None);

    let second = brave
        .search(&SearchRequest::new("cargo"))
        .await
        .expect("remembered web fallback");
    assert_eq!(second.endpoint, Endpoint::WebSearch);

    let requests = mock.requests();
    let paths: Vec<&str> = requests
        .iter()
        .map(|request| request.path.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "/res/v1/llm/context",
            "/res/v1/web/search",
            "/res/v1/web/search"
        ]
    );
    assert_eq!(
        requests[1].query.get("extra_snippets").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        requests[1].query.get("count").map(String::as_str),
        Some("8")
    );
    assert_eq!(
        requests[1]
            .headers
            .get("x-subscription-token")
            .map(String::as_str),
        Some(TEST_KEY)
    );
}

#[tokio::test]
async fn unrelated_400_does_not_fall_back() {
    let mock = Mock::new(vec![Reply::Json(
        400,
        r#"{"error":{"code":"SUBSCRIPTION_TOKEN_INVALID","detail":"bad token"}}"#.to_string(),
    )]);
    let brave = client(&mock);

    let error = brave
        .search(&SearchRequest::new("rust"))
        .await
        .expect_err("no fallback for an unrelated provider error");
    assert_eq!(error, SearchError::Http(400));
    assert!(!brave.fallback_active());
    assert_eq!(brave.take_fallback_notice(), None);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn auth_errors_are_stable_and_do_not_fall_back() {
    let mock = Mock::new(vec![Reply::Json(
        403,
        r#"{"error":{"code":"FORBIDDEN"}}"#.to_string(),
    )]);
    let brave = client(&mock);

    let error = brave
        .search(&SearchRequest::new("rust"))
        .await
        .expect_err("auth error");
    assert_eq!(error, SearchError::InvalidKey);
    assert_eq!(error.to_string(), "Brave Search rejected the API key");
    assert!(!brave.fallback_active());
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn grounding_limits_are_clamped_into_the_request() {
    let mock = Mock::new(vec![Reply::Json(200, LLM_CONTEXT.to_string())]);
    let brave = BraveSearch::new(
        TEST_KEY,
        SearchConfig {
            base_url: mock.base_url(),
            timeout: Duration::from_secs(2),
            max_tokens: 1,
            max_urls: 100,
            max_results: 100,
            ..SearchConfig::default()
        },
    )
    .expect("test client");

    brave
        .search(&SearchRequest::new("rust"))
        .await
        .expect("clamped search");

    let request = &mock.requests()[0];
    assert_eq!(
        request
            .query
            .get("maximum_number_of_tokens")
            .map(String::as_str),
        Some("1024")
    );
    assert_eq!(
        request
            .query
            .get("maximum_number_of_urls")
            .map(String::as_str),
        Some("20")
    );
    assert_eq!(request.query.get("count").map(String::as_str), Some("20"));
}

#[tokio::test]
async fn oversized_response_body_is_rejected() {
    let body = format!(
        r#"{{"grounding":{{"generic":[]}},"padding":"{}"}}"#,
        "x".repeat(4096)
    );
    let mock = Mock::new(vec![Reply::Json(200, body)]);
    let brave = BraveSearch::new(
        TEST_KEY,
        SearchConfig {
            base_url: mock.base_url(),
            timeout: Duration::from_secs(2),
            max_body_bytes: 512,
            ..SearchConfig::default()
        },
    )
    .expect("test client");

    let error = brave
        .search(&SearchRequest::new("rust"))
        .await
        .expect_err("oversized body");
    assert_eq!(error, SearchError::BodyTooLarge);
}

#[tokio::test]
async fn total_result_byte_cap_drops_trailing_sources() {
    let mut generic = Vec::new();
    let mut sources = serde_json::Map::new();
    for index in 0..4 {
        let url = format!("https://example.com/{index}");
        generic.push(serde_json::json!({
            "url": url,
            "title": format!("Result {index}"),
            "snippets": ["x".repeat(300)]
        }));
        sources.insert(
            url,
            serde_json::json!({"title": format!("Result {index}"), "age": []}),
        );
    }
    let body = serde_json::json!({
        "grounding": {"generic": generic},
        "sources": sources
    })
    .to_string();
    let mock = Mock::new(vec![Reply::Json(200, body)]);
    let brave = BraveSearch::new(
        TEST_KEY,
        SearchConfig {
            base_url: mock.base_url(),
            timeout: Duration::from_secs(2),
            total_result_bytes: 550,
            ..SearchConfig::default()
        },
    )
    .expect("test client");

    let response = brave
        .search(&SearchRequest::new("rust"))
        .await
        .expect("bounded search");
    assert!(response.results.len() < 4);
    assert!(response.truncated());
    assert!(response.render().len() <= 550);
    assert!(response.render().contains("more sources omitted"));
}

#[tokio::test]
async fn request_timeout_is_bounded() {
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
    let error = brave
        .search(&SearchRequest::new("rust"))
        .await
        .expect_err("timeout");
    assert_eq!(error, SearchError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn production_origin_is_required_outside_tests_but_loopback_is_allowed_here() {
    let foreign = SearchConfig {
        base_url: "https://example.com".to_string(),
        ..SearchConfig::default()
    };
    assert_eq!(
        BraveSearch::new(TEST_KEY, foreign).unwrap_err(),
        SearchError::InvalidConfig
    );

    let loopback = SearchConfig {
        base_url: "http://127.0.0.1:9".to_string(),
        ..SearchConfig::default()
    };
    assert!(BraveSearch::new(TEST_KEY, loopback).is_ok());
}

#[test]
fn debug_output_does_not_contain_the_key() {
    let brave = BraveSearch::new("super-secret-token", SearchConfig::default()).unwrap();
    let debug = format!("{brave:?}");
    assert!(!debug.contains("super-secret-token"));
    assert!(debug.contains("api.search.brave.com"));
}
