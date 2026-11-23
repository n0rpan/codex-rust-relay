//! Direct Brave Search client for codeox.
//!
//! This module is deliberately self-contained: it owns one pooled `reqwest`
//! client, never spawns a task, and has no dependency on the relay's provider
//! or adapter types. Subsequent wiring can hold one [`BraveSearch`] (or clones
//! of it) and await [`BraveSearch::search`].
//!
//! # Endpoints
//!
//! The primary endpoint is Brave's LLM Context API
//! (`GET /res/v1/llm/context`), which answers with grounding snippets rather
//! than a second model call. A plan without that endpoint answers HTTP 400 with
//! `{"error":{"code":"OPTION_NOT_IN_PLAN"}}`; only that exact provider code
//! switches this client to `GET /res/v1/web/search`. The decision is remembered
//! in the client's shared entitlement state, so LLM Context is not asked again
//! for that client/key. Every other error is returned without fallback.
//!
//! # Safety properties
//!
//! * The subscription token is stored in a sensitive [`HeaderValue`] and is
//!   only attached to requests for the validated base origin.
//! * Redirects are disabled in the client builder, so the token cannot be
//!   forwarded to another host.
//! * Every response body is read through [`SearchConfig::max_body_bytes`];
//!   every request is bounded by [`SearchConfig::timeout`].
//! * Errors contain neither provider response bodies nor the token.
//! * Dropping the [`BraveSearch::search`] future cancels it: the client does
//!   not detach the request or retry in the background.
//!
//! The default build uses Brave's HTTPS origin. Tests can use a loopback mock.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, HeaderValue};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;

/// Brave Search's real origin. The key is only ever sent here in production.
pub const DEFAULT_BASE_URL: &str = "https://api.search.brave.com";
/// Shown once when the first request falls back to web search.
pub const FALLBACK_NOTICE: &str = "Brave plan lacks LLM Context; using web search";

pub const DEFAULT_MAX_TOKENS: u32 = 2048;
pub const MIN_MAX_TOKENS: u32 = 1024;
pub const MAX_MAX_TOKENS: u32 = 32768;
pub const DEFAULT_MAX_URLS: u32 = 8;
pub const MAX_MAX_URLS: u32 = 20;
pub const DEFAULT_MAX_RESULTS: u32 = 8;
pub const MAX_MAX_RESULTS: u32 = 20;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_MAX_RESULT_BYTES: usize = 12 * 1024;

const LLM_PATH: &str = "/res/v1/llm/context";
const WEB_PATH: &str = "/res/v1/web/search";
const SUBSCRIPTION_HEADER: &str = "x-subscription-token";
const USER_AGENT: &str = concat!("codeox-brave-search/", env!("CARGO_PKG_VERSION"));
const MAX_QUERY_CHARS: usize = 600;
const MAX_QUERY_WORDS: usize = 75;
const MAX_TITLE_CHARS: usize = 200;
const MAX_URL_BYTES: usize = 2048;
const MAX_SNIPPET_BYTES: usize = 1000;
const MAX_SNIPPETS: usize = 5;
const MAX_RESULT_BLOCK_BYTES: usize = 2600;
const CUT: &str = "…";

/// The endpoint that answered a search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    LlmContext,
    WebSearch,
}

impl Endpoint {
    fn path(self) -> &'static str {
        match self {
            Self::LlmContext => LLM_PATH,
            Self::WebSearch => WEB_PATH,
        }
    }
}

