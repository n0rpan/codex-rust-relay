//! Chat Completions adapter: one wire protocol in, two shapes out.
//!
//! * [`translate_request`] turns the Responses API request body that Codex sends to
//!   `POST /v1/responses` into a Chat Completions request body for an upstream
//!   `POST /v1/chat/completions` endpoint.
//! * [`ResponseTranslator`] consumes the upstream's already-framed SSE events
//!   (`event` name plus `data` payload, framing done by the caller) and returns
//!   Responses API stream events, so Codex keeps speaking one protocol.
//!
//! Both halves are pure and offline: no I/O, no clocks, no randomness, and no new
//! dependencies. The caller owns sockets, the upstream bearer token, response IDs
//! and timestamps.
//!
//! ## Baseline
//!
//! Only documented OpenAI wire formats are produced and consumed. Where
//! compatible servers diverge, [`AdapterOptions`] carries that single narrow seam
//! and the default stays on the widest baseline. Vendor behaviour is never sniffed
//! from the request body.
//!
//! ## Mapping
//!
//! Requests:
//!
//! | Responses | Chat Completions |
//! | --- | --- |
//! | `instructions` | leading `system` message |
//! | `input` string | single `user` message |
//! | `input[]` `message` | `system`/`user`/`assistant`/`developer` message |
//! | `input[]` `function_call` | `tool_calls[]` entry on the preceding assistant message |
//! | `input[]` `function_call_output` | `tool` message keyed by `tool_call_id` |
//! | `input[]` `reasoning` | dropped: not representable, and unusable without upstream state |
//! | `tools[]` `function` | `tools[]` `{"type":"function","function":{...}}` |
//! | `tool_choice` string or `{"type":"function","name"}` | `"auto"`/`"none"`/`"required"` or `{"type":"function","function":{"name"}}` |
//! | `max_output_tokens` | `max_tokens` (see [`MaxTokensField`]) |
//! | `text.format` | `response_format` |
//! | `reasoning.effort` | dropped (see [`ReasoningEffort`]) |
//! | `temperature`, `top_p`, `seed`, `user`, `metadata`, `service_tier` | forwarded unchanged |
//! | `model` | replaced by the caller's `upstream_model` |
//!
//! Streams:
//!
//! | Chat Completions chunk | Responses events |
//! | --- | --- |
//! | first chunk | `response.created`, `response.in_progress` |
//! | `delta.content` | `message` item, `response.content_part.added`, `response.output_text.delta` |
//! | `delta.refusal` | `message` item with a `refusal` content part |
//! | `delta.tool_calls[]` | `function_call` item, `response.function_call_arguments.delta` |
//! | `delta.function_call` (legacy, no ID) | same as `tool_calls`, with a generated stable ID |
//! | `finish_reason` | closes open items, then `response.completed` / `response.incomplete` on `[DONE]` |
//! | `usage` | `response.usage` |
//!
//! ## Dropped without translation
//!
//! Fields with no Chat Completions equivalent are dropped, never invented:
//! `include`, `prompt_cache_key`, `reasoning.summary`, `text.verbosity`,
//! `truncation`, `store: false`, `background: false`, and Codex's `client_metadata`.
//!
//! ## Deliberately unhandled
//!
//! Requests that need upstream server-side state (`previous_response_id`,
//! `conversation`, `store: true`, `background: true`), hosted tools (`web_search`,
//! `file_search`, `computer_use`, `image_generation`, `code_interpreter`, `mcp`,
//! `local_shell`, freeform `custom` tools), item types other than message /
//! function call / function call output / reasoning, and non-streaming requests
//! are rejected with an error rather than dropped, because dropping them would
//! silently change what the model may do. The relay decides how to surface the
//! error; it must not retry with a narrower request on its own.

use serde_json::{Map, Value, json};

/// Build the `item_id` reported for a generated assistant message.
///
/// Codex keys transcript rows by `(turn, item_id)` and appends a row only the
/// first time it sees a key, so an id that repeats within a turn makes every
/// later reply rewrite the first row in place. A name built only from a
/// per-stream `output_index` repeats on every response, because the index
/// restarts at 0. Prefixing the response id keeps ids unique across a thread
/// while staying reproducible.
fn message_item_id(response_id: &str, index: usize) -> String {
    format!("msg_{response_id}_{index}")
}

/// Which Chat Completions field carries the Responses `max_output_tokens` limit.
///
/// `MaxTokens` is deprecated upstream but is what essentially every compatible
/// server accepts; `MaxCompletionTokens` is the field the official API now
/// documents. The adapter never sends both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MaxTokensField {
    /// Send `max_tokens`.
    #[default]
    MaxTokens,
    /// Send `max_completion_tokens`.
    MaxCompletionTokens,
}

impl MaxTokensField {
    fn name(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// How the Responses `reasoning.effort` hint is handled.
///
/// Codex always sends an effort, but non-reasoning upstreams reject
/// `reasoning_effort`, so the default drops the hint and only an explicit opt-in
/// forwards it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReasoningEffort {
    /// Drop `reasoning.effort`.
    #[default]
    Drop,
    /// Forward `reasoning.effort` as the official Chat Completions `reasoning_effort`.
    Forward,
}

/// Where Responses `developer` messages are sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeveloperRole {
    /// Send them as `system` messages, which every compatible server accepts.
    #[default]
    System,
    /// Keep the official Chat Completions `developer` role.
    Developer,
}

