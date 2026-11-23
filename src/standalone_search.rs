//! Standalone web search request/response bridge for Codex.
//!
//! Codex 0.171.0 (`codex-rs/codex-api/src/endpoint/search.rs`) posts
//! `alpha/search` relative to the provider base URL, so a `/v1` base becomes
//! [`ENDPOINT_PATH`] (`POST /v1/alpha/search`). It decodes exactly
//! `codex_api::SearchResponse`:
//!
//! ```json
//! {"encrypted_output": null, "output": "…", "results": [{"type": "text_result", …}]}
//! ```
//!
//! This module is the pure endpoint handler: it decodes one request body,
//! validates Codex's `commands`/`settings`, performs bounded Brave queries
//! through [`crate::search::BraveSearch`], and renders a Codex-shaped reply. It
//! does no routing, spawning, logging, or credential loading, so the relay can
//! mount it on an already-authenticated loopback route.
//!
//! # Supported surface
//!
//! * Only `commands.search_query` is executed. `literature_search_query`,
//!   `image_query`, `open`, `click`, `find`, `screenshot`, `finance`, `weather`,
//!   `sports`, `time`, and unknown operations return an explicit tool error
//!   instead of being ignored.
//! * `search_query[].recency` is forwarded only for the exact Brave freshness
//!   buckets (1, 7, 30, 365 days). Any other value fails closed: Brave has no
//!   portable "N days" parameter, and silently widening or narrowing the window
//!   would change the caller's constraint.
//! * `search_query[].domains` and `settings.filters.allowed_domains` become
//!   `site:` clauses on the Brave query, one AND group per constraint list.
//!   Returned URLs must also match each constraint list. A
//!   `blocked_domains` filter, or a domain list that cannot be represented as
//!   bare DNS names, fails closed.
//! * `settings.external_web_access` must be `true` or `"live"`. `false`,
//!   `"cached"`, and `"indexed"` are rejected, so live/disabled policy is never
//!   weakened by this bridge.
//! * `input` (Codex's recent-conversation tail) is never read, so nothing but
//!   explicit queries and settings reach Brave.
//!
//! `encrypted_output` is always `null`: this bridge has no encrypted result
//! channel. Text results are returned out-of-band in `results` with stable,
//! unique `turn0searchN` reference ids and in `output` as citation blocks.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use serde_json::{Value, json};

use crate::search::{BraveSearch, SearchError, SearchRequest, SearchResult};

/// Codex's absolute path for a `/v1` provider base. Production relay route.
pub const ENDPOINT_PATH: &str = "/v1/alpha/search";
/// The provider-relative endpoint Codex appends to its base URL.
#[allow(dead_code)] // documented contract; the relay matches the absolute path
pub const PROVIDER_RELATIVE_PATH: &str = "alpha/search";
/// Bound the relay should use while buffering a search request body.
///
/// The body may contain Codex's recent-conversation tail, which this handler
/// deliberately ignores; 1 MiB is far above any real search request.
#[allow(dead_code)] // consumed by the relay route once `main.rs` wires it
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// Maximum `commands.search_query` entries in one request.
pub const MAX_QUERIES: usize = 4;
/// Maximum domains in one `site:` AND group.
pub const MAX_DOMAINS_PER_FILTER: usize = 8;
/// Per-query rendered output budget, matching the direct client's default
/// aggregate result budget.
pub const MAX_QUERY_OUTPUT_BYTES: usize = crate::search::DEFAULT_MAX_RESULT_BYTES;
/// Aggregate rendered output budget for one request.
pub const MAX_OUTPUT_BYTES: usize = MAX_QUERY_OUTPUT_BYTES * MAX_QUERIES;
/// Stable reference-id prefix used by Codex's standalone search results.
pub const REF_ID_PREFIX: &str = "turn0search";

/// One HTTP reply for the relay to send unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointReply {
    /// HTTP status code.
    pub status: u16,
    /// JSON body.
    pub body: Vec<u8>,
}

