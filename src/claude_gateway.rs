//! Anthropic Messages gateway for stock Claude Code.
//!
//! Claude Code posts `POST /v1/messages` requests. This module answers them
//! from whichever wire the routed vendor speaks:
//!
//! * `anthropic_messages` is forwarded natively, so thinking blocks keep their
//!   signatures and both streaming and JSON pass through unchanged;
//! * `responses` is converted with [`crate::claude_request::into_responses`],
//!   sent to the vendor's Responses endpoint, and mapped back with
//!   [`crate::claude_response::MessagesStream`];
//! * `chat_completions` reuses [`crate::upstream::translated`] with the same
//!   canonical Responses request, then maps the resulting Responses stream back.
//!
//! Native requests keep Anthropic capability headers. Each upstream request
//! uses the vendor's configured credentials.
//!
//! Errors always use the Messages error envelope. HTTP failures keep their
//! status where it is meaningful, read at most a bounded error body, and redact
//! configured credentials before anything reaches the client.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, Stream, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::header::{
    ACCEPT, CACHE_CONTROL, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER,
};
use hyper::{Response, StatusCode};
use reqwest::Client;
use serde_json::{Value, json};

use crate::claude_response::MessagesStream;
use crate::provider::{AnthropicOptions, RelayError, SecretKey, Vendor, Wire, valid_api_version};
use crate::sse::SseFramer;
use crate::upstream::{self, Translation};

/// Largest upstream error body read to recover a message.
const MAX_UPSTREAM_ERROR_BYTES: usize = 64 * 1024;
/// Bound for a buffered non-streaming native Messages response.
const MAX_NATIVE_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Largest single upstream SSE event.
const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// Cumulative ceiling for the converted Messages stream.
const MAX_MESSAGES_STREAM_BYTES: usize = 32 * 1024 * 1024;
/// Longest upstream error message relayed back.
const MAX_ERROR_CHARS: usize = 400;
/// Longest caller-supplied `anthropic-beta` value forwarded.
const MAX_BETA_CHARS: usize = 2048;
const ANTHROPIC_BETA: HeaderName = HeaderName::from_static("anthropic-beta");
const ANTHROPIC_VERSION: HeaderName = HeaderName::from_static("anthropic-version");
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const COUNT_TOKENS_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_COUNT_BODY_BYTES: usize = 64 * 1024;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, RelayError>> + Send>>;

/// Answer one Claude Code Messages request from the routed vendor.
///
/// Anthropic beta headers are forwarded only on the native Anthropic wire.
pub(crate) async fn infer(
    client: &Client,
    context: Translation<'_>,
    payload: Value,
    beta: bool,
) -> Response<crate::RelayBody> {
    if !payload.is_object() {
        return messages_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "the request body must be a JSON object",
        );
    }
    match context.vendor.wire {
        Wire::AnthropicMessages => infer_native(client, context, payload, beta, false).await,
        Wire::Responses | Wire::ChatCompletions => infer_translated(client, context, payload).await,
    }
}

pub(crate) async fn count_tokens(
    client: &Client,
    context: Translation<'_>,
    payload: Value,
    beta: bool,
) -> Response<crate::RelayBody> {
    match context.vendor.wire {
        Wire::AnthropicMessages => infer_native(client, context, payload, beta, true).await,
        Wire::Responses => responses_token_count(client, context, &payload).await,
        Wire::ChatCompletions => messages_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "this Chat Completions route has no standard token counting endpoint",
        ),
    }
}

async fn responses_token_count(
    client: &Client,
    context: Translation<'_>,
    payload: &Value,
) -> Response<crate::RelayBody> {
    let body = match crate::claude_request::into_input_token_count(payload, context.upstream_model)
    {
        Ok(body) => body,
        Err(message) => {
            return messages_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &safe_message(&message, &context.secrets),
            );
        }
    };
    let Ok(headers) = context
        .vendor
        .inference_headers(context.key, context.incoming)
    else {
        return messages_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "could not build the upstream authentication header",
        );
    };
    let result = client
        .post(
            context
                .vendor
                .responses_url("/responses/input_tokens", None),
        )
        .headers(headers)
        .json(&body)
        .timeout(COUNT_TOKENS_TIMEOUT)
        .send()
        .await;
    let Ok(response) = result else {
        return messages_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "the upstream token count request failed",
        );
    };
    if upstream::is_http_failure(&response) {
        return messages_http_failure(response, &context.secrets, false).await;
    }
    if response.status().is_success()
        && let Ok(bytes) = crate::read_upstream(response, MAX_COUNT_BODY_BYTES).await
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        && let Some(count) = value["input_tokens"].as_u64()
    {
        return crate::json_response(StatusCode::OK, json!({"input_tokens": count}));
    }
    messages_error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        "the upstream returned an invalid token count",
    )
}

/// Forward a Messages request after applying the vendor's model and API version.
async fn infer_native(
    client: &Client,
    context: Translation<'_>,
    mut payload: Value,
    beta: bool,
    count_tokens: bool,
) -> Response<crate::RelayBody> {
    let wants_stream = !count_tokens
        && payload
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let headers = match native_headers(context.vendor, context.key, context.incoming, wants_stream)
    {
        Ok(headers) => headers,
        Err(_) => {
            return messages_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "could not build the upstream authentication header",
            );
        }
    };
    if count_tokens {
        payload["model"] = json!(context.upstream_model);
    } else {
        let version = headers[ANTHROPIC_VERSION].to_str().unwrap_or_default();
        payload = prepare_native_body(payload, context.vendor, context.upstream_model, version);
    }
    let encoded = payload.to_string();
    drop(payload);

    let suffix = if count_tokens {
        "/messages/count_tokens"
    } else {
        "/messages"
    };
    let mut url = context.vendor.responses_url(suffix, None);
    if beta && context.vendor.is_direct_anthropic() {
        url.set_query(Some("beta=true"));
    }
    let mut request = client.post(url).headers(headers).body(encoded);
    if count_tokens {
        request = request.timeout(COUNT_TOKENS_TIMEOUT);
    }
    let upstream = request.send().await;
    let upstream = match upstream {
        Ok(response) => response,
        Err(_) => {
            return messages_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "the upstream request failed",
            );
        }
    };
    if upstream::is_http_failure(&upstream) {
        return messages_http_failure(upstream, &context.secrets, true).await;
    }
    if upstream.status().is_redirection() {
        return messages_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "provider returned an unexpected redirect",
        );
    }

    // Trust the response, not only the request flag: a vendor that does return
    // SSE must be streamed, and one that returned JSON is relayed as JSON.
    if !count_tokens && is_event_stream(upstream.headers()) {
        return crate::stream_response(upstream);
    }
    if wants_stream {
        let detail = read_upstream_detail(upstream, &context.secrets).await;
        return messages_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("provider returned a non-streaming response{detail}"),
        );
    }

    let status = upstream.status();
    let metadata = native_response_metadata(upstream.headers(), &context.secrets);
    let body_limit = if count_tokens {
        MAX_COUNT_BODY_BYTES
    } else {
        MAX_NATIVE_BODY_BYTES
    };
    let bytes = match crate::read_upstream(upstream, body_limit).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return messages_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "the upstream response was too large or unreadable",
            );
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) if !count_tokens || value["input_tokens"].as_u64().is_some() => {
            let mut response = crate::json_response(status, value);
            response.headers_mut().extend(metadata);
            response
        }
        _ => messages_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "the upstream returned an invalid response body",
        ),
    }
}