/// Limits and the production endpoint. Clamping happens in [`BraveSearch::new`].
#[derive(Clone, Debug)]
pub struct SearchConfig {
    /// Scheme + host (+ optional port), with no path. Production:
    /// [`DEFAULT_BASE_URL`].
    pub base_url: String,
    /// Whole-request timeout (connect + headers + body).
    pub timeout: Duration,
    /// LLM Context `maximum_number_of_tokens` (clamped to 1024..=32768).
    pub max_tokens: u32,
    /// LLM Context `maximum_number_of_urls` (clamped to 1..=20).
    pub max_urls: u32,
    /// Results requested and returned (clamped to 1..=20).
    pub max_results: u32,
    /// Largest response body that will be read.
    pub max_body_bytes: usize,
    /// Aggregate byte budget for the returned result set and its rendering.
    pub total_result_bytes: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: DEFAULT_TIMEOUT,
            max_tokens: DEFAULT_MAX_TOKENS,
            max_urls: DEFAULT_MAX_URLS,
            max_results: DEFAULT_MAX_RESULTS,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            total_result_bytes: DEFAULT_MAX_RESULT_BYTES,
        }
    }
}

/// One validated search request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRequest {
    pub query: String,
    /// Overrides [`SearchConfig::max_results`] for this call.
    pub count: Option<u32>,
    /// Two-letter country code, e.g. `us`.
    pub country: Option<String>,
    /// Search language, e.g. `en` or `pt-BR`.
    pub search_lang: Option<String>,
    /// `pd`, `pw`, `pm`, `py`, or a `YYYY-MM-DDtoYYYY-MM-DD` range.
    pub freshness: Option<String>,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            count: None,
            country: None,
            search_lang: None,
            freshness: None,
        }
    }
}

/// A citation-ready result, in the provider's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    /// The provider's publication date when it gave one, as `YYYY-MM-DD`.
    pub date: Option<String>,
    pub snippets: Vec<String>,
}

/// The bounded answer to one search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResponse {
    /// Which Brave endpoint answered.
    pub endpoint: Endpoint,
    /// Provider order is preserved. The whole set is within
    /// [`SearchConfig::total_result_bytes`].
    pub results: Vec<SearchResult>,
    /// Provider results left out by the result count or the byte budget.
    pub omitted: usize,
    query: String,
    max_bytes: usize,
}

impl SearchResponse {
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    #[allow(dead_code)] // public result helpers; exercised by the direct-client tests
    pub fn truncated(&self) -> bool {
        self.omitted > 0
    }

    /// Deterministic citation text within the configured result byte budget.
    /// Nothing from the local clock is added.
    #[allow(dead_code)] // public result helper; exercised by the direct-client tests
    pub fn render(&self) -> String {
        let mut out = header_line(&self.query);
        if self.results.is_empty() {
            out.push_str("\n\n");
            out.push_str(if self.omitted == 0 {
                "No results."
            } else {
                "No results within the result size limit."
            });
        } else {
            for (index, result) in self.results.iter().enumerate() {
                out.push_str("\n\n");
                out.push_str(&render_block(index + 1, result));
            }
            if self.omitted > 0 {
                out.push_str("\n\n");
                out.push_str(&omitted_line(self.omitted));
            }
        }
        if out.len() > self.max_bytes {
            out = cut_bytes(&out, self.max_bytes);
        }
        out
    }
}

/// Stable, safe errors. No variant carries a provider body or the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchError {
    /// The configured base URL or another local setting is unusable.
    InvalidConfig,
    /// The query or one of its options is out of range.
    InvalidRequest,
    Timeout,
    Unavailable,
    /// HTTP 401 or 403.
    InvalidKey,
    /// HTTP 429.
    RateLimited,
    /// HTTP 5xx.
    ServerError,
    /// Another non-success status.
    Http(u16),
    BodyTooLarge,
    MalformedResponse,
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig => f.write_str("Brave Search is configured incorrectly"),
            Self::InvalidRequest => f.write_str("invalid Brave Search request"),
            Self::Timeout => f.write_str("Brave Search request timed out"),
            Self::Unavailable => f.write_str("could not reach Brave Search"),
            Self::InvalidKey => f.write_str("Brave Search rejected the API key"),
            Self::RateLimited => f.write_str("Brave Search rate limit exceeded"),
            Self::ServerError => f.write_str("Brave Search is unavailable"),
            Self::Http(status) => write!(f, "Brave Search returned HTTP {status}"),
            Self::BodyTooLarge => f.write_str("Brave Search response exceeded the size limit"),
            Self::MalformedResponse => f.write_str("Brave Search returned an invalid response"),
        }
    }
}