impl EndpointReply {
    /// True for a Codex-successful search reply.
    #[allow(dead_code)] // public reply helpers; exercised by the endpoint tests
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Decodes the reply for tests or structured logging.
    #[allow(dead_code)] // public reply helper; exercised by the endpoint tests
    pub fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    fn encode<T: Serialize>(status: u16, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self { status, body },
            Err(_) => Self {
                status: 500,
                body: br#"{"error":{"code":"internal_error","message":"search response could not be encoded"}}"#
                    .to_vec(),
            },
        }
    }
}

/// One citation-ready text result, in the exact shape Codex's UI consumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextResult {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub ref_id: String,
    pub url: String,
    pub title: String,
    pub snippet: String,
}

/// The endpoint handler. Holds only a borrowed client; the relay owns it.
pub struct SearchEndpoint<'a> {
    search: &'a BraveSearch,
}

impl<'a> SearchEndpoint<'a> {
    pub fn new(search: &'a BraveSearch) -> Self {
        Self { search }
    }

    /// Handles one decoded-or-not POST body. Always returns a bounded reply.
    pub async fn handle(&self, body: &[u8]) -> EndpointReply {
        match self.try_handle(body).await {
            Ok(success) => EndpointReply::encode(200, &success),
            Err(error) => error.reply(),
        }
    }

    async fn try_handle(&self, body: &[u8]) -> Result<SuccessBody, BridgeError> {
        let request: RequestEnvelope =
            serde_json::from_slice(body).map_err(|_| BridgeError::InvalidJson)?;
        let policy = parse_settings(request.settings.as_ref())?;
        let mut queries = parse_commands(request.commands.as_ref())?;
        for query in &mut queries {
            query.apply_policy(&policy);
        }

        let mut output = String::new();
        let mut results = Vec::new();
        let mut next_ref = 0_usize;
        for query in &queries {
            let mut brave = SearchRequest::new(query.brave.clone());
            brave.freshness = query.freshness.map(str::to_string);
            brave.country = policy.country.clone();
            let mut response = self
                .search
                .search(&brave)
                .await
                .map_err(BridgeError::Upstream)?;
            if !query.domains.is_empty() || !policy.allowed_domains.is_empty() {
                response.results.retain(|result| {
                    reqwest::Url::parse(&result.url)
                        .ok()
                        .and_then(|url| {
                            url.host_str().map(|host| {
                                matches_domains(host, &query.domains)
                                    && matches_domains(host, &policy.allowed_domains)
                            })
                        })
                        .unwrap_or(false)
                });
            }

            if !output.is_empty() {
                output.push_str("\n\n");
            }
            render_section(&mut output, &mut results, &mut next_ref, query, &response);
        }
        let output = cut_bytes(&output, MAX_OUTPUT_BYTES);

        Ok(SuccessBody {
            output,
            encrypted_output: None,
            results,
        })
    }
}

#[derive(Serialize)]
struct SuccessBody {
    output: String,
    encrypted_output: Option<String>,
    results: Vec<TextResult>,
}

// Top-level fields other than commands/settings (`id`, `model`, `reasoning`,
// `input`, `max_output_tokens`) are intentionally not declared: they are
// ignored, and `input` must never be forwarded.
#[derive(Deserialize)]
struct RequestEnvelope {
    #[serde(default)]
    commands: Option<Value>,
    #[serde(default)]
    settings: Option<Value>,
}