/// Convert a Messages request into a canonical Responses request and answer it
/// from either a Responses or a Chat Completions vendor.
async fn infer_translated(
    client: &Client,
    context: Translation<'_>,
    payload: Value,
) -> Response<crate::RelayBody> {
    let wants_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let requested_model = context.requested_model.to_owned();

    let canonical = match crate::claude_request::into_responses(&payload, context.upstream_model) {
        Ok(value) => value,
        Err(message) => {
            return messages_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &safe_message(&message, &context.secrets),
            );
        }
    };
    let mut canonical = canonical;
    if !canonical.is_object() {
        return messages_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "the relay could not translate the request",
        );
    }
    // Both translation paths consume Responses SSE; a non-streaming Messages
    // request is answered from the stream's terminal message below.
    canonical["stream"] = json!(true);

    let upstream = match context.vendor.wire {
        Wire::Responses => responses_upstream(client, &context, canonical).await,
        Wire::ChatCompletions => {
            upstream::translated(
                client,
                Translation {
                    vendor: context.vendor,
                    key: context.key,
                    incoming: context.incoming,
                    upstream_model: context.upstream_model,
                    requested_model: context.requested_model,
                    secrets: Arc::clone(&context.secrets),
                },
                canonical,
            )
            .await
        }
        Wire::AnthropicMessages => unreachable!("native Anthropic requests are not translated"),
    };
    if !upstream.status().is_success() {
        return messages_from_relay_error(upstream, &context.secrets).await;
    }

    let stream: ByteStream = Box::pin(relay_body_stream(upstream.into_body()));
    if wants_stream {
        stream_messages(
            stream,
            &requested_model,
            context.secrets,
            KEEPALIVE_INTERVAL,
        )
    } else {
        collect_message(stream, &requested_model, &context.secrets).await
    }
}

/// Send the canonical Responses request to a Responses-wire vendor.
async fn responses_upstream(
    client: &Client,
    context: &Translation<'_>,
    payload: Value,
) -> Response<crate::RelayBody> {
    let mut headers = match context
        .vendor
        .inference_headers(context.key, context.incoming)
    {
        Ok(headers) => headers,
        Err(_) => {
            return messages_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "could not build the upstream authentication header",
            );
        }
    };
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));

    let upstream = client
        .post(context.vendor.responses_url("/responses", None))
        .headers(headers)
        .body(payload.to_string())
        .send()
        .await;
    let upstream = match upstream {
        Ok(response) => response,
        Err(_) => {
            return messages_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "the upstream request failed",
            );
        }
    };
    if upstream::is_http_failure(&upstream) {
        return messages_http_failure(upstream, &context.secrets, false).await;
    }
    if !is_event_stream(upstream.headers()) {
        let detail = read_upstream_detail(upstream, &context.secrets).await;
        return messages_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("provider returned a non-streaming response{detail}"),
        );
    }
    crate::stream_response(upstream)
}

/// Answers a non-streaming Messages request from the converted SSE stream's
/// terminal message.
async fn collect_message(
    stream: ByteStream,
    requested_model: &str,
    secrets: &[String],
) -> Response<crate::RelayBody> {
    let mut converter = MessagesStream::new(requested_model);
    let mut framer = SseFramer::new(MAX_EVENT_BYTES);
    let mut remaining = MAX_MESSAGES_STREAM_BYTES;
    let mut error_detail = None;
    let mut stream = stream;

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return messages_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "the upstream stream ended with a transport error",
                );
            }
        };
        let Some(left) = remaining.checked_sub(chunk.len()) else {
            return messages_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "the upstream stream exceeded the relay's response limit",
            );
        };
        remaining = left;
        let frames = match framer.feed(&chunk) {
            Ok(frames) => frames,
            Err(message) => {
                return messages_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &safe_message(&message, secrets),
                );
            }
        };
        for frame in frames {
            let event = frame.event.as_deref().unwrap_or("");
            match converter.push(event, &frame.data) {
                Ok(events) => record_error_detail(&events, &mut error_detail),
                Err(message) => {
                    return messages_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        &safe_message(&message, secrets),
                    );
                }
            }
            if converter.is_terminal() {
                break;
            }
        }
        if converter.is_terminal() {
            break;
        }
    }

    if converter.is_terminal() {
        upstream::recycle(stream);
    } else {
        if let Err(message) = framer.finish() {
            return messages_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &safe_message(&message, secrets),
            );
        }
        match converter.finish() {
            Ok(events) => record_error_detail(&events, &mut error_detail),
            Err(message) => {
                return messages_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &safe_message(&message, secrets),
                );
            }
        }
    }

    match converter.message() {
        Some(message) => {
            let encoded = redact(&message.to_string(), secrets).into_owned();
            match serde_json::from_str::<Value>(&encoded) {
                Ok(value) => crate::json_response(StatusCode::OK, value),
                Err(_) => messages_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "the relay could not encode the translated message",
                ),
            }
        }
        None => {
            let detail = error_detail
                .as_deref()
                .map(|message| format!(": {}", safe_message(message, secrets)))
                .unwrap_or_default();
            messages_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("the upstream stream ended without a complete message{detail}"),
            )
        }
    }
}

/// Remember the most specific error an Anthropic event carries, so a failed
/// non-streaming response can report it instead of a generic message.
fn record_error_detail(events: &[Value], detail: &mut Option<String>) {
    for event in events {
        let message = event
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str);
        if let Some(message) = message {
            *detail = Some(message.to_owned());
        }
    }
}