impl Error for SearchError {}

/// One reused async Brave Search client.
///
/// Clones share the pooled HTTP client and the entitlement/fallback state. A
/// new client (for example after a key change) starts with fresh state.
pub struct BraveSearch {
    client: Client,
    config: SearchConfig,
    base: Url,
    auth: HeaderValue,
    state: Arc<Entitlement>,
}

#[derive(Debug, Default)]
struct Entitlement {
    /// The plan lacks LLM Context; use web search for this client/key.
    llm_missing: AtomicBool,
    /// The one-time fallback notice is still unread.
    notice_pending: AtomicBool,
}

impl Clone for BraveSearch {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            config: self.config.clone(),
            base: self.base.clone(),
            auth: self.auth.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl fmt::Debug for BraveSearch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BraveSearch")
            .field("base_url", &self.config.base_url)
            .field("fallback", &self.fallback_active())
            .finish_non_exhaustive()
    }
}

impl BraveSearch {
    /// Builds the one pooled client. A non-production base URL is accepted only
    /// in tests, and only when it is loopback.
    pub fn new(key: &str, mut config: SearchConfig) -> Result<Self, SearchError> {
        #[cfg(feature = "test-brave")]
        if key == "brave-fixture-key"
            && let Ok(base) = std::env::var("CODEOX_TEST_BRAVE_BASE_URL")
        {
            config.base_url = base;
        }
        config.max_tokens = config.max_tokens.clamp(MIN_MAX_TOKENS, MAX_MAX_TOKENS);
        config.max_urls = config.max_urls.clamp(1, MAX_MAX_URLS);
        config.max_results = config.max_results.clamp(1, MAX_MAX_RESULTS);
        if config.timeout.is_zero() {
            config.timeout = DEFAULT_TIMEOUT;
        }
        config.timeout = config.timeout.min(MAX_TIMEOUT);
        config.max_body_bytes = config.max_body_bytes.max(1);
        let base = validate_base(&config.base_url)?;
        let auth = subscription_header(key)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.timeout)
            .timeout(config.timeout)
            .user_agent(USER_AGENT);
        #[cfg(any(test, feature = "test-brave"))]
        let client = if is_loopback(&base) {
            client.no_proxy()
        } else {
            client
        };
        let client = client.build().map_err(|_| SearchError::InvalidConfig)?;
        Ok(Self {
            client,
            config,
            base,
            auth,
            state: Arc::new(Entitlement::default()),
        })
    }

    /// Whether this client/key already switched from LLM Context to web search.
    pub fn fallback_active(&self) -> bool {
        self.state.llm_missing.load(Ordering::SeqCst)
    }

    /// Takes the one-time notice produced by the first fallback.
    pub fn take_fallback_notice(&self) -> Option<&'static str> {
        self.state
            .notice_pending
            .swap(false, Ordering::SeqCst)
            .then_some(FALLBACK_NOTICE)
    }

    /// Runs one search. Dropping the returned future cancels the request.
    pub async fn search(&self, request: &SearchRequest) -> Result<SearchResponse, SearchError> {
        let query = self.prepare(request)?;

        if !self.fallback_active() {
            match self.get(Endpoint::LlmContext, &query).await? {
                Reply::Success(body) => {
                    return self.parse_response(Endpoint::LlmContext, &query, &body);
                }
                Reply::Rejected(400, body) if is_option_not_in_plan(&body) => {
                    if !self.state.llm_missing.swap(true, Ordering::SeqCst) {
                        self.state.notice_pending.store(true, Ordering::SeqCst);
                    }
                }
                Reply::Rejected(status, _) => return Err(status_error(status)),
            }
        }

        match self.get(Endpoint::WebSearch, &query).await? {
            Reply::Success(body) => self.parse_response(Endpoint::WebSearch, &query, &body),
            Reply::Rejected(status, _) => Err(status_error(status)),
        }
    }

    fn prepare(&self, request: &SearchRequest) -> Result<PreparedQuery, SearchError> {
        let text = normalise(&request.query);
        let chars = text.chars().count();
        if chars == 0
            || chars > MAX_QUERY_CHARS
            || text.split_whitespace().count() > MAX_QUERY_WORDS
        {
            return Err(SearchError::InvalidRequest);
        }
        let count = match request.count {
            Some(count) if !(1..=MAX_MAX_RESULTS).contains(&count) => {
                return Err(SearchError::InvalidRequest);
            }
            Some(count) => count,
            None => self.config.max_results,
        };
        let country = match request.country.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(value)
                if value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_alphabetic()) =>
            {
                Some(value.to_ascii_lowercase())
            }
            Some(_) => return Err(SearchError::InvalidRequest),
        };
        let search_lang = match request.search_lang.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(value) => Some(validated_lang(value)?),
        };
        let freshness = match request.freshness.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(value) => Some(validated_freshness(value)?),
        };
        Ok(PreparedQuery {
            text,
            count,
            country,
            search_lang,
            freshness,
        })
    }

    async fn get(&self, endpoint: Endpoint, query: &PreparedQuery) -> Result<Reply, SearchError> {
        let mut url = self.base.clone();
        url.set_path(endpoint.path());
        url.set_query(None);

        let mut params: Vec<(&str, String)> = vec![("q", query.text.clone())];
        match endpoint {
            Endpoint::LlmContext => {
                params.push(("count", query.count.to_string()));
                params.push((
                    "maximum_number_of_tokens",
                    self.config.max_tokens.to_string(),
                ));
                params.push((
                    "maximum_number_of_urls",
                    query.count.min(self.config.max_urls).to_string(),
                ));
            }
            Endpoint::WebSearch => {
                params.push(("count", query.count.to_string()));
                params.push(("extra_snippets", "true".to_string()));
            }
        }
        if let Some(country) = &query.country {
            params.push(("country", country.clone()));
        }
        if let Some(search_lang) = &query.search_lang {
            params.push(("search_lang", search_lang.clone()));
        }
        if let Some(freshness) = &query.freshness {
            params.push(("freshness", freshness.clone()));
        }

        let response = self
            .client
            .get(url)
            .header(ACCEPT, "application/json")
            .header(SUBSCRIPTION_HEADER, self.auth.clone())
            .query(&params)
            .send()
            .await
            .map_err(request_error)?;

        let status = response.status();
        if status == StatusCode::OK {
            return Ok(Reply::Success(
                read_body(response, self.config.max_body_bytes).await?,
            ));
        }
        if status == StatusCode::BAD_REQUEST {
            // Only a 400 can prove OPTION_NOT_IN_PLAN. The body is bounded and is
            // never exposed in an error.
            return Ok(Reply::Rejected(
                400,
                read_body(response, self.config.max_body_bytes).await?,
            ));
        }
        // Other error bodies are not read at all.
        Ok(Reply::Rejected(status.as_u16(), Vec::new()))
    }

    fn parse_response(
        &self,
        endpoint: Endpoint,
        query: &PreparedQuery,
        body: &[u8],
    ) -> Result<SearchResponse, SearchError> {
        let found = match endpoint {
            Endpoint::LlmContext => parse_llm(body)?,
            Endpoint::WebSearch => parse_web(body)?,
        };
        let provider_count = found.len();
        let mut omitted = provider_count.saturating_sub(query.count as usize);
        let found: Vec<SearchResult> = found.into_iter().take(query.count as usize).collect();
        let (results, cap_omitted) =
            bound_results(found, &query.text, omitted, self.config.total_result_bytes);
        omitted += cap_omitted;
        Ok(SearchResponse {
            endpoint,
            results,
            omitted,
            query: query.text.clone(),
            max_bytes: self.config.total_result_bytes,
        })
    }
}