#[derive(Debug, PartialEq, Eq)]
enum BridgeError {
    InvalidJson,
    InvalidRequest(&'static str),
    UnsupportedOperation(String),
    UnsupportedSetting(String),
    UnsupportedRecency(u64),
    TooManyQueries(usize),
    TooManyDomains(usize),
    UnsupportedPolicy(&'static str),
    Upstream(SearchError),
}

impl BridgeError {
    fn invalid(message: &'static str) -> Self {
        Self::InvalidRequest(message)
    }

    fn status(&self) -> u16 {
        match self {
            Self::InvalidJson
            | Self::InvalidRequest(_)
            | Self::UnsupportedOperation(_)
            | Self::UnsupportedSetting(_)
            | Self::UnsupportedRecency(_)
            | Self::TooManyQueries(_)
            | Self::TooManyDomains(_) => 400,
            Self::UnsupportedPolicy(_) => 403,
            Self::Upstream(error) => upstream_status(*error),
        }
    }

    fn error_type(&self) -> &'static str {
        match self.status() {
            400 => "invalid_request_error",
            403 => "permission_error",
            429 => "rate_limit_error",
            _ => "api_error",
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::InvalidRequest(_) => "invalid_request",
            Self::UnsupportedOperation(_) => "unsupported_operation",
            Self::UnsupportedSetting(_) => "unsupported_setting",
            Self::UnsupportedRecency(_) => "unsupported_recency",
            Self::TooManyQueries(_) => "too_many_queries",
            Self::TooManyDomains(_) => "too_many_domains",
            Self::UnsupportedPolicy(_) => "unsupported_policy",
            Self::Upstream(error) => match error {
                SearchError::InvalidConfig => "upstream_config",
                SearchError::InvalidRequest => "upstream_invalid_query",
                SearchError::Timeout => "upstream_timeout",
                SearchError::Unavailable => "upstream_unavailable",
                SearchError::InvalidKey => "upstream_auth",
                SearchError::RateLimited => "upstream_rate_limited",
                SearchError::ServerError | SearchError::Http(_) => "upstream_error",
                SearchError::BodyTooLarge | SearchError::MalformedResponse => {
                    "upstream_bad_response"
                }
            },
        }
    }

    fn message(&self) -> String {
        match self {
            Self::InvalidJson => "search request body is not valid JSON".to_string(),
            Self::InvalidRequest(message) => (*message).to_string(),
            Self::UnsupportedOperation(operation) => {
                format!("web search does not support the {operation} operation")
            }
            Self::UnsupportedSetting(setting) => {
                format!("web search does not support the {setting} setting")
            }
            Self::UnsupportedRecency(days) => {
                format!("recency of {days} days cannot be represented; use 1, 7, 30 or 365 days")
            }
            Self::TooManyQueries(count) => {
                format!("search_query has {count} entries; at most {MAX_QUERIES} are allowed")
            }
            Self::TooManyDomains(count) => format!(
                "domain filter has {count} entries; at most {MAX_DOMAINS_PER_FILTER} are allowed"
            ),
            Self::UnsupportedPolicy(message) => (*message).to_string(),
            Self::Upstream(error) => error.to_string(),
        }
    }

    fn reply(&self) -> EndpointReply {
        let body = json!({
            "error": {
                "message": self.message(),
                "type": self.error_type(),
                "code": self.code(),
            }
        });
        EndpointReply::encode(self.status(), &body)
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

fn upstream_status(error: SearchError) -> u16 {
    match error {
        SearchError::InvalidRequest => 400,
        SearchError::RateLimited => 429,
        SearchError::Timeout => 504,
        SearchError::InvalidConfig
        | SearchError::Unavailable
        | SearchError::InvalidKey
        | SearchError::ServerError
        | SearchError::Http(_)
        | SearchError::BodyTooLarge
        | SearchError::MalformedResponse => 502,
    }
}

#[derive(Debug)]
struct PlannedQuery {
    /// The caller's normalised query, used in `output` and citations.
    display: String,
    /// The query actually sent to Brave, with `site:` constraints appended.
    brave: String,
    freshness: Option<&'static str>,
    domains: Vec<String>,
}

impl PlannedQuery {
    fn apply_policy(&mut self, policy: &Policy) {
        let global = policy.allowed_domains.as_slice();
        let local = self.domains.as_slice();
        let groups: Vec<&[String]> = if local.is_empty() || local == global {
            vec![global]
        } else if global.is_empty() {
            vec![local]
        } else {
            vec![global, local]
        };
        self.brave = apply_site_filters(&self.display, &groups);
    }
}

fn parse_commands(value: Option<&Value>) -> Result<Vec<PlannedQuery>, BridgeError> {
    let Some(value) = value else {
        return Err(BridgeError::invalid("commands.search_query is required"));
    };
    let Value::Object(commands) = value else {
        return Err(BridgeError::invalid("commands must be an object"));
    };
    let mut queries = None;
    for (key, value) in commands {
        match key.as_str() {
            "search_query" => queries = Some(parse_queries(value)?),
            // A rendering hint, not a restriction; our output is bounded by
            // MAX_QUERY_OUTPUT_BYTES either way.
            "response_length" => validate_response_length(value)?,
            operation => return Err(BridgeError::UnsupportedOperation(operation.to_string())),
        }
    }
    queries.ok_or_else(|| BridgeError::invalid("commands.search_query is required"))
}

fn validate_response_length(value: &Value) -> Result<(), BridgeError> {
    match value {
        Value::Null => Ok(()),
        Value::String(length) if matches!(length.as_str(), "short" | "medium" | "long") => Ok(()),
        _ => Err(BridgeError::invalid(
            "response_length must be short, medium or long",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuery {
    q: String,
    #[serde(default)]
    recency: Option<u64>,
    #[serde(default)]
    domains: Option<Vec<String>>,
}

fn parse_queries(value: &Value) -> Result<Vec<PlannedQuery>, BridgeError> {
    let Value::Array(items) = value else {
        return Err(BridgeError::invalid("search_query must be an array"));
    };
    if items.is_empty() {
        return Err(BridgeError::invalid("search_query must not be empty"));
    }
    if items.len() > MAX_QUERIES {
        return Err(BridgeError::TooManyQueries(items.len()));
    }
    let mut queries = Vec::with_capacity(items.len());
    for item in items {
        let raw: RawQuery = serde_json::from_value(item.clone())
            .map_err(|_| BridgeError::invalid("search_query entries have an unsupported shape"))?;
        let display = normalise(&raw.q);
        if display.is_empty() {
            return Err(BridgeError::invalid(
                "search_query entries need a non-empty q",
            ));
        }
        let freshness = match raw.recency {
            None => None,
            Some(days) => Some(freshness_for_days(days)?),
        };
        let domains = match raw.domains {
            None => Vec::new(),
            Some(domains) if domains.is_empty() => {
                return Err(BridgeError::invalid(
                    "search_query domains must not be empty",
                ));
            }
            Some(domains) => parse_domain_list(domains)?,
        };
        queries.push(PlannedQuery {
            brave: display.clone(),
            display,
            freshness,
            domains,
        });
    }
    Ok(queries)
}

/// Brave exposes only past-day/week/month/year shortcuts plus date ranges.
/// Only exact buckets are forwarded; everything else fails closed.
fn freshness_for_days(days: u64) -> Result<&'static str, BridgeError> {
    match days {
        1 => Ok("pd"),
        7 => Ok("pw"),
        30 => Ok("pm"),
        365 => Ok("py"),
        other => Err(BridgeError::UnsupportedRecency(other)),
    }
}

#[derive(Debug, Default)]
struct Policy {
    country: Option<String>,
    allowed_domains: Vec<String>,
}

fn parse_settings(value: Option<&Value>) -> Result<Policy, BridgeError> {
    let Some(value) = value else {
        return Err(BridgeError::invalid(
            "settings.external_web_access is required",
        ));
    };
    let Value::Object(settings) = value else {
        return Err(BridgeError::invalid("settings must be an object"));
    };
    let mut policy = Policy::default();
    let mut access_seen = false;
    for (key, value) in settings {
        match key.as_str() {
            "external_web_access" => {
                validate_web_access(value)?;
                access_seen = true;
            }
            "user_location" => policy.country = parse_user_location(value)?,
            "search_context_size" => validate_context_size(value)?,
            "filters" => policy.allowed_domains = parse_filters(value)?,
            "image_settings" => validate_image_settings(value)?,
            "allowed_callers" => validate_callers(value)?,
            setting => return Err(BridgeError::UnsupportedSetting(setting.to_string())),
        }
    }
    if !access_seen {
        return Err(BridgeError::invalid(
            "settings.external_web_access is required",
        ));
    }
    Ok(policy)
}

/// Codex maps its `web_search` mode onto `external_web_access`:
/// disabled/cached -> `false`, indexed -> `"indexed"`, live -> `true`.
/// This bridge performs live Brave queries only, so everything but live fails.
fn validate_web_access(value: &Value) -> Result<(), BridgeError> {
    match value {
        Value::Bool(true) => Ok(()),
        Value::Bool(false) => Err(BridgeError::UnsupportedPolicy(
            "web search is disabled for this request",
        )),
        Value::String(mode) => match mode.as_str() {
            "live" => Ok(()),
            "cached" => Err(BridgeError::UnsupportedPolicy(
                "cached web search is not supported; only live search is available",
            )),
            "indexed" => Err(BridgeError::UnsupportedPolicy(
                "indexed web search is not supported; only live search is available",
            )),
            _ => Err(BridgeError::invalid(
                "settings.external_web_access must be a boolean or a known access mode",
            )),
        },
        _ => Err(BridgeError::invalid(
            "settings.external_web_access must be a boolean or a known access mode",
        )),
    }
}

fn parse_user_location(value: &Value) -> Result<Option<String>, BridgeError> {
    if value.is_null() {
        return Ok(None);
    }
    let Value::Object(location) = value else {
        return Err(BridgeError::invalid(
            "settings.user_location must be an object",
        ));
    };
    let mut country = None;
    let mut kind_seen = false;
    for (key, value) in location {
        match key.as_str() {
            "type" => {
                if value.as_str() != Some("approximate") {
                    return Err(BridgeError::invalid(
                        "settings.user_location.type must be approximate",
                    ));
                }
                kind_seen = true;
            }
            "country" => country = parse_country(value)?,
            // Brave accepts only a country. Region, city and timezone are
            // finer-grained preferences that cannot be represented, so they are
            // ignored rather than silently used as something else.
            "region" | "city" | "timezone" => {
                if !value.is_null() && value.as_str().is_none() {
                    return Err(BridgeError::invalid(
                        "settings.user_location text fields must be strings",
                    ));
                }
            }
            field => {
                return Err(BridgeError::UnsupportedSetting(format!(
                    "user_location.{field}"
                )));
            }
        }
    }
    if !kind_seen {
        return Err(BridgeError::invalid(
            "settings.user_location.type must be approximate",
        ));
    }
    Ok(country)
}

fn parse_country(value: &Value) -> Result<Option<String>, BridgeError> {
    let Some(country) = value.as_str() else {
        return Err(BridgeError::invalid(
            "settings.user_location.country must be a string",
        ));
    };
    let country = country.trim();
    if country.is_empty() {
        return Ok(None);
    }
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(BridgeError::invalid(
            "settings.user_location.country must be a two-letter code",
        ));
    }
    Ok(Some(country.to_ascii_lowercase()))
}

fn validate_context_size(value: &Value) -> Result<(), BridgeError> {
    match value {
        Value::Null => Ok(()),
        // A generation-size hint for the hosted backend. We render provider
        // snippets ourselves under a fixed byte bound.
        Value::String(size) if matches!(size.as_str(), "low" | "medium" | "high") => Ok(()),
        _ => Err(BridgeError::invalid(
            "settings.search_context_size must be low, medium or high",
        )),
    }
}

fn parse_filters(value: &Value) -> Result<Vec<String>, BridgeError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Value::Object(filters) = value else {
        return Err(BridgeError::invalid("settings.filters must be an object"));
    };
    let mut allowed = Vec::new();
    for (key, value) in filters {
        match key.as_str() {
            "allowed_domains" => {
                allowed = parse_domains_value(value)?;
            }
            "blocked_domains" => {
                let blocked = parse_domains_value(value)?;
                if !blocked.is_empty() {
                    return Err(BridgeError::UnsupportedPolicy(
                        "blocked domains cannot be honored by the live search provider",
                    ));
                }
            }
            filter => {
                return Err(BridgeError::UnsupportedSetting(format!("filters.{filter}")));
            }
        }
    }
    Ok(allowed)
}

fn validate_image_settings(value: &Value) -> Result<(), BridgeError> {
    if value.is_null() {
        return Ok(());
    }
    let Value::Object(image) = value else {
        return Err(BridgeError::invalid(
            "settings.image_settings must be an object",
        ));
    };
    for (key, value) in image {
        match key.as_str() {
            "max_results" if !value.is_null() && value.as_u64().is_none() => {
                return Err(BridgeError::invalid(
                    "settings.image_settings.max_results must be an integer",
                ));
            }
            "caption" if !value.is_null() && value.as_bool().is_none() => {
                return Err(BridgeError::invalid(
                    "settings.image_settings.caption must be a boolean",
                ));
            }
            // Image queries are rejected above, so these settings never apply.
            _ => {}
        }
    }
    Ok(())
}

fn validate_callers(value: &Value) -> Result<(), BridgeError> {
    if value.is_null() {
        return Ok(());
    }
    let Value::Array(callers) = value else {
        return Err(BridgeError::invalid(
            "settings.allowed_callers must be an array",
        ));
    };
    let mut direct = false;
    for caller in callers {
        match caller.as_str() {
            Some("direct") => direct = true,
            Some("shell" | "code_interpreter") => {}
            _ => {
                return Err(BridgeError::invalid(
                    "settings.allowed_callers contains an unknown caller",
                ));
            }
        }
    }
    if !direct {
        return Err(BridgeError::UnsupportedPolicy(
            "web search is not allowed for a direct model caller",
        ));
    }
    Ok(())
}

fn parse_domains_value(value: &Value) -> Result<Vec<String>, BridgeError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Value::Array(items) = value else {
        return Err(BridgeError::invalid("domain filters must be arrays"));
    };
    let mut domains = Vec::with_capacity(items.len());
    for item in items {
        let Some(raw) = item.as_str() else {
            return Err(BridgeError::invalid("domain filters must be strings"));
        };
        domains.push(raw.to_string());
    }
    parse_domain_list(domains)
}

fn parse_domain_list(domains: Vec<String>) -> Result<Vec<String>, BridgeError> {
    let mut unique = Vec::with_capacity(domains.len());
    for domain in domains {
        let domain = normalise_domain(&domain)?;
        if !unique.contains(&domain) {
            unique.push(domain);
        }
    }
    if unique.len() > MAX_DOMAINS_PER_FILTER {
        return Err(BridgeError::TooManyDomains(unique.len()));
    }
    Ok(unique)
}

fn normalise_domain(raw: &str) -> Result<String, BridgeError> {
    let domain = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    let valid = !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    if !valid {
        return Err(BridgeError::invalid(
            "domain filters must be bare ASCII DNS names",
        ));
    }
    Ok(domain)
}

fn apply_site_filters(query: &str, groups: &[&[String]]) -> String {
    let mut out = query.to_string();
    for group in groups {
        match group {
            [] => {}
            [domain] => {
                out.push_str(" site:");
                out.push_str(domain);
            }
            domains => {
                out.push_str(" (");
                for (index, domain) in domains.iter().enumerate() {
                    if index > 0 {
                        out.push_str(" OR ");
                    }
                    out.push_str("site:");
                    out.push_str(domain);
                }
                out.push(')');
            }
        }
    }
    out
}

fn matches_domains(host: &str, domains: &[String]) -> bool {
    let host = host.trim_end_matches('.');
    domains.is_empty()
        || domains.iter().any(|domain| {
            host == domain
                || host
                    .strip_suffix(domain)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
}

fn render_section(
    output: &mut String,
    results: &mut Vec<TextResult>,
    next_ref: &mut usize,
    query: &PlannedQuery,
    response: &crate::search::SearchResponse,
) {
    let start = output.len();
    output.push_str("Web search results for: ");
    output.push_str(&query.display);
    if response.is_empty() {
        output.push_str("\n\nNo results.");
    }

    // Leave room for the "\n\n" separator between sections so the aggregate
    // body never exceeds MAX_OUTPUT_BYTES.
    let budget = MAX_QUERY_OUTPUT_BYTES.saturating_sub(2);
    let total = response.results.len();
    let mut omitted = response.omitted;
    for (index, result) in response.results.iter().enumerate() {
        let ref_id = format!("{REF_ID_PREFIX}{next_ref}");
        let block = render_block(&ref_id, result);
        let reserve = omitted_line(omitted + (total - index)).len() + 2;
        if output.len() - start + 2 + block.len() + reserve > budget {
            omitted += total - index;
            break;
        }
        *next_ref += 1;
        output.push_str("\n\n");
        output.push_str(&block);
        results.push(TextResult {
            kind: "text_result",
            ref_id,
            url: result.url.clone(),
            title: display_title(result),
            snippet: result.snippets.join(" "),
        });
    }
    if omitted > 0 {
        output.push_str("\n\n");
        output.push_str(&omitted_line(omitted));
    }
}

fn render_block(ref_id: &str, result: &SearchResult) -> String {
    let mut out = format!("[{ref_id}] {}\n{}", display_title(result), result.url);
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

fn display_title(result: &SearchResult) -> String {
    if result.title.trim().is_empty() {
        "Untitled".to_string()
    } else {
        result.title.clone()
    }
}

fn omitted_line(omitted: usize) -> String {
    format!("({omitted} more sources omitted)")
}

fn normalise(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cut_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    const CUT: &str = "…";
    if max < CUT.len() {
        return String::new();
    }
    let mut end = max - CUT.len();
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{CUT}", value[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchError;

    #[test]
    fn domains_are_validated_and_normalised() {
        assert_eq!(
            normalise_domain("  Docs.Rust-Lang.org. ").unwrap(),
            "docs.rust-lang.org"
        );
        for bad in [
            "",
            "https://rust-lang.org",
            "rust-lang.org/path",
            "rust lang.org",
            "*.rust-lang.org",
            "-rust.org",
            "rust-.org",
            "rust..org",
        ] {
            assert!(normalise_domain(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn site_filters_form_one_and_group_per_constraint() {
        let global = vec!["rust-lang.org".to_string()];
        let local = vec!["doc.rust-lang.org".to_string(), "docs.rs".to_string()];
        assert_eq!(
            apply_site_filters("rust", &[&global, &local]),
            "rust site:rust-lang.org (site:doc.rust-lang.org OR site:docs.rs)"
        );
        assert_eq!(apply_site_filters("rust", &[&[]]), "rust");
    }

    #[test]
    fn recency_only_maps_exact_buckets() {
        assert_eq!(freshness_for_days(1), Ok("pd"));
        assert_eq!(freshness_for_days(7), Ok("pw"));
        assert_eq!(freshness_for_days(30), Ok("pm"));
        assert_eq!(freshness_for_days(365), Ok("py"));
        assert_eq!(
            freshness_for_days(3),
            Err(BridgeError::UnsupportedRecency(3))
        );
    }

    #[test]
    fn error_codes_and_statuses_are_stable() {
        let cases = [
            (BridgeError::InvalidJson, 400, "invalid_json"),
            (
                BridgeError::UnsupportedOperation("open".to_string()),
                400,
                "unsupported_operation",
            ),
            (
                BridgeError::UnsupportedPolicy("disabled"),
                403,
                "unsupported_policy",
            ),
            (
                BridgeError::Upstream(SearchError::RateLimited),
                429,
                "upstream_rate_limited",
            ),
            (
                BridgeError::Upstream(SearchError::Timeout),
                504,
                "upstream_timeout",
            ),
            (
                BridgeError::Upstream(SearchError::MalformedResponse),
                502,
                "upstream_bad_response",
            ),
        ];
        for (error, status, code) in cases {
            assert_eq!(error.status(), status);
            assert_eq!(error.code(), code);
        }
    }

    #[test]
    fn output_is_byte_bounded() {
        let long = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let cut = cut_bytes(&long, MAX_OUTPUT_BYTES);
        assert!(cut.len() <= MAX_OUTPUT_BYTES);
        assert!(cut.is_char_boundary(cut.len()));
    }
}