/// Incrementally convert Responses SSE into Messages SSE.
fn stream_messages(
    upstream: ByteStream,
    requested_model: &str,
    secrets: Arc<Vec<String>>,
    idle_interval: Duration,
) -> Response<crate::RelayBody> {
    let converter = MessagesStream::new(requested_model);
    let state = MessagesState {
        upstream,
        framer: SseFramer::new(MAX_EVENT_BYTES),
        converter,
        secrets,
        queue: VecDeque::new(),
        remaining_bytes: MAX_MESSAGES_STREAM_BYTES,
        done: false,
        terminal_sent: false,
        failed: false,
    };
    let body = stream::unfold(state, move |mut state| async move {
        let deadline = tokio::time::Instant::now() + idle_interval;
        loop {
            if let Some(chunk) = state.queue.pop_front() {
                return Some((Ok::<_, RelayError>(Frame::data(chunk)), state));
            }
            if state.done {
                return None;
            }
            // A ping does not advance the converter or affect the upstream read deadline.
            if tokio::time::timeout_at(deadline, state.pump())
                .await
                .is_err()
            {
                let ping = Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
                return Some((Ok(Frame::data(ping)), state));
            }
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

struct MessagesState {
    upstream: ByteStream,
    framer: SseFramer,
    converter: MessagesStream,
    secrets: Arc<Vec<String>>,
    queue: VecDeque<Bytes>,
    remaining_bytes: usize,
    done: bool,
    terminal_sent: bool,
    failed: bool,
}

impl MessagesState {
    /// Pull upstream until a downstream frame is queued or the stream ends.
    async fn pump(&mut self) {
        if self.terminal_sent || self.failed {
            self.done = true;
            return;
        }
        match self.upstream.next().await {
            None => self.finish_upstream(),
            Some(Err(_)) => self.fail("the upstream stream ended with a transport error"),
            Some(Ok(chunk)) => {
                let Some(remaining) = self.remaining_bytes.checked_sub(chunk.len()) else {
                    self.fail("the upstream stream exceeded the relay's response limit");
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
                    match self.converter.push(event, &frame.data) {
                        Ok(events) => {
                            self.emit(events);
                            if self.converter.is_terminal() {
                                self.terminal_sent = true;
                                self.done = true;
                                let upstream: ByteStream = Box::pin(stream::empty());
                                upstream::recycle(std::mem::replace(&mut self.upstream, upstream));
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
        match self.converter.finish() {
            Ok(events) => {
                self.emit(events);
                if self.converter.is_terminal() {
                    self.terminal_sent = true;
                } else {
                    self.fail("the upstream stream ended without a terminal message");
                }
            }
            Err(message) => self.fail(&message),
        }
        self.done = true;
    }

    fn emit(&mut self, events: Vec<Value>) {
        for event in events {
            self.enqueue(event);
        }
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

    /// Emit one terminal Messages `error` event and stop. The message may embed
    /// upstream text, so it is redacted before serialization.
    fn fail(&mut self, message: &str) {
        if self.terminal_sent || self.failed {
            return;
        }
        self.failed = true;
        self.done = true;
        let message = truncate(&redact(message, &self.secrets));
        self.enqueue(json!({
            "type": "error",
            "error": {"type": "api_error", "message": message},
        }));
    }
}

/// Request body for a native Anthropic upstream: rewrite the model ID and
/// normalize version-specific sampling fields. Preserve thinking blocks,
/// signatures, tools, safeguards, and cache markers.
fn prepare_native_body(
    mut payload: Value,
    vendor: &Vendor,
    upstream_model: &str,
    version: &str,
) -> Value {
    payload["model"] = json!(upstream_model);
    if payload.get("max_tokens").is_none_or(Value::is_null) {
        let tokens = vendor
            .max_output_tokens
            .unwrap_or(upstream::DEFAULT_MAX_OUTPUT_TOKENS);
        payload["max_tokens"] = json!(tokens);
    }
    if let Some(object) = payload.as_object_mut()
        && AnthropicOptions::version_rejects_sampling(version)
    {
        upstream::strip_anthropic_sampling(object);
    }
    payload
}

/// Pass API capability headers through, while keeping authentication and
/// workspace selection controlled by the gateway.
fn native_headers(
    vendor: &Vendor,
    key: &SecretKey,
    incoming: Option<&HeaderMap>,
    wants_stream: bool,
) -> Result<HeaderMap, RelayError> {
    let mut headers = vendor.inference_headers(key, incoming)?;
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if wants_stream {
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    }
    if let Some(incoming) = incoming {
        let incoming = crate::forward_headers(incoming.clone());
        if !vendor.anthropic.version_pinned
            && let Some(version) = incoming.get(ANTHROPIC_VERSION)
            && incoming.get_all(ANTHROPIC_VERSION).iter().count() == 1
            && version.to_str().is_ok_and(valid_api_version)
        {
            headers.insert(ANTHROPIC_VERSION, version.clone());
        }
        for (name, value) in &incoming {
            let name_text = name.as_str();
            if name_text == ANTHROPIC_VERSION.as_str() || !name_text.starts_with("anthropic-") {
                continue;
            }
            if name == ANTHROPIC_BETA {
                if valid_beta(value.to_str().ok()) {
                    headers.append(name, value.clone());
                }
            } else if !name_text.split('-').any(|part| {
                matches!(
                    part,
                    "auth"
                        | "authorization"
                        | "cookie"
                        | "credential"
                        | "credentials"
                        | "key"
                        | "secret"
                        | "token"
                        | "workspace"
                )
            }) && value.to_str().is_ok_and(|text| text.len() <= 4096)
            {
                headers.append(name, value.clone());
            }
        }
    }
    Ok(headers)
}

/// Accept a conservative `anthropic-beta` token list, never an arbitrary
/// header value.
fn valid_beta(beta: Option<&str>) -> bool {
    beta.is_some_and(|beta| {
        beta.len() <= MAX_BETA_CHARS
            && !beta.trim().is_empty()
            && beta.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b',' | b' ')
            })
    })
}

/// Turn a buffered relay body into a byte stream so cancellation of the
/// downstream response drops the upstream stream with it.
fn relay_body_stream(
    body: crate::RelayBody,
) -> impl Stream<Item = Result<Bytes, RelayError>> + Send {
    stream::unfold(body, |mut body| async move {
        loop {
            let frame = body.frame().await?;
            match frame {
                Ok(frame) => {
                    if let Ok(data) = frame.into_data() {
                        return Some((Ok(data), body));
                    }
                }
                Err(error) => return Some((Err(error), body)),
            }
        }
    })
}

/// Preserve bounded native Messages errors that clients use to recognize
/// conditions such as a full context window. Summarize other upstream errors.
async fn messages_http_failure(
    response: reqwest::Response,
    secrets: &[String],
    native: bool,
) -> Response<crate::RelayBody> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok());
    let metadata = native.then(|| native_response_metadata(response.headers(), secrets));
    let bytes = crate::read_upstream(response, MAX_UPSTREAM_ERROR_BYTES)
        .await
        .ok();
    let body = bytes
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
    let mut downstream = if native
        && let Some(body) = &body
        && body["type"] == "error"
        && body["error"]["type"].is_string()
        && body["error"]["message"].is_string()
        && !contains_secret(body, secrets)
    {
        // Preserve the original envelope when raw and decoded representations
        // can both be screened. Reserializing escaped JSON also drops duplicate
        // fields that a Value-level scan cannot see.
        match bytes.filter(|bytes| {
            std::str::from_utf8(bytes).is_ok_and(|text| {
                !text.contains('\\')
                    && !secrets
                        .iter()
                        .any(|key| !key.is_empty() && text.contains(key))
            })
        }) {
            Some(bytes) => crate::bytes_response(status, bytes, "application/json"),
            None => crate::json_response(status, body.clone()),
        }
    } else {
        let message = upstream_error(status.as_u16(), body.as_ref(), secrets);
        messages_error(status, kind_for_status(status), &message)
    };
    if let Some(metadata) = metadata {
        downstream.headers_mut().extend(metadata);
    } else {
        copy_retry_after(&mut downstream, status, retry_after, secrets);
    }
    downstream
}

/// Metadata Claude Code uses for retry and usage-limit decisions, filtered
/// before attaching it to a buffered native Messages response.
fn native_response_metadata(headers: &HeaderMap, secrets: &[String]) -> HeaderMap {
    let mut metadata = HeaderMap::new();
    for (name, value) in headers {
        let Ok(text) = value.to_str() else {
            continue;
        };
        let supported = match name.as_str() {
            "retry-after" => {
                !text.is_empty()
                    && text.bytes().all(|byte| byte.is_ascii_digit())
                    && text.parse::<u32>().is_ok()
            }
            "x-should-retry" => matches!(text, "true" | "false"),
            "request-id" => {
                !text.is_empty()
                    && text.len() <= 256
                    && text.bytes().all(|byte| byte.is_ascii_graphic())
            }
            name => name.starts_with("anthropic-ratelimit-unified-") && text.len() <= 1024,
        };
        if supported
            && !secrets
                .iter()
                .any(|key| !key.is_empty() && (text.contains(key) || name.as_str().contains(key)))
        {
            metadata.append(name, value.clone());
        }
    }
    metadata
}

fn contains_secret(value: &Value, secrets: &[String]) -> bool {
    let matches = |text: &str| {
        secrets
            .iter()
            .any(|key| !key.is_empty() && text.contains(key))
    };
    match value {
        Value::String(text) => matches(text),
        Value::Array(items) => items.iter().any(|item| contains_secret(item, secrets)),
        Value::Object(fields) => fields
            .iter()
            .any(|(name, item)| matches(name) || contains_secret(item, secrets)),
        Value::Number(number) => matches(&number.to_string()),
        _ => false,
    }
}

/// A translated-path failure already carries a safe Responses error body.
async fn messages_from_relay_error(
    response: Response<crate::RelayBody>,
    secrets: &[String],
) -> Response<crate::RelayBody> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok());
    let body = read_relay_error(response).await;
    let message = body
        .as_ref()
        .and_then(extract_error_message)
        .map(|message| truncate(&redact(message, secrets)))
        .unwrap_or_else(|| format!("provider returned HTTP {}", status.as_u16()));
    let mut downstream = messages_error(status, kind_for_status(status), &message);
    copy_retry_after(&mut downstream, status, retry_after, secrets);
    downstream
}

/// Messages error body: `{"type":"error","error":{"type":...,"message":...}}`.
fn messages_error(status: StatusCode, kind: &str, message: &str) -> Response<crate::RelayBody> {
    crate::json_response(
        status,
        json!({
            "type": "error",
            "error": {"type": kind, "message": message},
        }),
    )
}

fn kind_for_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        })
}

async fn read_upstream_error(response: reqwest::Response) -> Option<Value> {
    let bytes = crate::read_upstream(response, MAX_UPSTREAM_ERROR_BYTES)
        .await
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn read_relay_error(response: Response<crate::RelayBody>) -> Option<Value> {
    let mut body = response.into_body();
    let mut buffer = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.ok()?;
        if let Ok(data) = frame.into_data() {
            if buffer.len() + data.len() > MAX_UPSTREAM_ERROR_BYTES {
                return None;
            }
            buffer.extend_from_slice(&data);
        }
    }
    serde_json::from_slice(&buffer).ok()
}

/// Read a bounded error body only to summarize it; never relay the body itself.
async fn read_upstream_detail(response: reqwest::Response, secrets: &[String]) -> String {
    read_upstream_error(response)
        .await
        .and_then(|body| {
            body.get("error")
                .and_then(|error| error.get("message"))
                .or_else(|| body.get("message"))
                .and_then(Value::as_str)
                .map(|message| format!(": {}", truncate(&redact(message, secrets))))
        })
        .unwrap_or_default()
}

fn upstream_error(status: u16, body: Option<&Value>, secrets: &[String]) -> String {
    let detail = body
        .and_then(extract_error_message)
        .map(|message| format!(": {}", truncate(&redact(message, secrets))))
        .unwrap_or_default();
    format!("provider returned HTTP {status}{detail}")
}

fn extract_error_message(value: &Value) -> Option<&str> {
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .or_else(|| value.get("message"))
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)?;
    (!message.trim().is_empty()).then_some(message)
}