#[derive(Debug)]
struct PreparedQuery {
    text: String,
    count: u32,
    country: Option<String>,
    search_lang: Option<String>,
    freshness: Option<String>,
}

enum Reply {
    Success(Vec<u8>),
    Rejected(u16, Vec<u8>),
}

fn subscription_header(key: &str) -> Result<HeaderValue, SearchError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(SearchError::InvalidConfig);
    }
    let mut value = HeaderValue::from_str(key).map_err(|_| SearchError::InvalidConfig)?;
    value.set_sensitive(true);
    Ok(value)
}

fn validate_base(raw: &str) -> Result<Url, SearchError> {
    let url = Url::parse(raw).map_err(|_| SearchError::InvalidConfig)?;
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(SearchError::InvalidConfig);
    }
    let brave = url.scheme() == "https"
        && url.host_str() == Some("api.search.brave.com")
        && url.port().is_none();
    if brave {
        return Ok(url);
    }
    #[cfg(any(test, feature = "test-brave"))]
    if is_loopback(&url) {
        return Ok(url);
    }
    Err(SearchError::InvalidConfig)
}

#[cfg(any(test, feature = "test-brave"))]
fn is_loopback(url: &Url) -> bool {
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    loopback && matches!(url.scheme(), "http" | "https")
}

