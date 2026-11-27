//! Translated upstream transport: Responses request in, vendor wire request
//! out, vendor SSE in, Responses SSE out.
//!
//! One request is translated, sent to the configured same-origin endpoint, and
//! its body is reframed incrementally. Nothing is buffered end to end, so a
//! long answer streams as it arrives. A stream that ends before a terminal
//! Responses event is reported as `response.failed`; it is never completed
//! silently. Dropping the downstream body cancels the upstream request. Once
//! the terminal event is sent, the short rest of the upstream body is read in
//! the background so its connection is reused by the next request.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::stream::{self, Stream, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use hyper::{Response, StatusCode};
use reqwest::Client;
use serde_json::{Value, json};

use crate::adapters::{anthropic, chat_completions};
use crate::provider::{ChatOptions, RelayError, SecretKey, ThinkingMode, Vendor, Wire};
use crate::sse::SseFramer;
use crate::translate::{EventRewriter, ToolMap, normalize_request};

/// Largest upstream error body read for its message.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Largest single upstream SSE event.
const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// Bounds the cumulative text, tool arguments, and events a translator may retain.
const MAX_TRANSLATED_STREAM_BYTES: usize = 32 * 1024 * 1024;
/// Longest upstream error message relayed back.
const MAX_ERROR_CHARS: usize = 400;
/// Longest a finished stream may take to deliver the end of its HTTP body
/// before its connection is closed instead of reused.
const RECYCLE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a chat completions stream may stay silent after its `finish_reason`
/// before the relay closes the response itself. Without this, an upstream that
/// sends the finish chunk and then never sends `[DONE]` or closes would hold the
/// response open forever.
const FINISH_GRACE: Duration = Duration::from_secs(5);
/// Most trailing bytes read after a terminal event before giving up on reuse.
const RECYCLE_MAX_BYTES: usize = 64 * 1024;

pub struct Translation<'a> {
    pub vendor: &'a Vendor,
    pub key: &'a SecretKey,
    pub incoming: Option<&'a reqwest::header::HeaderMap>,
    pub upstream_model: &'a str,
    pub requested_model: &'a str,
    pub secrets: Arc<Vec<String>>,
    pub adapt: &'a crate::adapt::Memory,
    pub reasoning: &'a crate::provider::ReasoningPolicy,
}