/// Narrow, documented seams for compatible servers that diverge from the baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterOptions {
    /// Field carrying `max_output_tokens`.
    pub max_tokens_field: MaxTokensField,
    /// Whether `reasoning.effort` is forwarded.
    pub reasoning_effort: ReasoningEffort,
    /// Where `developer` messages go.
    pub developer_role: DeveloperRole,
    /// Whether to ask for a final usage chunk with `stream_options.include_usage`.
    ///
    /// On by default: without it the upstream reports no token counts, and
    /// Responses clients treat usage as required context accounting.
    pub include_stream_usage: bool,
}

impl Default for AdapterOptions {
    fn default() -> Self {
        Self {
            max_tokens_field: MaxTokensField::default(),
            reasoning_effort: ReasoningEffort::default(),
            developer_role: DeveloperRole::default(),
            include_stream_usage: true,
        }
    }
}

/// Responses scalar fields forwarded to Chat Completions without renaming.
const FORWARDED_FIELDS: [&str; 6] = [
    "metadata",
    "seed",
    "service_tier",
    "temperature",
    "top_p",
    "user",
];

/// Translate a Responses API request body into a Chat Completions request body.
///
/// `upstream_model` always wins over the request's `model`, because Codex sends the
/// slug it showed in `/model` and the relay knows the upstream's real name.
#[allow(dead_code)] // default-options entry point used by the adapter contract test
pub fn translate_request(input: &Value, upstream_model: &str) -> Result<Value, String> {
    translate_request_with(input, upstream_model, &AdapterOptions::default())
}

/// [`translate_request`] with explicit vendor seams.
pub fn translate_request_with(
    input: &Value,
    upstream_model: &str,
    options: &AdapterOptions,
) -> Result<Value, String> {
    let request = input
        .as_object()
        .ok_or("the response request body must be a JSON object")?;
    if upstream_model.trim().is_empty() {
        return Err("the upstream model id must not be empty".into());
    }
    reject_server_state(request)?;
    reject_non_streaming(request)?;

    let mut messages = Vec::new();
    if let Some(instructions) = request.get("instructions") {
        match instructions {
            Value::Null => {}
            Value::String(text) if text.is_empty() => {}
            Value::String(text) => messages.push(json!({"role": "system", "content": text})),
            _ => return Err("instructions must be a string".into()),
        }
    }
    match request.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => push_input_items(items, options, &mut messages)?,
        Some(Value::Null) | None => return Err("the response request is missing input".into()),
        Some(_) => return Err("input must be a string or an array of input items".into()),
    }

    if messages.is_empty() {
        return Err("the response request has no messages".into());
    }

    let tools = translate_tools(request.get("tools"))?;
    let tool_choice = translate_tool_choice(request.get("tool_choice"))?;
    let response_format = translate_text_format(request.get("text"))?;

    let mut body = Map::new();
    body.insert("model".into(), json!(upstream_model));
    body.insert("messages".into(), Value::Array(messages));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tool_choice) = tool_choice {
        body.insert("tool_choice".into(), tool_choice);
    }
    if let Some(parallel) = request.get("parallel_tool_calls") {
        match parallel {
            Value::Bool(_) => {
                body.insert("parallel_tool_calls".into(), parallel.clone());
            }
            Value::Null => {}
            _ => return Err("parallel_tool_calls must be a boolean".into()),
        }
    }
    if let Some(response_format) = response_format {
        body.insert("response_format".into(), response_format);
    }
    if let Some(max_tokens) = request.get("max_output_tokens") {
        match max_tokens {
            Value::Null => {}
            Value::Number(number) if number.as_u64().is_some() => {
                body.insert(options.max_tokens_field.name().into(), max_tokens.clone());
            }
            _ => return Err("max_output_tokens must be a non-negative integer".into()),
        }
    }
    for field in FORWARDED_FIELDS {
        if let Some(value) = request.get(field)
            && !value.is_null()
        {
            body.insert(field.into(), value.clone());
        }
    }
    if options.reasoning_effort == ReasoningEffort::Forward
        && let Some(effort) = request
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
        && !effort.is_null()
    {
        body.insert("reasoning_effort".into(), effort.clone());
    }
    body.insert("stream".into(), Value::Bool(true));
    if options.include_stream_usage {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Ok(Value::Object(body))
}

/// Responses features whose semantics need upstream-stored state, which a stateless
/// Chat Completions endpoint cannot honour.
fn reject_server_state(request: &Map<String, Value>) -> Result<(), String> {
    for (field, message) in [
        (
            "previous_response_id",
            "previous_response_id is not supported by the chat completions adapter; resend the full conversation in input",
        ),
        (
            "conversation",
            "conversation is not supported by the chat completions adapter; resend the full conversation in input",
        ),
    ] {
        match request.get(field) {
            None | Some(Value::Null) => {}
            Some(_) => return Err(message.into()),
        }
    }
    for (field, message) in [
        (
            "store",
            "store: true is not supported by the chat completions adapter",
        ),
        (
            "background",
            "background: true is not supported by the chat completions adapter",
        ),
    ] {
        if request.get(field) == Some(&Value::Bool(true)) {
            return Err(message.into());
        }
    }
    Ok(())
}