fn request_error(error: reqwest::Error) -> SearchError {
    if error.is_timeout() {
        SearchError::Timeout
    } else {
        SearchError::Unavailable
    }
}

fn status_error(status: u16) -> SearchError {
    match status {
        401 | 403 => SearchError::InvalidKey,
        429 => SearchError::RateLimited,
        500..=599 => SearchError::ServerError,
        other => SearchError::Http(other),
    }
}

async fn read_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, SearchError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(SearchError::BodyTooLarge);
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(request_error)?;
        if body.len() + chunk.len() > limit {
            return Err(SearchError::BodyTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validated_lang(value: &str) -> Result<String, SearchError> {
    let mut parts = value.split('-');
    let head = parts.next().unwrap_or("");
    let head_ok =
        (2..=3).contains(&head.len()) && head.bytes().all(|byte| byte.is_ascii_alphabetic());
    let rest_ok = parts.all(|part| {
        (2..=4).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
    });
    if head_ok && rest_ok {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(SearchError::InvalidRequest)
    }
}

fn validated_freshness(value: &str) -> Result<String, SearchError> {
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "pd" | "pw" | "pm" | "py" => Ok(lower),
        _ if freshness_range(&lower) => Ok(lower),
        _ => Err(SearchError::InvalidRequest),
    }
}

fn freshness_range(value: &str) -> bool {
    value
        .split_once("to")
        .is_some_and(|(start, end)| date_shape(start) && date_shape(end))
}

fn date_shape(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

fn normalise(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cut_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    if max < CUT.len() {
        return String::new();
    }
    let mut end = max.saturating_sub(CUT.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{CUT}", value[..end].trim_end())
}

fn cut_chars(value: &str, max: usize) -> String {
    match value.char_indices().nth(max) {
        Some((end, _)) => format!("{}{CUT}", value[..end].trim_end()),
        None => value.to_string(),
    }
}

fn valid_result_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() || url.len() > MAX_URL_BYTES {
        return None;
    }
    let parsed = Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(url.to_string())
}

fn make_result(
    title: String,
    url: String,
    date: Option<String>,
    snippets: Vec<String>,
) -> SearchResult {
    let title = cut_chars(&normalise(&title), MAX_TITLE_CHARS);
    let date = date.and_then(|date| date_prefix(&date));
    let mut kept = Vec::new();
    let mut used = title.len() + url.len() + date.as_deref().map_or(0, str::len);
    for snippet in snippets {
        let snippet = cut_bytes(&normalise(&snippet), MAX_SNIPPET_BYTES);
        if snippet.is_empty() || kept.iter().any(|existing: &String| existing == &snippet) {
            continue;
        }
        if kept.len() >= MAX_SNIPPETS || used + snippet.len() > MAX_RESULT_BLOCK_BYTES {
            break;
        }
        used += snippet.len();
        kept.push(snippet);
    }
    SearchResult {
        title,
        url,
        date,
        snippets: kept,
    }
}

/// `YYYY-MM-DD` at the start of a provider date, with plausible month/day.
fn date_prefix(value: &str) -> Option<String> {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 10 || !value.is_char_boundary(10) || !date_shape(&value[..10]) {
        return None;
    }
    let month = (bytes[5] - b'0') * 10 + (bytes[6] - b'0');
    let day = (bytes[8] - b'0') * 10 + (bytes[9] - b'0');
    if (1..=12).contains(&month) && (1..=31).contains(&day) {
        Some(value[..10].to_string())
    } else {
        None
    }
}

fn date_from_ages(ages: &[String]) -> Option<String> {
    ages.iter().find_map(|age| date_prefix(age))
}

fn strip_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(character),
            _ => {}
        }
    }
    decode_entities(&out)
}