/// A translator-supplied message can embed upstream text, so it is both
/// truncated and redacted before it reaches the client.
fn safe_message(message: &str, secrets: &[String]) -> String {
    truncate(&redact(message, secrets))
}

fn redact<'a>(text: &'a str, secrets: &[String]) -> Cow<'a, str> {
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

/// Copy only a well-formed numeric `Retry-After` on `429`, and never one that
/// would echo a credential.
fn copy_retry_after(
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
    use std::convert::Infallible;
    use std::sync::Mutex;

    use crate::provider::{AnthropicOptions, ChatOptions};
    use http_body_util::{BodyExt, Full};
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use reqwest::Url;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    #[derive(Clone, Debug)]
    struct Hit {
        path: String,
        headers: HeaderMap,
        body: Value,
    }

    /// Loopback vendor replaying one canned response and recording each request.
    struct MockUpstream {
        base: String,
        hits: Arc<Mutex<Vec<Hit>>>,
        task: JoinHandle<()>,
    }

    impl MockUpstream {
        fn spawn(status: u16, content_type: &'static str, reply: &'static str) -> Self {
            Self::spawn_with_headers(status, content_type, reply, &[])
        }

        fn spawn_with_headers(
            status: u16,
            content_type: &'static str,
            reply: &'static str,
            extra_headers: &'static [(&str, &str)],
        ) -> Self {
            let hits = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&hits);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                let listener = TcpListener::from_std(listener).unwrap();
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let recorded = Arc::clone(&recorded);
                    tokio::spawn(async move {
                        let service = service_fn(move |request: hyper::Request<Incoming>| {
                            let recorded = Arc::clone(&recorded);
                            async move {
                                let path = request.uri().path().to_owned();
                                let headers = request.headers().clone();
                                let bytes = request
                                    .into_body()
                                    .collect()
                                    .await
                                    .expect("request body")
                                    .to_bytes();
                                let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                                recorded.lock().unwrap().push(Hit {
                                    path,
                                    headers,
                                    body,
                                });
                                let mut response =
                                    Response::new(Full::new(Bytes::from_static(reply.as_bytes())));
                                *response.status_mut() = StatusCode::from_u16(status).unwrap();
                                response
                                    .headers_mut()
                                    .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
                                for &(name, value) in extra_headers {
                                    response.headers_mut().append(
                                        HeaderName::from_static(name),
                                        HeaderValue::from_static(value),
                                    );
                                }
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });
            Self { base, hits, task }
        }

        fn only_hit(&self) -> Hit {
            let hits = self.hits.lock().unwrap();
            assert_eq!(hits.len(), 1, "expected one upstream request");
            hits[0].clone()
        }
    }

    impl Drop for MockUpstream {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn vendor(wire: Wire) -> Vendor {
        let url = Url::parse("https://anthropic.example/v1").unwrap();
        Vendor {
            key: "vendor".to_owned(),
            name: "Vendor".to_owned(),
            base_url: url.clone(),
            models_url: url,
            env_key: None,
            wire,
            discovery: false,
            models: Vec::new(),
            max_output_tokens: None,
            anthropic: AnthropicOptions::default(),
            chat: ChatOptions::default(),
        }
    }

    fn context<'a>(
        vendor: &'a Vendor,
        key: &'a SecretKey,
        secrets: &Arc<Vec<String>>,
    ) -> Translation<'a> {
        Translation {
            vendor,
            key,
            incoming: None,
            upstream_model: "upstream-model",
            requested_model: "vendor/upstream-model",
            secrets: Arc::clone(secrets),
        }
    }

    async fn response_json(response: Response<crate::RelayBody>) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn non_object_bodies_are_rejected_with_a_messages_error() {
        let vendor = vendor(Wire::AnthropicMessages);
        let key = SecretKey::new("sk-test-key".to_owned());
        let secrets = Arc::new(Vec::new());
        let response = infer(
            &Client::new(),
            context(&vendor, &key, &secrets),
            json!([1, 2, 3]),
            false,
        )
        .await;
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn native_body_rewrites_the_model_and_preserves_thinking() {
        let vendor = vendor(Wire::AnthropicMessages);
        let payload = json!({
            "model": "vendor/upstream-model",
            "max_tokens": 32000,
            "stream": true,
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "c2lnbmF0dXJl"},
                    {"type": "text", "text": "answer"}
                ]
            }],
            "tools": [{"name": "read", "input_schema": {"type": "object"}}]
        });
        let body =
            prepare_native_body(payload, &vendor, "real-model-id", &vendor.anthropic.version);
        assert_eq!(body["model"], "real-model-id");
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["messages"][0]["content"][0]["signature"],
            "c2lnbmF0dXJl"
        );
        assert_eq!(body["messages"][0]["content"][0]["thinking"], "hmm");
        assert_eq!(body["tools"][0]["name"], "read");
    }

    #[test]
    fn native_body_supplies_missing_max_tokens_from_configuration() {
        let mut vendor = vendor(Wire::AnthropicMessages);
        vendor.max_output_tokens = Some(1234);
        let body = prepare_native_body(
            json!({"messages": []}),
            &vendor,
            "m",
            &vendor.anthropic.version,
        );
        assert_eq!(body["max_tokens"], 1234);

        let body = prepare_native_body(
            json!({"max_tokens": 99}),
            &vendor,
            "m",
            &vendor.anthropic.version,
        );
        assert_eq!(body["max_tokens"], 99);
    }

    #[test]
    fn native_body_removes_sampling_only_on_the_current_api() {
        let mut vendor = vendor(Wire::AnthropicMessages);
        let payload = json!({"temperature": 1, "top_p": 1, "top_k": 0, "messages": []});
        let legacy = prepare_native_body(payload.clone(), &vendor, "m", &vendor.anthropic.version);
        vendor.anthropic.version = "2026-10-21".to_owned();
        let current = prepare_native_body(payload.clone(), &vendor, "m", &vendor.anthropic.version);
        for field in ["temperature", "top_p", "top_k"] {
            assert_eq!(legacy[field], payload[field]);
            assert!(current.get(field).is_none());
        }
    }

    #[test]
    fn native_headers_forward_beta_values_and_drop_caller_authentication() {
        let mut vendor = vendor(Wire::AnthropicMessages);
        let key = SecretKey::new("sk-vendor-key".to_owned());
        let mut incoming = HeaderMap::new();
        incoming.append(
            ANTHROPIC_BETA,
            HeaderValue::from_static("interleaved-thinking-2025-05-14"),
        );
        incoming.append(
            ANTHROPIC_BETA,
            HeaderValue::from_static("dangerous-tool-use-2026-09-03"),
        );
        incoming.append(ANTHROPIC_BETA, HeaderValue::from_static("bad;value"));
        incoming.insert(
            "authorization",
            HeaderValue::from_static("Bearer relay-fixture"),
        );
        incoming.insert("x-api-key", HeaderValue::from_static("relay-fixture"));
        let headers = native_headers(&vendor, &key, Some(&incoming), true).unwrap();
        assert_eq!(headers["x-api-key"], "sk-vendor-key");
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert_eq!(
            headers.get_all(&ANTHROPIC_BETA).iter().collect::<Vec<_>>(),
            [
                "interleaved-thinking-2025-05-14",
                "dangerous-tool-use-2026-09-03"
            ]
        );
        assert!(!valid_beta(Some("  ")));
        assert!(!valid_beta(Some("bad\r\nX-Injected: 1")));
        assert!(!valid_beta(Some(&"a".repeat(MAX_BETA_CHARS + 1))));
        assert_eq!(headers[ACCEPT], "text/event-stream");
        assert!(!headers.contains_key("authorization"));

        incoming.insert(ANTHROPIC_VERSION, HeaderValue::from_static("2026-10-21"));
        let headers = native_headers(&vendor, &key, Some(&incoming), true).unwrap();
        assert_eq!(headers[ANTHROPIC_VERSION], "2026-10-21");
        vendor.anthropic.version_pinned = true;
        let headers = native_headers(&vendor, &key, Some(&incoming), true).unwrap();
        assert_eq!(headers[ANTHROPIC_VERSION], "2023-06-01");
    }

    #[test]
    fn errors_use_the_messages_envelope_and_map_statuses() {
        let response = messages_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", "slow");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers()[CONTENT_TYPE],
            HeaderValue::from_static("application/json")
        );

        assert_eq!(
            kind_for_status(StatusCode::BAD_REQUEST),
            "invalid_request_error"
        );
        assert_eq!(
            kind_for_status(StatusCode::UNAUTHORIZED),
            "authentication_error"
        );
        assert_eq!(kind_for_status(StatusCode::FORBIDDEN), "permission_error");
        assert_eq!(kind_for_status(StatusCode::NOT_FOUND), "not_found_error");
        assert_eq!(
            kind_for_status(StatusCode::PAYLOAD_TOO_LARGE),
            "request_too_large"
        );
        assert_eq!(
            kind_for_status(StatusCode::TOO_MANY_REQUESTS),
            "rate_limit_error"
        );
        assert_eq!(
            kind_for_status(StatusCode::from_u16(529).unwrap()),
            "overloaded_error"
        );
        assert_eq!(
            kind_for_status(StatusCode::INTERNAL_SERVER_ERROR),
            "api_error"
        );
    }

    #[test]
    fn upstream_error_text_is_redacted_and_truncated() {
        let body = json!({"error": {"message": "bad key sk-live-12345"}});
        let message = upstream_error(401, Some(&body), &["sk-live-12345".to_owned()]);
        assert_eq!(message, "provider returned HTTP 401: bad key [redacted]");
        assert!(!message.contains("sk-live"));

        let long = "x".repeat(MAX_ERROR_CHARS + 10);
        assert_eq!(truncate(&long).chars().count(), MAX_ERROR_CHARS + 1);
        assert_eq!(safe_message(&long, &[]), truncate(&long));
    }

    #[test]
    fn retry_after_is_only_copied_on_rate_limits() {
        let mut response =
            messages_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", "slow");
        copy_retry_after(&mut response, StatusCode::TOO_MANY_REQUESTS, Some(7), &[]);
        assert_eq!(response.headers()[RETRY_AFTER], "7");

        let mut response = messages_error(StatusCode::BAD_GATEWAY, "api_error", "down");
        copy_retry_after(&mut response, StatusCode::BAD_GATEWAY, Some(7), &[]);
        assert!(!response.headers().contains_key(RETRY_AFTER));

        let mut response =
            messages_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", "slow");
        copy_retry_after(
            &mut response,
            StatusCode::TOO_MANY_REQUESTS,
            Some(1234),
            &["1234".to_owned()],
        );
        assert!(!response.headers().contains_key(RETRY_AFTER));
    }

    fn responses_event(value: Value) -> String {
        let kind = value["type"].as_str().unwrap().to_owned();
        format!("event: {kind}\ndata: {value}\n\n")
    }

    fn responses_stream() -> String {
        let created = json!({
            "type": "response.created",
            "response": {
                "id": "resp_abc",
                "object": "response",
                "model": "upstream-model",
                "status": "in_progress",
                "output": []
            }
        });
        let item = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"id": "msg_0", "type": "message", "status": "in_progress", "role": "assistant", "content": []}
        });
        let part = json!({
            "type": "response.content_part.added",
            "item_id": "msg_0",
            "output_index": 0,
            "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []}
        });
        let delta = json!({
            "type": "response.output_text.delta",
            "item_id": "msg_0",
            "output_index": 0,
            "content_index": 0,
            "delta": "Hello"
        });
        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_abc",
                "object": "response",
                "model": "upstream-model",
                "status": "completed",
                "output": [{
                    "id": "msg_0",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Hello", "annotations": []}]
                }],
                "usage": {
                    "input_tokens": 12,
                    "input_tokens_details": {"cached_tokens": 2},
                    "output_tokens": 3,
                    "total_tokens": 15
                }
            }
        });
        [created, item, part, delta, completed]
            .into_iter()
            .map(responses_event)
            .collect()
    }

    fn fake_stream(input: &str) -> ByteStream {
        let bytes = Bytes::copy_from_slice(input.as_bytes());
        let chunks = bytes
            .chunks(7)
            .map(|chunk| Ok::<_, RelayError>(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();
        Box::pin(stream::iter(chunks))
    }

    async fn collect_sse(response: Response<crate::RelayBody>) -> Vec<Value> {
        let mut body = response.into_body();
        let mut framer = SseFramer::new(MAX_EVENT_BYTES);
        let mut events = Vec::new();
        while let Some(frame) = body.frame().await {
            let data = frame.unwrap().into_data().unwrap();
            for framed in framer.feed(&data).unwrap() {
                events.push(serde_json::from_str(&framed.data).unwrap());
            }
        }
        framer.finish().unwrap();
        events
    }

    #[tokio::test]
    async fn streaming_translation_emits_messages_events_incrementally() {
        let response = stream_messages(
            fake_stream(&responses_stream()),
            "vendor/upstream-model",
            Arc::new(Vec::new()),
            KEEPALIVE_INTERVAL,
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");

        let events = collect_sse(response).await;
        let kinds = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[0]["message"]["model"], "vendor/upstream-model");
        assert_eq!(events[2]["delta"]["text"], "Hello");
        assert_eq!(events[4]["delta"]["stop_reason"], "end_turn");
    }

    /// A complete Responses stream followed by a tail that stays open until
    /// `release` fires and then records that it was read to its end.
    fn stream_with_held_tail() -> (
        ByteStream,
        tokio::sync::oneshot::Sender<()>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let drained = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&drained);
        let tail = stream::once(async move {
            let _ = held.await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, RelayError>(Bytes::new())
        });
        let body = stream::iter([Ok::<_, RelayError>(Bytes::from(responses_stream()))]).chain(tail);
        (Box::pin(body), release, drained)
    }

    async fn wait_until_drained(drained: &std::sync::atomic::AtomicBool) -> bool {
        for _ in 0..100 {
            if drained.load(std::sync::atomic::Ordering::SeqCst) {
                return true;
            }
            tokio::task::yield_now().await;
        }
        false
    }

    #[tokio::test]
    async fn a_finished_messages_stream_ends_at_once_then_drains_its_upstream() {
        let (upstream, release, drained) = stream_with_held_tail();
        let response = stream_messages(
            upstream,
            "vendor/upstream-model",
            Arc::new(Vec::new()),
            KEEPALIVE_INTERVAL,
        );
        // The upstream body is still open: the client stream must not wait.
        let events = tokio::time::timeout(Duration::from_secs(2), collect_sse(response))
            .await
            .expect("the Messages stream must end at the terminal event");
        assert_eq!(events.last().unwrap()["type"], "message_stop");
        assert!(!drained.load(std::sync::atomic::Ordering::SeqCst));
        let _ = release.send(());
        assert!(
            wait_until_drained(&drained).await,
            "the upstream body must be read to its end so its connection is reused"
        );
    }

    #[tokio::test]
    async fn a_collected_message_answers_at_once_then_drains_its_upstream() {
        let (upstream, release, drained) = stream_with_held_tail();
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            collect_message(upstream, "vendor/x", &[]),
        )
        .await
        .expect("the message must be answered at the terminal event");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!drained.load(std::sync::atomic::Ordering::SeqCst));
        let _ = release.send(());
        assert!(
            wait_until_drained(&drained).await,
            "the upstream body must be read to its end so its connection is reused"
        );
    }

    #[tokio::test]
    async fn an_idle_translated_stream_pings_then_finishes_normally() {
        let (send, ready) = tokio::sync::oneshot::channel();
        let upstream = stream::once(async move {
            ready.await.unwrap();
            Ok(Bytes::from(responses_stream()))
        });
        let response = stream_messages(
            Box::pin(upstream),
            "vendor/upstream-model",
            Arc::new(Vec::new()),
            Duration::from_millis(10),
        );
        let mut body = response.into_body();
        let ping = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(ping, "event: ping\ndata: {\"type\":\"ping\"}\n\n");
        send.send(()).unwrap();
        let events = collect_sse(Response::new(body)).await;
        assert_eq!(events[0]["type"], "message_start");
        assert_eq!(events.last().unwrap()["type"], "message_stop");
    }

    #[tokio::test]
    async fn non_streaming_translation_reuses_the_terminal_message() {
        let response = collect_message(fake_stream(&responses_stream()), "vendor/x", &[]).await;
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["type"], "message");
        assert_eq!(body["model"], "vendor/x");
        assert_eq!(body["content"][0]["text"], "Hello");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["usage"]["output_tokens"], 3);
    }

    #[tokio::test]
    async fn truncated_translation_reports_a_messages_error() {
        let created = responses_event(json!({
            "type": "response.created",
            "response": {"id": "resp_abc", "object": "response", "model": "m", "status": "in_progress", "output": []}
        }));
        let response = collect_message(fake_stream(&created), "vendor/x", &[]).await;
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "api_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("without a complete message")
        );
    }

    #[tokio::test]
    async fn upstream_failure_events_are_redacted_into_messages_errors() {
        let created = responses_event(json!({
            "type": "response.created",
            "response": {"id": "resp_abc", "object": "response", "model": "m", "status": "in_progress", "output": []}
        }));
        let failed = responses_event(json!({
            "type": "response.failed",
            "response": {
                "id": "resp_abc",
                "object": "response",
                "status": "failed",
                "error": {"code": "server_error", "message": "upstream exploded with sk-live-12345"}
            }
        }));
        let response = collect_message(
            fake_stream(&format!("{created}{failed}")),
            "vendor/x",
            &["sk-live-12345".to_owned()],
        )
        .await;
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("upstream exploded"));
        assert!(message.contains("[redacted]"));
        assert!(!message.contains("sk-live-12345"));
    }

    fn vendor_at(base: &str, wire: Wire) -> Vendor {
        let url = Url::parse(&format!("{base}/v1")).unwrap();
        Vendor {
            key: "vendor".to_owned(),
            name: "Vendor".to_owned(),
            base_url: url.clone(),
            models_url: url,
            env_key: None,
            wire,
            discovery: false,
            models: Vec::new(),
            max_output_tokens: None,
            anthropic: AnthropicOptions::default(),
            chat: ChatOptions::default(),
        }
    }

    #[tokio::test]
    async fn native_wire_streams_with_configured_credentials_and_beta() {
        const REPLY: &str = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool-fixture\",\"name\":\"read_fixture\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"fixture.txt\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",",
            "\"delta\":{\"safeguard_results\":[{\"tool_use_id\":\"tool-fixture\",\"decision\":\"deny\"}]}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mock = MockUpstream::spawn(200, "text/event-stream", REPLY);
        let vendor = vendor_at(&mock.base, Wire::AnthropicMessages);
        let key = SecretKey::new("sk-vendor-key".to_owned());
        let secrets = Arc::new(vec!["sk-vendor-key".to_owned()]);
        let payload = json!({
            "model": "vendor/upstream-model",
            "max_tokens": 1024,
            "stream": true,
            "safeguards": {"fixture": true},
            "tools": [{
                "name": "read_fixture",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
                "eager_input_streaming": true
            }],
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "reasoning", "signature": "c2ln"},
                    {"type": "text", "text": "answer"}
                ]
            }]
        });
        let mut incoming = HeaderMap::new();
        incoming.insert(
            ANTHROPIC_BETA,
            HeaderValue::from_static(
                "interleaved-thinking-2025-05-14,dangerous-tool-use-2026-09-03",
            ),
        );
        let response = infer(
            &Client::new(),
            Translation {
                vendor: &vendor,
                key: &key,
                incoming: Some(&incoming),
                upstream_model: "real-model",
                requested_model: "vendor/upstream-model",
                secrets: Arc::clone(&secrets),
            },
            payload,
            false,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], REPLY.as_bytes());

        let hit = mock.only_hit();
        assert_eq!(hit.path, "/v1/messages");
        assert_eq!(hit.headers["x-api-key"], "sk-vendor-key");
        assert_eq!(hit.headers["anthropic-version"], "2023-06-01");
        assert_eq!(
            hit.headers["anthropic-beta"],
            "interleaved-thinking-2025-05-14,dangerous-tool-use-2026-09-03"
        );
        assert!(!hit.headers.contains_key("authorization"));
        assert_eq!(hit.body["model"], "real-model");
        assert_eq!(hit.body["safeguards"], json!({"fixture": true}));
        assert_eq!(hit.body["tools"][0]["eager_input_streaming"], true);
        assert_eq!(hit.body["messages"][0]["content"][0]["signature"], "c2ln");
        assert_eq!(hit.body["messages"][0]["content"][1]["text"], "answer");
        assert_eq!(hit.body["stream"], true);
    }

    #[tokio::test]
    async fn native_wire_relays_non_streaming_json_unchanged() {
        const REPLY: &str = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"sig"},{"type":"text","text":"ok"}],"model":"real-model","stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":3}}"#;
        let mock = MockUpstream::spawn_with_headers(
            200,
            "application/json",
            REPLY,
            &[("anthropic-ratelimit-unified-status", "allowed")],
        );
        let vendor = vendor_at(&mock.base, Wire::AnthropicMessages);
        let key = SecretKey::new("sk-vendor-key".to_owned());
        let secrets = Arc::new(vec!["sk-vendor-key".to_owned()]);
        let response = infer(
            &Client::new(),
            Translation {
                vendor: &vendor,
                key: &key,
                incoming: None,
                upstream_model: "real-model",
                requested_model: "vendor/upstream-model",
                secrets,
            },
            json!({
                "model": "vendor/upstream-model",
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            false,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-status"],
            "allowed"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let relayed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(relayed, serde_json::from_str::<Value>(REPLY).unwrap());
        assert_eq!(relayed["content"][0]["signature"], "sig");
        let hit = mock.only_hit();
        assert_eq!(hit.body["model"], "real-model");
        assert_eq!(hit.body["stream"], Value::Null);
        assert!(!hit.headers.contains_key(ANTHROPIC_BETA));
    }

    #[tokio::test]
    async fn native_wire_preserves_the_context_limit_error_envelope() {
        const ERROR: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 200001 tokens > 200000 maximum"},"request_id":"req_fixture"}"#;
        let mock = MockUpstream::spawn_with_headers(
            400,
            "application/json",
            ERROR,
            &[
                ("x-should-retry", "false"),
                ("retry-after", "17"),
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-fixture", "sk-vendor-key"),
                ("x-fixture", "sk-vendor-key"),
            ],
        );
        let vendor = vendor_at(&mock.base, Wire::AnthropicMessages);
        let key = SecretKey::new("sk-vendor-key".to_owned());
        let response = infer(
            &Client::new(),
            Translation {
                vendor: &vendor,
                key: &key,
                incoming: None,
                upstream_model: "real-model",
                requested_model: "vendor/upstream-model",
                secrets: Arc::new(vec!["sk-vendor-key".to_owned()]),
            },
            json!({"model": "vendor/upstream-model", "max_tokens": 10, "messages": []}),
            false,
        )
        .await;
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert_eq!(response.headers()[RETRY_AFTER], "17");
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-status"],
            "rejected"
        );
        assert!(
            !response
                .headers()
                .contains_key("anthropic-ratelimit-unified-fixture")
        );
        assert!(!response.headers().contains_key("x-fixture"));
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, serde_json::from_str::<Value>(ERROR).unwrap());
    }

    #[tokio::test]
    async fn native_wire_failures_are_redacted_messages_errors() {
        let mock = MockUpstream::spawn(
            401,
            "application/json",
            r#"{"type":"error","error":{"type":"authentication_error","message":"bad key sk-vendor-key"}}"#,
        );
        let vendor = vendor_at(&mock.base, Wire::AnthropicMessages);
        let key = SecretKey::new("sk-vendor-key".to_owned());
        let secrets = Arc::new(vec!["sk-vendor-key".to_owned()]);
        let response = infer(
            &Client::new(),
            Translation {
                vendor: &vendor,
                key: &key,
                incoming: None,
                upstream_model: "real-model",
                requested_model: "vendor/upstream-model",
                secrets,
            },
            json!({"model": "vendor/upstream-model", "max_tokens": 10, "messages": []}),
            false,
        )
        .await;
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "authentication_error");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("[redacted]"));
        assert!(!message.contains("sk-vendor-key"));
    }

    #[tokio::test]
    async fn native_errors_do_not_relay_credentials_hidden_in_duplicate_json_fields() {
        let mock = MockUpstream::spawn(
            400,
            "application/json",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"too long"},"detail":"\u0073k-vendor-key","detail":"safe"}"#,
        );
        let vendor = vendor_at(&mock.base, Wire::AnthropicMessages);
        let key = SecretKey::new("sk-vendor-key".to_owned());
        let secrets = Arc::new(vec!["sk-vendor-key".to_owned()]);
        let response = infer(
            &Client::new(),
            context(&vendor, &key, &secrets),
            json!({"model":"vendor/upstream-model","messages":[]}),
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&body).contains("vendor-key"));
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["detail"],
            "safe"
        );
    }

    #[tokio::test]
    async fn native_wire_does_not_relay_redirects_even_with_sse_content_type() {
        let mock = MockUpstream::spawn_with_headers(
            302,
            "text/event-stream",
            "",
            &[("location", "https://untrusted.example/")],
        );
        let vendor = vendor_at(&mock.base, Wire::AnthropicMessages);
        let key = SecretKey::new("sk-vendor-key".to_owned());
        let secrets = Arc::new(vec!["sk-vendor-key".to_owned()]);
        let client = crate::upstream_client().unwrap();
        let response = infer(
            &client,
            context(&vendor, &key, &secrets),
            json!({"model": "vendor/upstream-model", "stream": true, "messages": []}),
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(!response.headers().contains_key("location"));
        mock.only_hit();
    }

    #[tokio::test]
    async fn relay_body_stream_stops_at_the_end() {
        let body = crate::json_response(StatusCode::OK, json!({"ok": true}));
        let mut stream = Box::pin(relay_body_stream(body.into_body()));
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, br#"{"ok":true}"#);
    }
}