/// Translate, send, and stream one request to a translated vendor.
pub async fn translated(
    client: &Client,
    translation: Translation<'_>,
    mut payload: Value,
) -> Response<crate::RelayBody> {
    let vendor = translation.vendor;
    let prepared = match prepare_body(vendor, &mut payload, translation.upstream_model) {
        Ok(prepared) => prepared,
        Err(message) => return bad_request(&message),
    };
    let (mut body, map) = prepared;
    drop(payload);
    // Rules learned from an earlier rejection in this process apply before the
    // request leaves, so a repeated turn does not pay the same 400 again.
    translation
        .adapt
        .apply_known(vendor.key.as_str(), translation.upstream_model, &mut body);

    let mut headers = match vendor.inference_headers(translation.key, translation.incoming) {
        Ok(headers) => headers,
        Err(_) => return bad_gateway("could not build the upstream authentication header"),
    };
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    let upstream = {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let upstream = client
                .post(vendor.endpoint())
                .headers(headers.clone())
                .body(body.to_string())
                .send()
                .await;
            let upstream = match upstream {
                Ok(response) => response,
                Err(_) => return bad_gateway("the upstream request failed"),
            };
            if upstream.status().is_success() {
                break upstream;
            }
            let status = upstream.status();
            let retry_after = upstream
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok());
            let error_body = read_error_body(upstream).await;
            let action = error_body
                .as_ref()
                .and_then(extract_error_message)
                .and_then(|message| crate::adapt::classify(status.as_u16(), message, &body));
            if attempt < crate::adapt::MAX_ATTEMPTS
                && let Some(action) = action
                && crate::adapt::apply(action, &mut body)
            {
                if translation
                    .adapt
                    .learn(vendor.key.as_str(), translation.upstream_model, action)
                {
                    eprintln!(
                        "ox: provider {:?}: model {:?}: {} because the provider rejected it\n  fix: {}",
                        vendor.key,
                        translation.upstream_model,
                        action.label(),
                        action.advice()
                    );
                }
                continue;
            }
            let message =
                upstream_error(status.as_u16(), error_body.as_ref(), &translation.secrets);
            return upstream_failure(status, retry_after, &message, &translation.secrets);
        }
    };
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if !content_type.starts_with("text/event-stream") {
        let body = read_error_body(upstream).await;
        let detail = body
            .as_ref()
            .and_then(extract_error_message)
            .map(|message| format!(": {}", truncate(&redact(message, &translation.secrets))))
            .unwrap_or_default();
        return bad_gateway(&format!(
            "provider {:?} returned a non-streaming response{}",
            vendor.key, detail
        ));
    }

    let translator = match vendor.wire {
        Wire::AnthropicMessages => {
            Translator::Anthropic(anthropic::ResponseTranslator::with_created_at_and_model(
                now_seconds(),
                translation.upstream_model,
            ))
        }
        Wire::ChatCompletions => Translator::Chat(chat_completions::ResponseTranslator::new(
            response_id(),
            translation.requested_model,
            now_seconds() as i64,
        )),
        Wire::Responses => unreachable!("the Responses wire is not translated"),
    };
    let state = SseState {
        upstream: upstream.bytes_stream().boxed(),
        framer: SseFramer::new(MAX_EVENT_BYTES),
        translator,
        rewriter: EventRewriter::new(map),
        secrets: translation.secrets,
        requested_model: translation.requested_model.to_owned(),
        queue: VecDeque::new(),
        remaining_bytes: MAX_TRANSLATED_STREAM_BYTES,
        done: false,
        terminal_sent: false,
        failed: false,
        finish_grace: FINISH_GRACE,
        finish_deadline: None,
    };
    let body = stream::unfold(state, |mut state| async move {
        loop {
            if let Some(chunk) = state.queue.pop_front() {
                return Some((Ok::<_, RelayError>(Frame::data(chunk)), state));
            }
            if state.done {
                return None;
            }
            state.pump().await;
        }
    });
    let mut response = Response::new(StreamBody::new(body).boxed_unsync());
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Build the vendor request body and the tool-name mapping used to translate
/// the stream back.
fn prepare_body(
    vendor: &Vendor,
    payload: &mut Value,
    upstream_model: &str,
) -> Result<(Value, ToolMap), String> {
    let normalized = normalize_request(payload)?;
    if payload.get("max_output_tokens").is_none_or(Value::is_null) {
        match vendor.wire {
            Wire::AnthropicMessages => {
                // Anthropic requires max_tokens, and Codex does not send it.
                let tokens = vendor.messages_output_default();
                payload["max_output_tokens"] = json!(tokens);
            }
            Wire::ChatCompletions => {
                if let Some(tokens) = vendor.max_output_tokens {
                    payload["max_output_tokens"] = json!(tokens);
                }
            }
            Wire::Responses => {}
        }
    }
    let mut body = match vendor.wire {
        Wire::AnthropicMessages => {
            let mut body = anthropic::translate_request(payload, upstream_model)?;
            finish_anthropic_body(&mut body, vendor)?;
            body
        }
        Wire::ChatCompletions => {
            let options = chat_options(&vendor.chat);
            chat_completions::translate_request_with(payload, upstream_model, &options)?
        }
        Wire::Responses => return Err("the Responses wire is not translated".to_owned()),
    };
    body.as_object_mut()
        .ok_or("the translated request must be a JSON object")?;
    vendor.add_cache_hints(&mut body);
    // The Chat Completions translator forwards a fixed field list, so the
    // OpenRouter extensions OX added to the canonical request are copied here.
    if vendor.is_openrouter()
        && let Some(object) = body.as_object_mut()
    {
        if let Some(preferences) = &vendor.openrouter_provider {
            object
                .entry("provider")
                .or_insert_with(|| preferences.clone());
        }
        if let Some(session) = payload.get("session_id") {
            object
                .entry("session_id")
                .or_insert_with(|| session.clone());
        }
    }
    Ok((body, normalized.map))
}