fn decode_entities(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        let end = tail.find(';').filter(|&end| end <= 10);
        match end.and_then(|end| entity(&tail[1..end]).map(|character| (end, character))) {
            Some((end, character)) => {
                out.push(character);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse::<u32>().ok()?,
            };
            return char::from_u32(code);
        }
    })
}

#[derive(Default, Deserialize)]
struct LlmEnvelope {
    #[serde(default)]
    grounding: LlmGrounding,
    #[serde(default)]
    sources: HashMap<String, LlmSource>,
}

#[derive(Default, Deserialize)]
struct LlmGrounding {
    #[serde(default)]
    generic: Vec<LlmGeneric>,
}

#[derive(Default, Deserialize)]
struct LlmGeneric {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    snippets: Vec<String>,
}

#[derive(Default, Deserialize)]
struct LlmSource {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    age: Vec<String>,
}

fn parse_llm(body: &[u8]) -> Result<Vec<SearchResult>, SearchError> {
    let envelope: LlmEnvelope =
        serde_json::from_slice(body).map_err(|_| SearchError::MalformedResponse)?;
    let mut found = Vec::new();
    for item in envelope.grounding.generic {
        let Some(url) = item.url.as_deref().and_then(valid_result_url) else {
            continue;
        };
        let source = envelope.sources.get(&url);
        let mut title = item.title.unwrap_or_default();
        if title.trim().is_empty() {
            title = source
                .and_then(|source| source.title.clone())
                .unwrap_or_default();
        }
        let date = source.and_then(|source| date_from_ages(&source.age));
        found.push(make_result(title, url, date, item.snippets));
    }
    Ok(found)
}

#[derive(Default, Deserialize)]
struct WebEnvelope {
    #[serde(default)]
    web: WebSection,
}

#[derive(Default, Deserialize)]
struct WebSection {
    #[serde(default)]
    results: Vec<WebResult>,
}

#[derive(Default, Deserialize)]
struct WebResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    extra_snippets: Option<Vec<String>>,
    #[serde(default)]
    page_age: Option<String>,
}