/// The adapter only implements the streaming half of the protocol.
fn reject_non_streaming(request: &Map<String, Value>) -> Result<(), String> {
    match request.get("stream") {
        None | Some(Value::Bool(true)) => Ok(()),
        Some(Value::Bool(false)) => {
            Err("the chat completions adapter only supports stream: true".into())
        }
        Some(_) => Err("stream must be a boolean".into()),
    }
}

fn push_input_items(
    items: &[Value],
    options: &AdapterOptions,
    messages: &mut Vec<Value>,
) -> Result<(), String> {
    for (position, item) in items.iter().enumerate() {
        let item = item
            .as_object()
            .ok_or_else(|| format!("input item {position} must be a JSON object"))?;
        let kind = match item.get("type").and_then(Value::as_str) {
            Some(kind) => kind,
            None if item.contains_key("role") => "message",
            None => return Err(format!("input item {position} needs a type")),
        };
        match kind {
            "message" => push_message(item, position, options, messages)?,
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("input item {position} needs a call_id"))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("input item {position} needs a name"))?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("input item {position} needs arguments"))?;
                push_function_call(messages, call_id, name, arguments);
            }
            "function_call_output" => {
                // Codex injects goal context as a namespaced pseudo-tool result
                // carrying `name` + `namespace` and no `call_id`. It is not a
                // real call result, so there is no `tool_call_id` to answer;
                // forwarding it as context keeps the objective on later turns.
                if is_injected_context(item) {
                    push_injected_context(messages, item, position)?;
                    continue;
                }
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("input item {position} needs a call_id"))?;
                let output = item
                    .get("output")
                    .ok_or_else(|| format!("input item {position} needs output"))?;
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": tool_output(output, position)?,
                }));
            }
            // Reasoning is replayable only against the store that produced it, so
            // it is dropped rather than forwarded as a bogus assistant message.
            "reasoning" => {}
            other => {
                return Err(format!(
                    "the {other} input item type is not supported by the chat completions adapter"
                ));
            }
        }
    }
    Ok(())
}

/// A `function_call_output` that stands in for tool output Codex holds itself
/// rather than a real call the model made. Codex marks these with a `namespace`
/// and no `call_id`.
fn is_injected_context(item: &Map<String, Value>) -> bool {
    let namespaced = item.get("namespace").is_some_and(|value| !value.is_null());
    let has_call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .is_some_and(|call_id| !call_id.is_empty());
    namespaced && !has_call_id
}

/// Fold an injected context item into a plain `user` turn.
///
/// It cannot become a `tool` message: there is no `tool_call_id` to answer, and
/// a `tool` message with an invented id would be rejected upstream or, worse,
/// silently answered as if the model had asked. A user message keeps the text in
/// the conversation for later turns without pretending a call happened.
fn push_injected_context(
    messages: &mut Vec<Value>,
    item: &Map<String, Value>,
    position: usize,
) -> Result<(), String> {
    let output = item
        .get("output")
        .ok_or_else(|| format!("input item {position} needs output"))?;
    let text = tool_output(output, position)?;
    if text.trim().is_empty() {
        return Ok(());
    }
    let label = match (
        item.get("namespace").and_then(Value::as_str),
        item.get("name").and_then(Value::as_str),
    ) {
        (Some(namespace), Some(name)) => format!("{namespace}.{name}"),
        (None, Some(name)) => name.to_owned(),
        _ => "context".to_owned(),
    };
    messages.push(json!({
        "role": "user",
        "content": format!("[{label}]\n{text}"),
    }));
    Ok(())
}

fn push_message(
    item: &Map<String, Value>,
    position: usize,
    options: &AdapterOptions,
    messages: &mut Vec<Value>,
) -> Result<(), String> {
    let role = item
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("input item {position} needs a role"))?;
    let role = match role {
        "system" => "system",
        "user" => "user",
        "assistant" => "assistant",
        "developer" => match options.developer_role {
            DeveloperRole::System => "system",
            DeveloperRole::Developer => "developer",
        },
        other => {
            return Err(format!(
                "the {other} message role is not supported by the chat completions adapter"
            ));
        }
    };
    let content = message_content(role, item.get("content"), position)?;
    messages.push(json!({"role": role, "content": content.unwrap_or(Value::Null)}));
    Ok(())
}