fn finish_anthropic_body(body: &mut Value, vendor: &Vendor) -> Result<(), String> {
    let object = body
        .as_object_mut()
        .ok_or("the translated Anthropic request must be an object")?;
    if !vendor.anthropic.wants_effort() {
        object.remove("output_config");
    }
    let max_tokens = object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .ok_or("the translated Anthropic request needs max_tokens")?;
    match vendor.anthropic.thinking {
        ThinkingMode::Off => {
            object.remove("thinking");
        }
        ThinkingMode::Manual => {
            let budget = vendor.anthropic.thinking_budget_tokens;
            if budget >= max_tokens {
                return Err(format!(
                    "anthropic_thinking_budget_tokens ({budget}) must be smaller than max_output_tokens ({max_tokens})"
                ));
            }
            object.insert(
                "thinking".to_owned(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
        }
        ThinkingMode::Adaptive => {
            object.insert("thinking".to_owned(), json!({"type": "adaptive"}));
        }
    }
    if vendor.anthropic.rejects_sampling() || vendor.anthropic.thinking != ThinkingMode::Off {
        strip_anthropic_sampling(object);
    }
    Ok(())
}

pub(crate) fn strip_anthropic_sampling(object: &mut serde_json::Map<String, Value>) {
    for field in ["temperature", "top_p", "top_k"] {
        object.remove(field);
    }
}

fn chat_options(options: &ChatOptions) -> chat_completions::AdapterOptions {
    chat_completions::AdapterOptions {
        max_tokens_field: match options.max_tokens_field {
            crate::provider::MaxTokensField::MaxTokens => {
                chat_completions::MaxTokensField::MaxTokens
            }
            crate::provider::MaxTokensField::MaxCompletionTokens => {
                chat_completions::MaxTokensField::MaxCompletionTokens
            }
        },
        reasoning_effort: match options.reasoning_effort {
            crate::provider::ReasoningEffortPolicy::Drop => chat_completions::ReasoningEffort::Drop,
            crate::provider::ReasoningEffortPolicy::Forward => {
                chat_completions::ReasoningEffort::Forward
            }
        },
        developer_role: match options.developer_role {
            crate::provider::DeveloperRole::System => chat_completions::DeveloperRole::System,
            crate::provider::DeveloperRole::Developer => chat_completions::DeveloperRole::Developer,
        },
        include_stream_usage: options.include_stream_usage,
    }
}

/// Read a finished upstream body to its end in the background.
///
/// The translators stop at the protocol's terminal event, which arrives before
/// the HTTP body's own end marker. Dropping the body at that point makes the
/// HTTP client close the connection, so the next request would pay a new TCP
/// and TLS handshake. Draining the short tail returns the connection to the
/// pool instead. The downstream response never waits for this.
pub(crate) fn recycle<S, E>(mut body: S)
where
    S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Send + 'static,
{
    tokio::spawn(async move {
        let drain = async {
            let mut budget = RECYCLE_MAX_BYTES;
            while let Some(Ok(chunk)) = body.next().await {
                match budget.checked_sub(chunk.len()) {
                    Some(left) => budget = left,
                    None => break,
                }
            }
        };
        let _ = tokio::time::timeout(RECYCLE_TIMEOUT, drain).await;
    });
}

enum Translator {
    Anthropic(anthropic::ResponseTranslator),
    Chat(chat_completions::ResponseTranslator),
}

impl Translator {
    fn push_event(&mut self, event: &str, data: &str) -> Result<Vec<Value>, String> {
        match self {
            Self::Anthropic(translator) => translator.push_event(event, data),
            Self::Chat(translator) => translator.push_event(event, data),
        }
    }

    fn finish(&mut self) -> Result<Vec<Value>, String> {
        match self {
            Self::Anthropic(translator) => translator.finish(),
            Self::Chat(translator) => translator.finish(),
        }
    }

    fn is_terminal(&self) -> bool {
        match self {
            Self::Anthropic(translator) => translator.is_terminal(),
            Self::Chat(translator) => translator.is_finished(),
        }
    }

    /// Whether the upstream has already announced the end of its answer. Only
    /// chat completions has a separate finish_reason; the Anthropic translator
    /// keeps the unbounded wait.
    fn has_finish_reason(&self) -> bool {
        match self {
            Self::Anthropic(_) => false,
            Self::Chat(translator) => translator.has_finish_reason(),
        }
    }
}

struct SseState {
    upstream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    framer: SseFramer,
    translator: Translator,
    rewriter: EventRewriter,
    secrets: Arc<Vec<String>>,
    requested_model: String,
    queue: VecDeque<Bytes>,
    remaining_bytes: usize,
    done: bool,
    terminal_sent: bool,
    failed: bool,
    /// Bound on the wait for the next upstream chunk once a finish_reason has
    /// arrived. Unbounded before that.
    finish_grace: Duration,
    /// When the grace period ends. Fixed the first time the stream is seen
    /// finished, so keepalive comments cannot renew it; only the final usage
    /// chunk is expected inside the window.
    finish_deadline: Option<tokio::time::Instant>,
}

impl SseState {
    /// Pull until at least one downstream chunk is queued or the stream ends.
    async fn pump(&mut self) {
        if self.terminal_sent || self.failed {
            self.done = true;
            return;
        }
        let next = if self.translator.has_finish_reason() {
            let deadline = *self
                .finish_deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + self.finish_grace);
            match tokio::time::timeout_at(deadline, self.upstream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    // The answer is complete; the upstream just never closed. Drop the
                    // connection and finish the response normally.
                    self.upstream = stream::empty().boxed();
                    self.finish_upstream();
                    return;
                }
            }
        } else {
            self.upstream.next().await
        };
        match next {
            None => self.finish_upstream(),
            Some(Err(_)) => {
                self.fail("the upstream stream ended with a transport error");
            }
            Some(Ok(chunk)) => {
                let Some(remaining) = self.remaining_bytes.checked_sub(chunk.len()) else {
                    self.fail("the upstream stream exceeded the relay's translated response limit");
                    return;
                };
                self.remaining_bytes = remaining;
                let frames = match self.framer.feed(&chunk) {
                    Ok(frames) => frames,
                    Err(message) => {
                        self.fail(&message);
                        return;
                    }
                };
                for frame in frames {
                    let event = frame.event.as_deref().unwrap_or("");
                    match self.translator.push_event(event, &frame.data) {
                        Ok(events) => {
                            if self.emit(events).is_err() {
                                return;
                            }
                            if self.translator.is_terminal() {
                                self.terminal_sent = true;
                                self.done = true;
                                recycle(std::mem::replace(
                                    &mut self.upstream,
                                    stream::empty().boxed(),
                                ));
                                return;
                            }
                        }
                        Err(message) => {
                            self.fail(&message);
                            return;
                        }
                    }
                }
            }
        }
    }

    fn finish_upstream(&mut self) {
        if self.terminal_sent || self.failed {
            return;
        }
        if let Err(message) = self.framer.finish() {
            self.fail(&message);
            return;
        }
        match self.translator.finish() {
            Ok(events) => {
                if self.emit(events).is_err() {
                    return;
                }
                if self.translator.is_terminal() {
                    self.terminal_sent = true;
                } else {
                    self.fail("the upstream stream ended without a terminal response event");
                }
            }
            Err(message) => self.fail(&message),
        }
    }

    fn emit(&mut self, events: Vec<Value>) -> Result<(), ()> {
        for event in events {
            match self.rewriter.rewrite(event) {
                Ok(rewritten) => {
                    for event in rewritten {
                        self.enqueue(event);
                    }
                }
                Err(message) => {
                    self.fail(&message);
                    return Err(());
                }
            }
            if self.failed {
                return Err(());
            }
        }
        Ok(())
    }

    fn enqueue(&mut self, event: Value) {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
            .to_owned();
        let encoded = event.to_string();
        let data = redact(&encoded, &self.secrets);
        let mut frame = Vec::with_capacity(kind.len() + data.len() + 16);
        frame.extend_from_slice(b"event: ");
        frame.extend_from_slice(kind.as_bytes());
        frame.extend_from_slice(b"\ndata: ");
        frame.extend_from_slice(data.as_bytes());
        frame.extend_from_slice(b"\n\n");
        self.queue.push_back(Bytes::from(frame));
    }

    /// Emit one terminal `response.failed` and stop. The message is already
    /// derived from translator errors that may embed upstream text, so it is
    /// redacted before serialization.
    fn fail(&mut self, message: &str) {
        if self.terminal_sent || self.failed {
            return;
        }
        self.failed = true;
        self.done = true;
        let sequence = self.rewriter.sequence();
        let message = truncate(&redact(message, &self.secrets));
        let event = json!({
            "type": "response.failed",
            "sequence_number": sequence,
            "response": {
                "id": "resp_codeox",
                "object": "response",
                "created_at": now_seconds(),
                "status": "failed",
                "model": self.requested_model,
                "output": [],
                "parallel_tool_calls": true,
                "tool_choice": "auto",
                "tools": [],
                "error": {"code": "relay_stream_error", "message": message},
                "incomplete_details": Value::Null,
                "usage": Value::Null,
            },
        });
        self.enqueue(event);
    }
}