fn parse_web(body: &[u8]) -> Result<Vec<SearchResult>, SearchError> {
    let envelope: WebEnvelope =
        serde_json::from_slice(body).map_err(|_| SearchError::MalformedResponse)?;
    let mut found = Vec::new();
    for item in envelope.web.results {
        let Some(url) = item.url.as_deref().and_then(valid_result_url) else {
            continue;
        };
        let mut snippets = Vec::new();
        if let Some(description) = item.description {
            snippets.push(strip_html(&description));
        }
        if let Some(extra) = item.extra_snippets {
            snippets.extend(extra.iter().map(|snippet| strip_html(snippet)));
        }
        let date = item.page_age.and_then(|age| date_prefix(&age));
        found.push(make_result(
            strip_html(item.title.as_deref().unwrap_or("")),
            url,
            date,
            snippets,
        ));
    }
    Ok(found)
}

fn is_option_not_in_plan(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        error: Option<ApiError>,
    }
    #[derive(Deserialize)]
    struct ApiError {
        #[serde(default)]
        code: Option<String>,
    }
    serde_json::from_slice::<Envelope>(body)
        .ok()
        .and_then(|envelope| envelope.error)
        .and_then(|error| error.code)
        .is_some_and(|code| code == "OPTION_NOT_IN_PLAN")
}

fn header_line(query: &str) -> String {
    format!("Web search results for: {}", normalise(query))
}

fn omitted_line(omitted: usize) -> String {
    format!("({omitted} more sources omitted)")
}

fn render_block(index: usize, result: &SearchResult) -> String {
    let title = if result.title.is_empty() {
        "Untitled".to_string()
    } else {
        result.title.clone()
    };
    let mut out = format!("[{index}] {title}\n{}", result.url);
    if let Some(date) = &result.date {
        out.push_str("\nPublished: ");
        out.push_str(date);
    }
    for snippet in &result.snippets {
        out.push('\n');
        out.push_str(snippet);
    }
    out
}

/// Keeps provider order and fits the result set (and its eventual rendering)
/// into `max_bytes`, reserving room for the header and omitted line.
fn bound_results(
    found: Vec<SearchResult>,
    query: &str,
    mut omitted: usize,
    max_bytes: usize,
) -> (Vec<SearchResult>, usize) {
    let mut used = header_line(query).len();
    let mut results = Vec::new();
    let total = found.len();
    for (index, result) in found.into_iter().enumerate() {
        let block = render_block(results.len() + 1, &result);
        let reserve = omitted_line(omitted + (total - index)).len();
        if used + 2 + block.len() + reserve > max_bytes {
            omitted += total - index;
            break;
        }
        used += 2 + block.len();
        results.push(result);
    }
    (results, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_normalised_and_relative_dates_are_ignored() {
        let ages = vec![
            "Monday, January 15, 2024".to_string(),
            "2024-01-15".to_string(),
            "380 days ago".to_string(),
        ];
        assert_eq!(date_from_ages(&ages).as_deref(), Some("2024-01-15"));
        assert_eq!(
            date_prefix("2024-02-03T10:00:00Z").as_deref(),
            Some("2024-02-03")
        );
        assert_eq!(date_prefix("2024-13-03"), None);
        assert_eq!(date_prefix("yesterday"), None);
    }

    #[test]
    fn html_entities_are_decoded() {
        assert_eq!(
            strip_html("a <strong>b</strong> &amp; c&#39;s &#x41;"),
            "a b & c's A"
        );
    }

    #[test]
    fn request_validation_rejects_bad_values() {
        let search = BraveSearch::new("key", SearchConfig::default()).unwrap();
        assert_eq!(
            search.prepare(&SearchRequest::new("   ")).unwrap_err(),
            SearchError::InvalidRequest
        );
        assert_eq!(
            search
                .prepare(&SearchRequest {
                    query: "rust".into(),
                    count: Some(21),
                    ..SearchRequest::new("rust")
                })
                .unwrap_err(),
            SearchError::InvalidRequest
        );
        assert_eq!(
            search
                .prepare(&SearchRequest {
                    query: "rust".into(),
                    country: Some("usa".into()),
                    ..SearchRequest::new("rust")
                })
                .unwrap_err(),
            SearchError::InvalidRequest
        );
    }
}