fn message_content(
    role: &str,
    content: Option<&Value>,
    position: usize,
) -> Result<Option<Value>, String> {
    match content {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(json!(text))),
        Some(Value::Array(parts)) => {
            let mut texts = Vec::new();
            let mut chat_parts = Vec::new();
            let mut has_image = false;
            for part in parts {
                let part = part.as_object().ok_or_else(|| {
                    format!("input item {position} has a content part that is not an object")
                })?;
                let kind = part.get("type").and_then(Value::as_str).ok_or_else(|| {
                    format!("input item {position} has a content part without a type")
                })?;
                match kind {
                    "input_text" | "output_text" | "text" | "refusal" => {
                        let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                            format!("the {kind} content part in input item {position} needs text")
                        })?;
                        texts.push(text);
                        chat_parts.push(json!({"type": "text", "text": text}));
                    }
                    "input_image" => {
                        let url = part.get("image_url").and_then(Value::as_str).ok_or_else(|| {
                            format!(
                                "the input_image content part in input item {position} needs image_url"
                            )
                        })?;
                        let mut image = Map::new();
                        image.insert("url".into(), json!(url));
                        if let Some(detail) = part.get("detail").filter(|detail| !detail.is_null())
                        {
                            image.insert("detail".into(), detail.clone());
                        }
                        chat_parts
                            .push(json!({"type": "image_url", "image_url": Value::Object(image)}));
                        has_image = true;
                    }
                    other => {
                        return Err(format!(
                            "the {other} content part type is not supported by the chat completions adapter"
                        ));
                    }
                }
            }
            if has_image {
                if role != "user" {
                    return Err(format!(
                        "{role} messages cannot carry image content in the chat completions adapter"
                    ));
                }
                Ok(Some(Value::Array(chat_parts)))
            } else {
                Ok(Some(json!(texts.concat())))
            }
        }
        Some(_) => Err(format!(
            "the message content in input item {position} must be a string or an array of content parts"
        )),
    }
}

fn tool_output(output: &Value, position: usize) -> Result<String, String> {
    match output {
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                let part = part.as_object().ok_or_else(|| {
                    format!("the function_call_output in input item {position} has a part that is not an object")
                })?;
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        text.push_str(part.get("text").and_then(Value::as_str).ok_or_else(
                            || {
                                format!(
                                    "a tool output text part in input item {position} needs text"
                                )
                            },
                        )?);
                    }
                    Some(other) => {
                        return Err(format!(
                            "the {other} tool output part is not supported by the chat completions adapter"
                        ));
                    }
                    None => {
                        return Err(format!(
                            "a tool output part in input item {position} needs a type"
                        ));
                    }
                }
            }
            Ok(text)
        }
        _ => Err(format!(
            "the function_call_output in input item {position} must be a string or an array of content parts"
        )),
    }
}

/// Append one function call to the assistant turn it belongs to.
///
/// Chat Completions carries every call of a turn in a single assistant message, so
/// consecutive `function_call` items fold into the preceding assistant message
/// instead of becoming separate messages. Their order inside `tool_calls` is the
/// order in `input`, and `call_id` is carried through unchanged so the matching
/// `tool` message still resolves.
fn push_function_call(messages: &mut Vec<Value>, call_id: &str, name: &str, arguments: &str) {
    let has_assistant_tail = messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("assistant");
    if !has_assistant_tail {
        messages.push(json!({"role": "assistant", "content": Value::Null}));
    }
    let message = messages
        .last_mut()
        .and_then(Value::as_object_mut)
        .expect("the assistant message was just pushed");
    if !message.contains_key("tool_calls") {
        message.insert("tool_calls".into(), Value::Array(Vec::new()));
    }
    let calls = message
        .get_mut("tool_calls")
        .and_then(Value::as_array_mut)
        .expect("tool_calls is an array");
    calls.push(json!({
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    }));
}

fn translate_tools(tools: Option<&Value>) -> Result<Vec<Value>, String> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };
    let tools = match tools {
        Value::Null => return Ok(Vec::new()),
        Value::Array(tools) => tools,
        _ => return Err("tools must be an array".into()),
    };
    let mut translated = Vec::with_capacity(tools.len());
    for (position, tool) in tools.iter().enumerate() {
        let tool = tool
            .as_object()
            .ok_or_else(|| format!("tool {position} must be a JSON object"))?;
        let kind = tool
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("tool {position} needs a type"))?;
        if kind != "function" {
            return Err(format!(
                "the {kind} tool type is not supported by the chat completions adapter; only function tools can be translated"
            ));
        }
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("function tool {position} needs a name"))?;
        let mut function = Map::new();
        function.insert("name".into(), json!(name));
        if let Some(description) = tool.get("description").filter(|value| !value.is_null()) {
            function.insert("description".into(), description.clone());
        }
        function.insert(
            "parameters".into(),
            tool.get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        );
        if let Some(strict) = tool.get("strict").filter(|value| !value.is_null()) {
            function.insert("strict".into(), strict.clone());
        }
        translated.push(json!({"type": "function", "function": Value::Object(function)}));
    }
    Ok(translated)
}

fn translate_tool_choice(tool_choice: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(tool_choice) = tool_choice else {
        return Ok(None);
    };
    match tool_choice {
        Value::Null => Ok(None),
        Value::String(mode) if matches!(mode.as_str(), "auto" | "none" | "required") => {
            Ok(Some(json!(mode)))
        }
        Value::Object(choice) if choice.get("type") == Some(&json!("function")) => {
            let name = choice
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or("a function tool_choice needs a name")?;
            Ok(Some(
                json!({"type": "function", "function": {"name": name}}),
            ))
        }
        Value::Object(choice) => {
            let kind = choice
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Err(format!(
                "the {kind} tool_choice is not supported by the chat completions adapter"
            ))
        }
        _ => Err("tool_choice must be a string or an object".into()),
    }
}