pub(crate) async fn read_error_body(response: reqwest::Response) -> Option<Value> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ERROR_BODY as u64)
    {
        return None;
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if buffer.len() + chunk.len() > MAX_ERROR_BODY {
            return None;
        }
        buffer.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buffer).ok()
}

pub(crate) fn upstream_error(status: u16, body: Option<&Value>, secrets: &[String]) -> String {
    let detail = body
        .and_then(extract_error_message)
        .map(|message| format!(": {}", truncate(&redact(message, secrets))))
        .unwrap_or_default();
    format!("provider returned HTTP {status}{detail}")
}

pub(crate) fn extract_error_message(value: &Value) -> Option<&str> {
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .or_else(|| value.get("message"))
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)?;
    (!message.trim().is_empty()).then_some(message)
}

/// Replace each configured secret in `text`. Secrets shorter than 4 bytes are left alone on
/// purpose: a 1-3 byte "secret" would mangle ordinary text (every "a" or "42"), and real API
/// keys and tokens are far longer (provider keys are 20+ bytes), so nothing real is skipped.
pub(crate) fn redact<'a>(text: &'a str, secrets: &[String]) -> Cow<'a, str> {
    let mut output = Cow::Borrowed(text);
    for secret in secrets {
        if secret.len() >= 4 && output.contains(secret) {
            output = Cow::Owned(output.replace(secret, "[redacted]"));
        }
    }
    output
}

fn truncate(message: &str) -> String {
    if message.chars().count() <= MAX_ERROR_CHARS {
        return message.to_owned();
    }
    let mut truncated = message.chars().take(MAX_ERROR_CHARS).collect::<String>();
    truncated.push('…');
    truncated
}

fn response_id() -> String {
    format!("resp_codeox_{}", now_nanos())
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn bad_request(message: &str) -> Response<crate::RelayBody> {
    crate::error_response(hyper::StatusCode::BAD_REQUEST, message)
}

fn bad_gateway(message: &str) -> Response<crate::RelayBody> {
    crate::error_response(hyper::StatusCode::BAD_GATEWAY, message)
}

fn upstream_failure(
    status: StatusCode,
    retry_after: Option<u32>,
    message: &str,
    secrets: &[String],
) -> Response<crate::RelayBody> {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return bad_gateway(message);
    }
    let mut response = crate::error_response(status, message);
    forward_retry_after(&mut response, status, retry_after, secrets);
    response
}

/// A native Responses status that must be reduced before it reaches Codex.
/// Redirects and other non-error statuses keep their existing passthrough.
pub(crate) fn is_http_failure(response: &reqwest::Response) -> bool {
    let status = response.status();
    status.is_client_error() || status.is_server_error()
}

/// Preserve the status and structured errors Codex uses to identify context
/// and quota failures. Only bounded, credential-free fields reach the client.
pub(crate) async fn http_failure(
    response: reqwest::Response,
    secrets: &[String],
) -> Response<crate::RelayBody> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok());
    let body = read_error_body(response).await;
    http_failure_parts(status, retry_after, body, secrets)
}