fn translate_text_format(text: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(text) = text else {
        return Ok(None);
    };
    match text {
        Value::Null => Ok(None),
        Value::Object(text) => {
            let Some(format) = text.get("format") else {
                return Ok(None);
            };
            let format = match format {
                Value::Null => return Ok(None),
                Value::Object(format) => format,
                _ => return Err("text.format must be an object".into()),
            };
            match format.get("type").and_then(Value::as_str) {
                Some("text") | None => Ok(None),
                Some("json_object") => Ok(Some(json!({"type": "json_object"}))),
                Some("json_schema") => {
                    let name = format
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or("a json_schema text format needs a name")?;
                    let mut schema = Map::new();
                    schema.insert("name".into(), json!(name));
                    schema.insert(
                        "schema".into(),
                        format
                            .get("schema")
                            .cloned()
                            .ok_or("a json_schema text format needs a schema")?,
                    );
                    if let Some(strict) = format.get("strict").filter(|value| !value.is_null()) {
                        schema.insert("strict".into(), strict.clone());
                    }
                    Ok(Some(
                        json!({"type": "json_schema", "json_schema": Value::Object(schema)}),
                    ))
                }
                Some(other) => Err(format!(
                    "the {other} text format is not supported by the chat completions adapter"
                )),
            }
        }
        _ => Err("text must be an object".into()),
    }
}

/// Translates Chat Completions SSE chunks into Responses API stream events.
///
/// The caller frames SSE, skips comments and blank lines, and passes the `event`
/// name plus the `data` payload of each frame. Every returned event is a complete
/// JSON object carrying `type` and `sequence_number`; the caller serialises each
/// one as `event: <type>` / `data: <json>`.
///
/// A stream is complete only after `[DONE]` (through [`push_event`]) or
/// [`finish`], and only when a `finish_reason` arrived. An upstream that stops
/// mid-stream is an error, never a silently truncated message.
///
/// [`push_event`]: ResponseTranslator::push_event
/// [`finish`]: ResponseTranslator::finish
pub struct ResponseTranslator {
    response_id: String,
    model: String,
    created_at: i64,
    sequence_number: u64,
    started: bool,
    finished: bool,
    finish_reason: Option<String>,
    usage: Option<Value>,
    items: Vec<StreamItem>,
    next_output_index: usize,
    output: Vec<Value>,
    compat_calls: usize,
}

struct MessageState {
    item_id: String,
    output_index: usize,
    parts: Vec<PartState>,
}

struct PartState {
    kind: PartKind,
    content_index: usize,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PartKind {
    Text,
    Refusal,
}

impl PartKind {
    fn delta_event(self) -> &'static str {
        match self {
            Self::Text => "response.output_text.delta",
            Self::Refusal => "response.refusal.delta",
        }
    }

    fn done_event(self) -> &'static str {
        match self {
            Self::Text => "response.output_text.done",
            Self::Refusal => "response.refusal.done",
        }
    }

    fn text_field(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Refusal => "refusal",
        }
    }

    fn empty_part(self) -> Value {
        match self {
            Self::Text => json!({"type": "output_text", "text": "", "annotations": []}),
            Self::Refusal => json!({"type": "refusal", "refusal": ""}),
        }
    }

    fn part(self, text: &str) -> Value {
        match self {
            Self::Text => json!({"type": "output_text", "text": text, "annotations": []}),
            Self::Refusal => json!({"type": "refusal", "refusal": text}),
        }
    }
}

struct ToolState {
    /// Position in the upstream chunk's `tool_calls[].index`.
    chunk_index: u64,
    /// Responses `output_index`, assigned on first appearance.
    output_index: usize,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    /// Byte offset into `arguments` already sent as deltas.
    emitted: usize,
    announced: bool,
}

enum StreamItem {
    Message(MessageState),
    Tool(ToolState),
}

impl MessageState {
    fn value(&self, status: &str) -> Value {
        let content = self
            .parts
            .iter()
            .map(|part| part.kind.part(&part.text))
            .collect::<Vec<_>>();
        json!({
            "id": self.item_id,
            "type": "message",
            "status": status,
            "role": "assistant",
            "content": content,
        })
    }
}

impl ToolState {
    fn value(&self, status: &str, arguments: &str) -> Value {
        json!({
            "id": self.item_id,
            "type": "function_call",
            "status": status,
            "call_id": self.call_id,
            "name": self.name,
            "arguments": arguments,
        })
    }
}

impl ResponseTranslator {
    /// `response_id` and `created_at` come from the caller so events are
    /// reproducible; `model` is reported back to the client as the answering model.
    pub fn new(response_id: impl Into<String>, model: impl Into<String>, created_at: i64) -> Self {
        Self {
            response_id: response_id.into(),
            model: model.into(),
            created_at,
            sequence_number: 0,
            started: false,
            finished: false,
            finish_reason: None,
            usage: None,
            items: Vec::new(),
            next_output_index: 0,
            output: Vec::new(),
            compat_calls: 0,
        }
    }