/// Builds the same bounded error response `http_failure` produces, from a body
/// that was already read. Adaptation needs the body before deciding whether to
/// retry, and must still return the original error when it does not.
pub(crate) fn http_failure_parts(
    status: StatusCode,
    retry_after: Option<u32>,
    body: Option<Value>,
    secrets: &[String],
) -> Response<crate::RelayBody> {
    let message = upstream_error(status.as_u16(), body.as_ref(), secrets);
    let mut error = json!({ "message": message });
    if let Some(provider_error) = body.as_ref().and_then(|body| body.get("error")) {
        for field in ["code", "type", "error_type"] {
            let value = provider_error.get(field).and_then(Value::as_str);
            if let Some(value) = value
                && (1..=128).contains(&value.len())
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && !secrets.iter().any(|secret| value.contains(secret))
            {
                error[field] = json!(value);
            }
        }
    }
    if let Some(error_type) = body.as_ref().and_then(|body| body.get("error_type"))
        && let Some(value) = error_type.as_str()
        && (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !secrets.iter().any(|secret| value.contains(secret))
    {
        error["error_type"] = json!(value);
    }
    let mut downstream = crate::json_response(status, json!({ "error": error }));
    forward_retry_after(&mut downstream, status, retry_after, secrets);
    downstream
}

/// Copy only a well-formed numeric `Retry-After` on `429`. Never forwards the
/// header verbatim, and never forwards it when it would echo a credential.
fn forward_retry_after(
    response: &mut Response<crate::RelayBody>,
    status: StatusCode,
    retry_after: Option<u32>,
    secrets: &[String],
) {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return;
    }
    let Some(delay) = retry_after else {
        return;
    };
    let delay = delay.to_string();
    if secrets.iter().any(|secret| delay.contains(secret)) {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(&delay) {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_keys_of_real_length_are_redacted_and_tiny_ones_do_not_mangle_text() {
        let secrets = vec!["sk-12345678".to_owned(), "ab".to_owned()];
        assert_eq!(
            redact("key sk-12345678 and abc", &secrets),
            "key [redacted] and abc"
        );
    }

    fn openrouter_vendor(wire: Wire) -> Vendor {
        let url = reqwest::Url::parse("https://eu.openrouter.ai/api/v1").unwrap();
        Vendor {
            key: "openrouter".to_owned(),
            name: "OpenRouter".to_owned(),
            base_url: url.clone(),
            models_url: crate::provider::openrouter_models_url(&url),
            env_key: None,
            wire,
            discovery: false,
            models: Vec::new(),
            openrouter_provider: Some(json!({"sort": "throughput"})),
            max_output_tokens: None,
            anthropic: crate::provider::AnthropicOptions::default(),
            chat: ChatOptions::default(),
        }
    }

    #[test]
    fn openrouter_chat_carries_preferences_and_the_session() {
        let vendor = openrouter_vendor(Wire::ChatCompletions);
        let mut payload = json!({
            "model": "alias",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "hi"}
            ]}],
            "session_id": "ox_abc",
            "stream": true,
        });
        let (body, _) = prepare_body(&vendor, &mut payload, "upstream-model").unwrap();
        assert_eq!(body["provider"]["sort"], "throughput");
        assert_eq!(body["session_id"], "ox_abc");

        // A provider that is not OpenRouter gets neither extension.
        let mut vendor = openrouter_vendor(Wire::ChatCompletions);
        vendor.base_url = reqwest::Url::parse("https://alpha.example/v1").unwrap();
        vendor.openrouter_provider = None;
        let mut payload = json!({
            "model": "alias",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "hi"}
            ]}],
            "session_id": "ox_abc",
            "stream": true,
        });
        let (body, _) = prepare_body(&vendor, &mut payload, "upstream-model").unwrap();
        assert!(body.get("provider").is_none());
        assert!(body.get("session_id").is_none());
    }

    #[tokio::test]
    async fn oversized_translated_stream_fails_before_consuming_the_next_chunk() {
        let mut state = SseState {
            upstream: stream::iter([Ok(Bytes::from_static(b"event: ping\n\ndata: ignored\n\n"))])
                .boxed(),
            framer: SseFramer::new(MAX_EVENT_BYTES),
            translator: Translator::Anthropic(anthropic::ResponseTranslator::new()),
            rewriter: EventRewriter::new(ToolMap::default()),
            secrets: Arc::new(Vec::new()),
            requested_model: "fixture/model".to_owned(),
            queue: VecDeque::new(),
            remaining_bytes: 12,
            done: false,
            terminal_sent: false,
            failed: false,
            finish_grace: FINISH_GRACE,
            finish_deadline: None,
        };
        state.pump().await;
        assert!(state.failed && state.done);
        let chunk = state.queue.pop_front().unwrap();
        let event = std::str::from_utf8(&chunk).unwrap();
        assert!(event.starts_with("event: response.failed\n"));
        assert!(event.contains("translated response limit"));
        assert!(state.queue.is_empty());
    }

    fn chat_state(
        upstream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    ) -> SseState {
        SseState {
            upstream,
            framer: SseFramer::new(MAX_EVENT_BYTES),
            translator: Translator::Chat(chat_completions::ResponseTranslator::new(
                "resp_test",
                "fixture/model",
                0,
            )),
            rewriter: EventRewriter::new(ToolMap::default()),
            secrets: Arc::new(Vec::new()),
            requested_model: "fixture/model".to_owned(),
            queue: VecDeque::new(),
            remaining_bytes: MAX_TRANSLATED_STREAM_BYTES,
            done: false,
            terminal_sent: false,
            failed: false,
            finish_grace: Duration::from_millis(20),
            finish_deadline: None,
        }
    }

    const FINISH_CHUNK: &[u8] =
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";

    #[tokio::test]
    async fn chat_stream_without_done_completes_after_finish_reason_grace() {
        // The finish chunk arrives, then the upstream never sends [DONE] or closes.
        let upstream = stream::iter([Ok(Bytes::from_static(FINISH_CHUNK))])
            .chain(stream::pending())
            .boxed();
        let mut state = chat_state(upstream);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.done {
                state.pump().await;
            }
        })
        .await
        .expect("the response must finish once the grace period lapses");
        assert!(state.terminal_sent && !state.failed);
        let mut text = String::new();
        while let Some(chunk) = state.queue.pop_front() {
            text.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        assert!(text.contains("event: response.completed\n"), "{text}");
    }

    #[tokio::test]
    async fn heartbeats_after_finish_reason_do_not_renew_the_grace_deadline() {
        let heartbeats = stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some((Ok(Bytes::from_static(b": keep-alive\n\n")), ()))
        });
        let upstream = stream::iter([Ok(Bytes::from_static(FINISH_CHUNK))])
            .chain(heartbeats)
            .boxed();
        let mut state = chat_state(upstream);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.done {
                state.pump().await;
            }
        })
        .await
        .expect("heartbeats must not keep a finished response open");
        assert!(state.terminal_sent && !state.failed);
    }

    #[tokio::test]
    async fn the_final_usage_chunk_still_lands_inside_the_grace_window() {
        let usage = Bytes::from_static(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
        );
        let late_usage = stream::once(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok::<_, reqwest::Error>(usage)
        });
        let upstream = stream::iter([Ok(Bytes::from_static(FINISH_CHUNK))])
            .chain(late_usage)
            .chain(stream::pending())
            .boxed();
        let mut state = chat_state(upstream);
        // The window must outlast a pause of this thread between arming it and
        // starting the 5 ms wait above. On a loaded four-core machine a running
        // thread stayed off the CPU for up to 0.46 s.
        state.finish_grace = Duration::from_secs(1);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.done {
                state.pump().await;
            }
        })
        .await
        .unwrap();
        let mut text = String::new();
        while let Some(chunk) = state.queue.pop_front() {
            text.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        assert!(text.contains("\"total_tokens\":5"), "{text}");
    }

    #[tokio::test]
    async fn chat_stream_waits_unbounded_before_finish_reason() {
        // No finish_reason yet: the grace timer must not apply, so a silent
        // upstream is still being awaited after well over the grace period.
        let upstream = stream::iter([Ok(Bytes::from_static(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
        ))])
        .chain(stream::pending())
        .boxed();
        let mut state = chat_state(upstream);
        state.pump().await;
        assert!(!state.done && !state.terminal_sent);
        let waited = tokio::time::timeout(Duration::from_millis(200), state.pump()).await;
        assert!(
            waited.is_err(),
            "pump must keep waiting before a finish_reason"
        );
        assert!(!state.terminal_sent && !state.failed);
    }

    #[tokio::test]
    async fn recycle_reads_a_finished_body_to_its_end() {
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&ended);
        let tail = stream::once(async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, RelayError>(Bytes::new())
        });
        let body = stream::iter([Ok::<_, RelayError>(Bytes::from_static(b"tail"))]).chain(tail);
        recycle(Box::pin(body));
        for _ in 0..100 {
            if ended.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(ended.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn recycle_gives_up_on_an_oversized_tail() {
        let polled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&polled);
        let chunk = Bytes::from(vec![b'x'; RECYCLE_MAX_BYTES / 2]);
        let body = stream::repeat_with(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, RelayError>(chunk.clone())
        });
        recycle(Box::pin(body));
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        // Two chunks use the whole budget; the third ends the drain.
        assert_eq!(polled.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn error_bodies_are_reduced_to_a_redacted_message() {
        let body = json!({"error": {"message": "bad key sk-live-12345", "type": "auth"}});
        let message = upstream_error(401, Some(&body), &["sk-live-12345".to_owned()]);
        assert_eq!(message, "provider returned HTTP 401: bad key [redacted]");
        assert!(!message.contains("sk-live"));
    }

    #[test]
    fn truncation_does_not_expose_a_partial_credential() {
        let secret = "sk-fixture-key";
        let body = json!({"message": format!("{}{}", "x".repeat(MAX_ERROR_CHARS - 5), secret)});
        let message = upstream_error(401, Some(&body), &[secret.to_owned()]);
        assert!(!message.contains("sk-fi"));
    }

    #[test]
    fn translates_rate_limits_with_numeric_retry_after() {
        let status = StatusCode::TOO_MANY_REQUESTS;
        let response = upstream_failure(status, Some(7), "rate limited", &[]);
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()[RETRY_AFTER], "7");

        let response = upstream_failure(status, Some(1234), "rate limited", &["1234".to_owned()]);
        assert!(!response.headers().contains_key(RETRY_AFTER));
    }

    #[test]
    fn missing_error_bodies_have_a_status_message() {
        let message = upstream_error(500, None, &[]);
        assert_eq!(message, "provider returned HTTP 500");
    }

    #[test]
    fn redaction_covers_every_occurrence() {
        assert_eq!(
            redact("a SECRET b SECRET", &["SECRET".to_owned()]),
            "a [redacted] b [redacted]"
        );
        assert_eq!(redact("unchanged", &["short".to_owned()]), "unchanged");
    }

    #[test]
    fn truncation_is_char_safe() {
        let long = "é".repeat(MAX_ERROR_CHARS + 5);
        assert_eq!(truncate(&long).chars().count(), MAX_ERROR_CHARS + 1);
    }
}