    /// Whether `[DONE]` or [`finish`](Self::finish) already completed the stream.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Handle one already-framed SSE event.
    ///
    /// `event` is the SSE `event:` name (`"message"` when the stream omits it);
    /// `data` is the payload. `[DONE]` completes the stream. Anything that cannot
    /// be translated is an error, including events after completion.
    pub fn push_event(&mut self, event: &str, data: &str) -> Result<Vec<Value>, String> {
        if self.finished {
            return Err("received an event after the chat completion stream finished".into());
        }
        let data = data.trim();
        if event == "error" {
            return Err(format!(
                "the chat completions endpoint reported an error: {}",
                error_message(data)
            ));
        }
        // SSE keepalive frames are not part of the protocol payload; they are
        // tolerated so a healthy stream is not failed by a heartbeat.
        if matches!(event, "ping" | "keepalive") {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            return self.finalize("[DONE]");
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|error| format!("malformed chat completion chunk: {error}"))?;
        let chunk = chunk
            .as_object()
            .ok_or("a chat completion chunk must be a JSON object")?;
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            return Err(format!(
                "the chat completions endpoint reported an error: {}",
                error_message(&error.to_string())
            ));
        }

        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(self.response_event("response.created", "in_progress", None));
            events.push(self.response_event("response.in_progress", "in_progress", None));
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = Some(translate_usage(usage)?);
        }
        let choices = chunk
            .get("choices")
            .and_then(Value::as_array)
            .ok_or("a chat completion chunk is missing the choices array")?;
        for choice in choices {
            let choice = choice
                .as_object()
                .ok_or("a chat completion choice must be a JSON object")?;
            // This adapter always asks for `n = 1`; other choices are not part of
            // the translated conversation and are ignored.
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            if let Some(delta) = choice.get("delta").filter(|delta| !delta.is_null()) {
                let delta = delta
                    .as_object()
                    .ok_or("a chat completion choice delta must be a JSON object")?;
                self.push_delta(delta, &mut events)?;
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                // Fail on an unknown reason before closing anything, so a caller
                // never receives half a completion.
                let (status, _) = finish_status(reason)?;
                self.finish_reason = Some(reason.to_owned());
                events.extend(self.close_items(status == "completed")?);
            }
        }
        Ok(events)
    }

    /// Finish a stream whose transport ended without `[DONE]`.
    ///
    /// Returns the pending completion events when a `finish_reason` arrived and an
    /// error when the stream was cut short. Idempotent once finished.
    pub fn finish(&mut self) -> Result<Vec<Value>, String> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finalize("the upstream connection closing")
    }

    fn finalize(&mut self, terminator: &str) -> Result<Vec<Value>, String> {
        let reason = self.finish_reason.clone().ok_or_else(|| {
            format!("the chat completion stream ended at {terminator} before a finish_reason")
        })?;
        let (status, incomplete) = finish_status(&reason)?;
        let mut events = self.close_items(status == "completed")?;
        let event = if status == "completed" {
            "response.completed"
        } else {
            "response.incomplete"
        };
        events.push(self.response_event(event, status, incomplete));
        self.finished = true;
        Ok(events)
    }

    fn push_delta(
        &mut self,
        delta: &Map<String, Value>,
        events: &mut Vec<Value>,
    ) -> Result<(), String> {
        match delta.get("content") {
            None | Some(Value::Null) => {}
            // An empty delta carries no text; opening a part for it would leave a
            // stray empty part on tool-call-only and refusal-only streams.
            Some(Value::String(text)) if text.is_empty() => {}
            Some(Value::String(text)) => self.push_part(PartKind::Text, text, events),
            Some(_) => return Err("a delta content value must be a string".into()),
        }
        match delta.get("refusal") {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) if text.is_empty() => {}
            Some(Value::String(text)) => self.push_part(PartKind::Refusal, text, events),
            Some(_) => return Err("a delta refusal value must be a string".into()),
        }
        if let Some(calls) = delta.get("tool_calls").filter(|calls| !calls.is_null()) {
            let calls = calls
                .as_array()
                .ok_or("delta tool_calls must be an array")?;
            for call in calls {
                self.push_tool_call(call, events)?;
            }
        }
        // Deprecated single-call delta, still emitted by some compatible servers.
        // It carries no ID, so the adapter generates a stable one for the response.
        if let Some(function) = delta.get("function_call").filter(|call| !call.is_null()) {
            let call_id = format!("call_compat_{}", self.compat_calls);
            self.compat_calls += 1;
            let wrapped = json!({"index": 0, "id": call_id, "function": function.clone()});
            self.push_tool_call(&wrapped, events)?;
        }
        Ok(())
    }

    fn push_part(&mut self, kind: PartKind, text: &str, events: &mut Vec<Value>) {
        if !self
            .items
            .iter()
            .any(|item| matches!(item, StreamItem::Message(_)))
        {
            let output_index = self.next_output_index;
            self.next_output_index += 1;
            let message = MessageState {
                item_id: message_item_id(&self.response_id, output_index),
                output_index,
                parts: Vec::new(),
            };
            let mut added = Map::new();
            added.insert("output_index".into(), json!(output_index));
            added.insert("item".into(), message.value("in_progress"));
            events.push(self.event("response.output_item.added", added));
            self.items.push(StreamItem::Message(message));
        }

        let position = self
            .items
            .iter()
            .position(|item| matches!(item, StreamItem::Message(_)))
            .expect("the message item was just inserted");
        let (item_id, output_index, content_index, part_added) = {
            let message = match &mut self.items[position] {
                StreamItem::Message(message) => message,
                StreamItem::Tool(_) => unreachable!("position matched a message item"),
            };
            let existing = message.parts.iter().position(|part| part.kind == kind);
            let content_index = match existing {
                Some(index) => {
                    message.parts[index].text.push_str(text);
                    message.parts[index].content_index
                }
                None => {
                    let content_index = message.parts.len();
                    message.parts.push(PartState {
                        kind,
                        content_index,
                        text: text.to_owned(),
                    });
                    content_index
                }
            };
            (
                message.item_id.clone(),
                message.output_index,
                content_index,
                existing.is_none(),
            )
        };

        if part_added {
            let mut added = Map::new();
            added.insert("item_id".into(), json!(item_id));
            added.insert("output_index".into(), json!(output_index));
            added.insert("content_index".into(), json!(content_index));
            added.insert("part".into(), kind.empty_part());
            events.push(self.event("response.content_part.added", added));
        }
        let mut delta = Map::new();
        delta.insert("item_id".into(), json!(item_id));
        delta.insert("output_index".into(), json!(output_index));
        delta.insert("content_index".into(), json!(content_index));
        // Both text and refusal deltas carry the incremental string in `delta`;
        // only the terminal `.done` event renames the accumulated value.
        delta.insert("delta".into(), json!(text));
        if kind == PartKind::Text {
            delta.insert("logprobs".into(), json!([]));
        }
        events.push(self.event(kind.delta_event(), delta));
    }

    /// Track one `tool_calls[]` delta.
    fn push_tool_call(&mut self, call: &Value, events: &mut Vec<Value>) -> Result<(), String> {
        let call = call
            .as_object()
            .ok_or("each delta tool call must be a JSON object")?;
        let chunk_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let function = call
            .get("function")
            .and_then(Value::as_object)
            .ok_or("each delta tool call needs a function object")?;
        let id = call.get("id").and_then(Value::as_str);
        let name = function.get("name").and_then(Value::as_str);
        let arguments = match function.get("arguments") {
            None | Some(Value::Null) => "",
            Some(Value::String(arguments)) => arguments.as_str(),
            Some(_) => return Err("delta tool call arguments must be a string".into()),
        };

        let position = match self.items.iter().position(
            |item| matches!(item, StreamItem::Tool(tool) if tool.chunk_index == chunk_index),
        ) {
            Some(position) => position,
            None => {
                let output_index = self.next_output_index;
                self.next_output_index += 1;
                self.items.push(StreamItem::Tool(ToolState {
                    chunk_index,
                    output_index,
                    item_id: String::new(),
                    call_id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    emitted: 0,
                    announced: false,
                }));
                self.items.len() - 1
            }
        };

        let mut announce = None;
        {
            let tool = match &mut self.items[position] {
                StreamItem::Tool(tool) => tool,
                StreamItem::Message(_) => unreachable!("position matched a tool item"),
            };
            // The first non-empty upstream ID wins, so later chunks that repeat or
            // omit it cannot change the ID the client already saw.
            if let Some(id) = id.filter(|id| !id.is_empty())
                && tool.call_id.is_empty()
            {
                tool.call_id = id.to_owned();
            }
            // The name is taken from the first delta that carries one; it is not
            // re-read afterwards, so a provider that splits the name is out of
            // baseline and the client sees the first fragment only.
            if let Some(name) = name.filter(|name| !name.is_empty())
                && !tool.announced
                && tool.name.is_empty()
            {
                tool.name.push_str(name);
            }
            if !arguments.is_empty() {
                tool.arguments.push_str(arguments);
            }
            if !tool.announced && !tool.name.is_empty() {
                tool.item_id = if tool.call_id.is_empty() {
                    format!("fc_{}", tool.output_index)
                } else {
                    format!("fc_{}", tool.call_id)
                };
                tool.announced = true;
                announce = Some((tool.output_index, tool.value("in_progress", "")));
            }
        }
        if let Some((output_index, item)) = announce {
            let mut added = Map::new();
            added.insert("output_index".into(), json!(output_index));
            added.insert("item".into(), item);
            events.push(self.event("response.output_item.added", added));
        }

        let mut pending = None;
        {
            let tool = match &mut self.items[position] {
                StreamItem::Tool(tool) => tool,
                StreamItem::Message(_) => unreachable!("position matched a tool item"),
            };
            if tool.announced && tool.emitted < tool.arguments.len() {
                pending = Some((
                    tool.item_id.clone(),
                    tool.output_index,
                    tool.arguments[tool.emitted..].to_owned(),
                ));
                tool.emitted = tool.arguments.len();
            }
        }
        if let Some((item_id, output_index, delta)) = pending {
            let mut event = Map::new();
            event.insert("item_id".into(), json!(item_id));
            event.insert("output_index".into(), json!(output_index));
            event.insert("delta".into(), json!(delta));
            events.push(self.event("response.function_call_arguments.delta", event));
        }
        Ok(())
    }

    /// Close every open item, in output order, and record it in the response.
    ///
    /// Validation happens before any item is consumed, so a failure leaves the
    /// translator's items intact for the caller to inspect.
    fn close_items(&mut self, complete_tools: bool) -> Result<Vec<Value>, String> {
        if !complete_tools {
            // A partial call must not become an executable Codex tool item.
            self.items
                .retain(|item| matches!(item, StreamItem::Message(_)));
        }
        if let Some(tool) = self.items.iter().find_map(|item| match item {
            StreamItem::Tool(tool) if !tool.announced => Some(tool),
            _ => None,
        }) {
            return Err(format!(
                "function call {} finished before the upstream named it",
                tool.chunk_index
            ));
        }
        let items = std::mem::take(&mut self.items);
        let mut events = Vec::new();
        for item in items {
            match item {
                StreamItem::Message(message) => {
                    for part in &message.parts {
                        let mut done = Map::new();
                        done.insert("item_id".into(), json!(message.item_id));
                        done.insert("output_index".into(), json!(message.output_index));
                        done.insert("content_index".into(), json!(part.content_index));
                        done.insert(part.kind.text_field().into(), json!(part.text));
                        if part.kind == PartKind::Text {
                            done.insert("logprobs".into(), json!([]));
                        }
                        events.push(self.event(part.kind.done_event(), done));

                        let mut part_done = Map::new();
                        part_done.insert("item_id".into(), json!(message.item_id));
                        part_done.insert("output_index".into(), json!(message.output_index));
                        part_done.insert("content_index".into(), json!(part.content_index));
                        part_done.insert("part".into(), part.kind.part(&part.text));
                        events.push(self.event("response.content_part.done", part_done));
                    }
                    let mut item_done = Map::new();
                    item_done.insert("output_index".into(), json!(message.output_index));
                    item_done.insert("item".into(), message.value("completed"));
                    events.push(self.event("response.output_item.done", item_done));
                    self.output.push(message.value("completed"));
                }
                StreamItem::Tool(tool) => {
                    let mut done = Map::new();
                    done.insert("item_id".into(), json!(tool.item_id));
                    done.insert("output_index".into(), json!(tool.output_index));
                    done.insert("arguments".into(), json!(tool.arguments));
                    events.push(self.event("response.function_call_arguments.done", done));
                    let mut item_done = Map::new();
                    item_done.insert("output_index".into(), json!(tool.output_index));
                    item_done.insert("item".into(), tool.value("completed", &tool.arguments));
                    events.push(self.event("response.output_item.done", item_done));
                    self.output.push(tool.value("completed", &tool.arguments));
                }
            }
        }
        Ok(events)
    }

    fn event(&mut self, kind: &str, fields: Map<String, Value>) -> Value {
        let mut event = fields;
        event.insert("type".into(), json!(kind));
        event.insert("sequence_number".into(), json!(self.sequence_number));
        self.sequence_number += 1;
        Value::Object(event)
    }

    fn response_event(&mut self, kind: &str, status: &str, incomplete: Option<Value>) -> Value {
        let mut fields = Map::new();
        fields.insert("response".into(), self.response_object(status, incomplete));
        self.event(kind, fields)
    }

    fn response_object(&self, status: &str, incomplete: Option<Value>) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "background": false,
            "error": Value::Null,
            "incomplete_details": incomplete,
            "instructions": Value::Null,
            "max_output_tokens": Value::Null,
            "model": self.model,
            "output": self.output,
            "parallel_tool_calls": true,
            "previous_response_id": Value::Null,
            "reasoning": {"effort": Value::Null, "summary": Value::Null},
            "store": false,
            "temperature": Value::Null,
            "text": {"format": {"type": "text"}},
            "tool_choice": "auto",
            "tools": [],
            "top_p": Value::Null,
            "truncation": "disabled",
            "usage": self.usage.clone().unwrap_or(Value::Null),
            "user": Value::Null,
            "metadata": {},
        })
    }
}

/// Map a Chat Completions `finish_reason` onto a Responses status.
fn finish_status(reason: &str) -> Result<(&'static str, Option<Value>), String> {
    match reason {
        "stop" | "tool_calls" | "function_call" => Ok(("completed", None)),
        "length" => Ok(("incomplete", Some(json!({"reason": "max_output_tokens"})))),
        "content_filter" => Ok(("incomplete", Some(json!({"reason": "content_filter"})))),
        other => Err(format!(
            "the chat completion finished with an unsupported finish_reason: {other}"
        )),
    }
}

/// Map Chat Completions usage onto Responses usage.
fn translate_usage(usage: &Value) -> Result<Value, String> {
    let usage = usage.as_object().ok_or("usage must be a JSON object")?;
    let count = |name: &str| usage.get(name).and_then(Value::as_u64);
    let input = count("prompt_tokens").ok_or("usage is missing prompt_tokens")?;
    let output = count("completion_tokens").ok_or("usage is missing completion_tokens")?;
    let total = count("total_tokens").unwrap_or(input + output);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": total,
    }))
}

/// Best-effort message for an upstream error payload, which may be a JSON object or
/// plain text.
fn error_message(data: &str) -> String {
    match serde_json::from_str::<Value>(data) {
        Ok(Value::Object(object)) => {
            let nested = match object.get("error") {
                Some(Value::Object(error)) => error.get("message").and_then(Value::as_str),
                Some(Value::String(message)) => Some(message.as_str()),
                _ => None,
            };
            nested
                .or_else(|| object.get("message").and_then(Value::as_str))
                .unwrap_or(data)
                .to_owned()
        }
        Ok(Value::String(message)) => message,
        _ => data.to_owned(),
    }
}
