//! Convert an Anthropic Messages request from stock Claude Code into the
//! relay's canonical Responses request.
//!
//! The converter is pure and offline: no I/O, no clocks, no dependencies
//! beyond `serde_json`. It is used twice: once as the request half of a
//! Responses upstream, and once as the input to the existing
//! [Responses to Chat Completions adapter](crate::adapters::chat_completions),
//! so every shape written here stays inside the subset that adapter accepts
//! for text-only conversations.
//!
//! # Mapping
//!
//! | Anthropic Messages | Responses |
//! | --- | --- |
//! | `model` | replaced by the configured `upstream_model` |
//! | `system` string or `text` blocks | `instructions`, blocks concatenated in order |
//! | leading `role: "system"` messages | appended to `instructions` |
//! | later `role: "system"` messages | `developer` message (text only) |
//! | `messages[].content` text | `message` with `input_text` / `output_text` |
//! | `tool_use` | `function_call` with `call_id`, `name`, compact JSON arguments |
//! | `tool_result` | `function_call_output` keyed by `call_id` |
//! | `image` (base64 or URL) | `input_image` with `image_url` |
//! | `tool_reference` in a tool result | a text note naming the tool |
//! | `tools[]` custom definition | `function` with `parameters` equal to `input_schema` |
//! | `tool_addition` / `tool_removal` | flatten the final active tool set |
//! | `tool_choice` auto/none/any/tool | `auto` / `none` / `required` / named function |
//! | `disable_parallel_tool_use` | `parallel_tool_calls: false` |
//! | `max_tokens` | `max_output_tokens` |
//! | `temperature`, `top_p` | forwarded unchanged |
//! | `stop_sequences` | `stop` |
//! | `metadata.user_id` | `metadata.user_id` |
//! | `output_config.effort` | `reasoning.effort` |
//! | `output_config.format` | `text.format` (`json_schema`) |
//! | `service_tier` auto/standard_only | `auto` / `default` |
//! | `stream` | copied, including `false`; absent stays absent |
//!
//! # Stock Claude Code quirks
//!
//! Captured local-loopback requests (`~/.claude/projects/*.jsonl`) show the
//! shapes handled here:
//!
//! * `system` is an array of `text` blocks. Prompt-cache breakpoints are
//!   expressed with `cache_control` on system blocks, message blocks, tools,
//!   and sometimes a top-level body field. All of them are stripped; Responses
//!   has no equivalent marker in the request body.
//! * Tools carry `eager_input_streaming`, `defer_loading`, `cache_control`, and
//!   `caller` hints that are not part of the Responses function-tool schema.
//!   They are ignored, while the name, description and complete JSON Schema
//!   are preserved.
//! * `defer_loading: true` withholds a declared tool until a `tool_addition`
//!   block surfaces it. The final active set is flattened into the Responses
//!   `tools` array because Responses has no per-turn tool changes. Deferred
//!   tools that were never surfaced are intentionally not offered (they were
//!   never offered to Claude either).
//! * `tool_addition` and `tool_removal` are `system` content blocks. They can
//!   be mixed with `text` blocks and are applied in conversation order.
//! * `metadata.user_id` is a JSON-encoded string containing Claude Code's
//!   device/account/session ids. It is preserved as an opaque string.
//! * `context_management`, `betas`, `safeguards`, `thread`, `speed`,
//!   `diagnostics`, `tags`, `thinking` and per-message `output_config` are
//!   provider-side hints with no Responses equivalent; they are ignored.
//!   `output_config.effort` and `output_config.format` are the two exceptions
//!   and are mapped above.
//! * `advisor_20260301` is a deferred Anthropic server tool. It stays out of
//!   `tools` unless a tool addition surfaces it; a *surfaced* server tool is
//!   rejected rather than silently advertised as a client function.
//!
//! # Deliberate rule: opaque thinking
//!
//! `thinking` and `redacted_thinking` blocks are omitted. Their signatures are
//! bound to an Anthropic model and cannot be replayed to a non-Anthropic
//! destination. Visible assistant `text` next to a thinking block is kept.
//!
//! # Known protocol gaps
//!
//! * `document` blocks (base64/URL/text documents) and Anthropic `file`
//!   image sources are rejected; only inline or URL text/images are
//!   representable in the subset shared by both destinations.
//! * A tool result containing an image becomes a `function_call_output` whose
//!   `output` is an array of `input_text`/`input_image` parts. Responses
//!   accepts that shape, but the existing Chat Completions adapter rejects
//!   image parts in tool output.
//! * A `tool_result`'s `is_error` flag has no Responses field; its text
//!   content is forwarded unchanged.
//! * `top_k` has no Responses field and is ignored.
//! * Chat Completions cannot represent non-streaming requests or `stop`, so a
//!   conversion produced from `stream: false` or with `stop_sequences` is only
//!   usable against a Responses upstream.
//! * `mcp_servers`/`container` declare provider-side tool containers and are
//!   rejected rather than dropped. Server-side `web_search` is expected to be
//!   normalized by the separate web-search worker into a normal function tool;
//!   an unnormalized server tool type is an error.
//! * `mcp_tool_reference` / `mcp_toolset_reference` tool changes are handled
//!   best-effort using Claude Code's `mcp__<server>__<tool>` naming. If the
//!   names do not match, the change is ignored rather than failing the turn.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

/// Longest tool name accepted by the converter. This matches the translated
/// adapters rather than the narrower Responses function-name limit, because
/// Claude Code's MCP names are commonly prefixed.
const MAX_TOOL_NAME: usize = 128;
/// Longest tool call id accepted. Anthropic ids are short; the bound only
/// rejects pathological input.
const MAX_TOOL_CALL_ID: usize = 512;
/// Reasoning efforts that can be forwarded unchanged to a Responses upstream.
const ALLOWED_EFFORTS: [&str; 7] = ["minimal", "low", "medium", "high", "xhigh", "max", "none"];

/// Convert one Anthropic Messages request body into canonical Responses JSON.
///
/// `upstream_model` always wins over the request's `model`, mirroring the
/// relay's other translated adapters.
pub(crate) fn into_responses(input: &Value, upstream_model: &str) -> Result<Value, String> {
    convert(input, upstream_model, false)
}

pub(crate) fn into_input_token_count(input: &Value, upstream_model: &str) -> Result<Value, String> {
    convert(input, upstream_model, true)
}

fn convert(input: &Value, upstream_model: &str, count_only: bool) -> Result<Value, String> {
    if upstream_model.trim().is_empty() {
        return Err("the upstream model id must not be empty".to_owned());
    }
    let request = input
        .as_object()
        .ok_or("the messages request body must be a JSON object")?;

    reject_provider_tool_containers(request)?;
    validate_thinking(request.get("thinking"))?;

    let max_tokens = (!count_only)
        .then(|| positive_u64(request.get("max_tokens"), "max_tokens"))
        .transpose()?;

    let mut builder = Builder::default();
    if let Some(system) = request.get("system") {
        append_system(system, &mut builder.instructions, "system")?;
    }

    let mut tools = ToolRegistry::from_request(request.get("tools"))?;
    let messages = match request.get("messages") {
        Some(Value::Array(messages)) => messages,
        Some(Value::Null) | None => {
            return Err("the messages request is missing messages".to_owned());
        }
        Some(_) => return Err("messages must be an array".to_owned()),
    };
    if messages.is_empty() {
        return Err("messages must contain at least one message".to_owned());
    }
    for (index, message) in messages.iter().enumerate() {
        builder.push_message(message, index, &mut tools)?;
    }
    let input_items = builder.finish()?;

    let (tool_choice, parallel_tool_calls) =
        translate_tool_choice(request.get("tool_choice"), &tools)?;
    let tools = tools.into_responses()?;

    let mut body = Map::new();
    body.insert("model".to_owned(), json!(upstream_model));
    // Anthropic concatenates system text blocks without inserting separators;
    // preserving that exactly matters for prompt-cache block boundaries.
    let instructions = builder.instructions.concat();
    if !instructions.is_empty() {
        body.insert("instructions".to_owned(), json!(instructions));
    }
    body.insert("input".to_owned(), Value::Array(input_items));
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(choice) = tool_choice {
        body.insert("tool_choice".to_owned(), choice);
    }
    if let Some(parallel) = parallel_tool_calls {
        body.insert("parallel_tool_calls".to_owned(), json!(parallel));
    }
    let Some(max_tokens) = max_tokens else {
        apply_output_config(request.get("output_config"), &mut body)?;
        return Ok(Value::Object(body));
    };
    body.insert("max_output_tokens".to_owned(), json!(max_tokens));
    insert_sampling(request, "temperature", &mut body)?;
    insert_sampling(request, "top_p", &mut body)?;
    if let Some(stop) = translate_stop_sequences(request.get("stop_sequences"))? {
        body.insert("stop".to_owned(), stop);
    }
    if let Some(metadata) = translate_metadata(request.get("metadata"))? {
        body.insert("metadata".to_owned(), metadata);
    }
    if let Some(tier) = translate_service_tier(request.get("service_tier"))? {
        body.insert("service_tier".to_owned(), json!(tier));
    }
    apply_output_config(request.get("output_config"), &mut body)?;
    if let Some(stream) = request.get("stream") {
        match stream {
            Value::Bool(_) => {
                body.insert("stream".to_owned(), stream.clone());
            }
            _ => return Err("stream must be a boolean when present".to_owned()),
        }
    }
    // The conversion is stateless: Anthropic Messages has no server-side
    // response store, so the destination must not create one either.
    body.insert("store".to_owned(), json!(false));
    Ok(Value::Object(body))
}

// ---------------------------------------------------------------------------
// Top-level request fields
// ---------------------------------------------------------------------------

/// Provider-side tool containers cannot be executed through a Responses
/// function surface. Rejecting them keeps tools visible instead of dropping
/// them silently.
fn reject_provider_tool_containers(request: &Map<String, Value>) -> Result<(), String> {
    match request.get("mcp_servers") {
        None | Some(Value::Null) => {}
        Some(Value::Array(servers)) if servers.is_empty() => {}
        Some(_) => {
            return Err(
                "mcp_servers declares provider-side MCP tools; normalize them into normal function tools before converting"
                    .to_owned(),
            );
        }
    }
    if request
        .get("container")
        .is_some_and(|value| !value.is_null())
    {
        return Err(
            "container is a provider-side tool container and cannot be converted to Responses"
                .to_owned(),
        );
    }
    Ok(())
}

fn validate_thinking(value: Option<&Value>) -> Result<(), String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let thinking = value
        .as_object()
        .ok_or("thinking must be an object when present")?;
    if let Some(kind) = thinking.get("type").filter(|value| !value.is_null())
        && !kind.is_string()
    {
        return Err("thinking.type must be a string".to_owned());
    }
    Ok(())
}

fn positive_u64(value: Option<&Value>, field: &str) -> Result<u64, String> {
    value
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{field} is required and must be a positive integer"))
}

fn insert_sampling(
    request: &Map<String, Value>,
    field: &str,
    body: &mut Map<String, Value>,
) -> Result<(), String> {
    let Some(value) = request.get(field).filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let number = value
        .as_f64()
        .ok_or_else(|| format!("{field} must be a number"))?;
    if !(0.0..=1.0).contains(&number) {
        return Err(format!("{field} must be between 0 and 1"));
    }
    // Claude Code can send sampling defaults explicitly. Some reasoning
    // models reject the fields even at their default values.
    if number != 1.0 {
        body.insert(field.to_owned(), value.clone());
    }
    Ok(())
}

fn translate_stop_sequences(value: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let sequences = match value {
        Value::String(sequence) => vec![json!(sequence)],
        Value::Array(sequences) => {
            let mut translated = Vec::with_capacity(sequences.len());
            for (index, sequence) in sequences.iter().enumerate() {
                let sequence = sequence
                    .as_str()
                    .filter(|sequence| !sequence.is_empty())
                    .ok_or_else(|| format!("stop_sequences[{index}] must be a non-empty string"))?;
                translated.push(json!(sequence));
            }
            translated
        }
        _ => return Err("stop_sequences must be a string or an array of strings".to_owned()),
    };
    if sequences.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Array(sequences)))
    }
}

fn translate_metadata(value: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let metadata = value
        .as_object()
        .ok_or("metadata must be an object when present")?;
    let Some(user_id) = metadata.get("user_id").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let user_id = user_id
        .as_str()
        .ok_or("metadata.user_id must be a string when present")?;
    if user_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({ "user_id": user_id })))
}

fn translate_service_tier(value: Option<&Value>) -> Result<Option<&'static str>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match value.as_str() {
        Some("auto") => Ok(Some("auto")),
        Some("standard_only") => Ok(Some("default")),
        Some(other) => Err(format!(
            "service_tier \"{other}\" is not supported; use auto or standard_only"
        )),
        None => Err("service_tier must be a string when present".to_owned()),
    }
}

fn apply_output_config(value: Option<&Value>, body: &mut Map<String, Value>) -> Result<(), String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let output_config = value
        .as_object()
        .ok_or("output_config must be an object when present")?;

    if let Some(effort) = output_config.get("effort").filter(|value| !value.is_null()) {
        let effort = effort
            .as_str()
            .ok_or("output_config.effort must be a string")?;
        if !ALLOWED_EFFORTS.contains(&effort) {
            return Err(format!(
                "output_config.effort \"{effort}\" is not supported; use minimal, low, medium, high, xhigh, max or none"
            ));
        }
        body.insert("reasoning".to_owned(), json!({ "effort": effort }));
    }

    let Some(format) = output_config.get("format").filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let format = format
        .as_object()
        .ok_or("output_config.format must be an object")?;
    let kind = format
        .get("type")
        .and_then(Value::as_str)
        .ok_or("output_config.format needs a type")?;
    match kind {
        "text" => Ok(()),
        "json_schema" => {
            let schema = format
                .get("schema")
                .filter(|value| value.is_object())
                .ok_or("output_config.format.json_schema needs an object schema")?;
            let name = format
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or("response");
            let mut translated = Map::new();
            translated.insert("type".to_owned(), json!("json_schema"));
            translated.insert("name".to_owned(), json!(name));
            translated.insert("schema".to_owned(), schema.clone());
            if let Some(strict) = format.get("strict").filter(|value| !value.is_null()) {
                if !strict.is_boolean() {
                    return Err("output_config.format.strict must be a boolean".to_owned());
                }
                translated.insert("strict".to_owned(), strict.clone());
            }
            body.insert("text".to_owned(), json!({ "format": translated }));
            Ok(())
        }
        other => Err(format!(
            "output_config.format type \"{other}\" is not supported"
        )),
    }
}

// ---------------------------------------------------------------------------
// System text
// ---------------------------------------------------------------------------

fn append_system(value: &Value, instructions: &mut Vec<String>, path: &str) -> Result<(), String> {
    match value {
        Value::String(text) => {
            if !text.trim().is_empty() {
                instructions.push(text.clone());
            }
            Ok(())
        }
        Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                let block_path = format!("{path}[{index}]");
                let block = block
                    .as_object()
                    .ok_or_else(|| format!("{block_path} must be an object"))?;
                match required_str(block, "type", &block_path)? {
                    "text" => {
                        let text = field_str(block, "text", &block_path)?;
                        if !text.trim().is_empty() {
                            instructions.push(text.to_owned());
                        }
                    }
                    other => {
                        return Err(format!(
                            "{block_path} has type \"{other}\", which cannot be used in the top-level system field"
                        ));
                    }
                }
            }
            Ok(())
        }
        Value::Null => Ok(()),
        _ => Err("system must be a string or an array of text blocks".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Builder {
    items: Vec<Value>,
    instructions: Vec<String>,
    conversation_started: bool,
    call_ids: BTreeSet<String>,
    result_ids: BTreeSet<String>,
}

impl Builder {
    fn finish(&mut self) -> Result<Vec<Value>, String> {
        if self.items.is_empty() {
            return Err(
                "the messages request has no user or assistant content after conversion".to_owned(),
            );
        }
        for call_id in &self.call_ids {
            if !self.result_ids.contains(call_id) {
                return Err(format!(
                    "tool call {} has no matching tool_result",
                    short(call_id)
                ));
            }
        }
        Ok(std::mem::take(&mut self.items))
    }

    fn push_message(
        &mut self,
        message: &Value,
        index: usize,
        tools: &mut ToolRegistry,
    ) -> Result<(), String> {
        let path = format!("messages[{index}]");
        let message = message
            .as_object()
            .ok_or_else(|| format!("{path} must be an object"))?;
        let role = required_str(message, "role", &path)?;
        let content = message
            .get("content")
            .ok_or_else(|| format!("{path}.content is required"))?;
        match role {
            "system" | "developer" => self.push_system_content(content, &path, tools),
            "user" => {
                self.conversation_started = true;
                self.push_user_content(content, &path, tools)
            }
            "assistant" => {
                self.conversation_started = true;
                self.push_assistant_content(content, &path)
            }
            other => Err(format!(
                "{path}.role \"{other}\" is not supported; use user, assistant or system"
            )),
        }
    }

    fn push_system_content(
        &mut self,
        content: &Value,
        path: &str,
        tools: &mut ToolRegistry,
    ) -> Result<(), String> {
        match content {
            Value::String(text) => {
                self.push_system_text(text);
                Ok(())
            }
            Value::Array(blocks) => {
                for (index, block) in blocks.iter().enumerate() {
                    let block_path = format!("{path}.content[{index}]");
                    let block = block
                        .as_object()
                        .ok_or_else(|| format!("{block_path} must be an object"))?;
                    match required_str(block, "type", &block_path)? {
                        "text" => self.push_system_text(field_str(block, "text", &block_path)?),
                        "tool_addition" => tools.apply_addition(block, &block_path)?,
                        "tool_removal" => tools.apply_removal(block, &block_path)?,
                        other => {
                            return Err(format!(
                                "{block_path} has type \"{other}\", which is not valid in a system message"
                            ));
                        }
                    }
                }
                Ok(())
            }
            Value::Null => Err(format!("{path}.content must not be null")),
            _ => Err(format!("{path}.content must be a string or an array")),
        }
    }

    fn push_system_text(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if self.conversation_started {
            // Responses has a mid-conversation developer role, so an
            // operator-level mid-conversation instruction keeps its position
            // and priority instead of being moved into the top-level prompt.
            self.push_text("developer", text);
        } else {
            self.instructions.push(text.to_owned());
        }
    }

    fn push_user_content(
        &mut self,
        content: &Value,
        path: &str,
        tools: &mut ToolRegistry,
    ) -> Result<(), String> {
        match content {
            Value::String(text) => {
                self.push_text("user", text);
                Ok(())
            }
            Value::Array(blocks) => {
                for (index, block) in blocks.iter().enumerate() {
                    let block_path = format!("{path}.content[{index}]");
                    let block = block
                        .as_object()
                        .ok_or_else(|| format!("{block_path} must be an object"))?;
                    match required_str(block, "type", &block_path)? {
                        "text" => self.push_text("user", field_str(block, "text", &block_path)?),
                        "image" => {
                            let part = image_part(block, &block_path)?;
                            self.push_part("user", part);
                        }
                        "tool_result" => self.push_tool_result(block, &block_path, tools)?,
                        "tool_reference" => {
                            let name = required_str(block, "tool_name", &block_path)?;
                            tools.activate(name);
                            self.push_text("user", &tool_reference_text(name));
                        }
                        "tool_addition" | "tool_removal" => {
                            return Err(format!(
                                "{block_path} is a tool change and must be in a system message"
                            ));
                        }
                        "document" => {
                            return Err(format!(
                                "{block_path} is a document block; only text and image content are converted"
                            ));
                        }
                        "search_result" => {
                            return Err(format!(
                                "{block_path} is a search_result block; server-side search results must be normalized into a tool result"
                            ));
                        }
                        other => {
                            return Err(format!(
                                "{block_path} has unsupported user content type \"{other}\""
                            ));
                        }
                    }
                }
                Ok(())
            }
            Value::Null => Err(format!("{path}.content must not be null")),
            _ => Err(format!("{path}.content must be a string or an array")),
        }
    }

    fn push_assistant_content(&mut self, content: &Value, path: &str) -> Result<(), String> {
        match content {
            Value::String(text) => {
                self.push_text("assistant", text);
                Ok(())
            }
            Value::Array(blocks) => {
                for (index, block) in blocks.iter().enumerate() {
                    let block_path = format!("{path}.content[{index}]");
                    let block = block
                        .as_object()
                        .ok_or_else(|| format!("{block_path} must be an object"))?;
                    match required_str(block, "type", &block_path)? {
                        "text" => {
                            self.push_text("assistant", field_str(block, "text", &block_path)?)
                        }
                        // Provider-bound opaque state. Visible text stays.
                        "thinking" | "redacted_thinking" => {}
                        "tool_use" => self.push_tool_use(block, &block_path)?,
                        "server_tool_use" => {
                            return Err(format!(
                                "{block_path} is an Anthropic server tool call; normalize or remove server-tool history before converting"
                            ));
                        }
                        "advisor_tool_result" => {
                            return Err(format!(
                                "{block_path} carries opaque Anthropic advisor state; remove server-tool history before converting"
                            ));
                        }
                        other => {
                            return Err(format!(
                                "{block_path} has unsupported assistant content type \"{other}\""
                            ));
                        }
                    }
                }
                Ok(())
            }
            Value::Null => Err(format!("{path}.content must not be null")),
            _ => Err(format!("{path}.content must be a string or an array")),
        }
    }

    fn push_text(&mut self, role: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        let kind = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        self.push_part(role, json!({ "type": kind, "text": text }));
    }

    fn push_part(&mut self, role: &str, part: Value) {
        if let Some(last) = self.items.last_mut()
            && last.get("type").and_then(Value::as_str) == Some("message")
            && last.get("role").and_then(Value::as_str) == Some(role)
            && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
        {
            content.push(part);
            return;
        }
        self.items
            .push(json!({ "type": "message", "role": role, "content": [part] }));
    }

    fn push_tool_use(&mut self, block: &Map<String, Value>, path: &str) -> Result<(), String> {
        let call_id = required_str(block, "id", path)?;
        validate_call_id(call_id, path)?;
        let name = required_str(block, "name", path)?;
        validate_tool_name(name, path)?;
        if !self.call_ids.insert(call_id.to_owned()) {
            return Err(format!(
                "{path}.id {} is used by more than one tool_use",
                short(call_id)
            ));
        }
        let input = block
            .get("input")
            .ok_or_else(|| format!("{path}.input is required for a tool_use block"))?;
        let arguments = match input {
            Value::Object(_) => input.to_string(),
            Value::Null => "{}".to_owned(),
            _ => return Err(format!("{path}.input must be a JSON object")),
        };
        self.items.push(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments,
        }));
        Ok(())
    }

    fn push_tool_result(
        &mut self,
        block: &Map<String, Value>,
        path: &str,
        tools: &mut ToolRegistry,
    ) -> Result<(), String> {
        let call_id = required_str(block, "tool_use_id", path)?;
        validate_call_id(call_id, path)?;
        if !self.call_ids.contains(call_id) {
            return Err(format!(
                "{path}.tool_use_id {} does not match an earlier tool_use",
                short(call_id)
            ));
        }
        if !self.result_ids.insert(call_id.to_owned()) {
            return Err(format!(
                "{path}.tool_use_id {} already has a tool_result",
                short(call_id)
            ));
        }
        if let Some(is_error) = block.get("is_error").filter(|value| !value.is_null())
            && !is_error.is_boolean()
        {
            return Err(format!("{path}.is_error must be a boolean"));
        }
        let content = block
            .get("content")
            .ok_or_else(|| format!("{path}.content is required for a tool_result"))?;
        let output = tool_result_output(content, path, tools)?;
        self.items.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

fn image_part(block: &Map<String, Value>, path: &str) -> Result<Value, String> {
    let source = block
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{path}.source must be an object"))?;
    let kind = required_str(source, "type", &format!("{path}.source"))?;
    match kind {
        "base64" => {
            let media_type = required_str(source, "media_type", &format!("{path}.source"))?;
            if !media_type.starts_with("image/") {
                return Err(format!(
                    "{path}.source.media_type must start with image/ for an image block"
                ));
            }
            let data = required_str(source, "data", &format!("{path}.source"))?;
            Ok(json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
            }))
        }
        "url" => {
            let url = required_str(source, "url", &format!("{path}.source"))?;
            Ok(json!({ "type": "input_image", "image_url": url }))
        }
        "file" => Err(format!(
            "{path}.source.type file uses an Anthropic file id, which cannot be replayed to another provider; inline the bytes or use a URL"
        )),
        other => Err(format!(
            "{path}.source.type \"{other}\" is not a supported image source"
        )),
    }
}

fn tool_result_output(
    content: &Value,
    path: &str,
    tools: &mut ToolRegistry,
) -> Result<Value, String> {
    match content {
        Value::String(text) => Ok(json!(text)),
        Value::Null => Ok(json!("")),
        Value::Array(blocks) => {
            let mut text = String::new();
            let mut parts = Vec::new();
            let mut has_image = false;
            for (index, block) in blocks.iter().enumerate() {
                let block_path = format!("{path}.content[{index}]");
                let block = block
                    .as_object()
                    .ok_or_else(|| format!("{block_path} must be an object"))?;
                match required_str(block, "type", &block_path)? {
                    "text" => {
                        let value = field_str(block, "text", &block_path)?;
                        text.push_str(value);
                        parts.push(json!({ "type": "input_text", "text": value }));
                    }
                    "image" => {
                        has_image = true;
                        parts.push(image_part(block, &block_path)?);
                    }
                    "tool_reference" => {
                        let name = required_str(block, "tool_name", &block_path)?;
                        tools.activate(name);
                        let value = tool_reference_text(name);
                        text.push_str(&value);
                        parts.push(json!({ "type": "input_text", "text": value }));
                    }
                    other => {
                        return Err(format!(
                            "{block_path} has unsupported tool_result content type \"{other}\""
                        ));
                    }
                }
            }
            if has_image {
                Ok(Value::Array(parts))
            } else {
                Ok(json!(text))
            }
        }
        _ => Err(format!(
            "{path}.content must be a string or an array of content blocks"
        )),
    }
}

fn tool_reference_text(name: &str) -> String {
    format!("[tool_reference: {name}]")
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

struct ToolEntry {
    name: String,
    definition: Value,
    active: bool,
}

#[derive(Default)]
struct ToolRegistry {
    entries: Vec<ToolEntry>,
}

impl ToolRegistry {
    fn from_request(value: Option<&Value>) -> Result<Self, String> {
        let mut registry = Self::default();
        let tools = match value {
            None | Some(Value::Null) => return Ok(registry),
            Some(Value::Array(tools)) => tools,
            Some(_) => return Err("tools must be an array".to_owned()),
        };
        for (index, tool) in tools.iter().enumerate() {
            let path = format!("tools[{index}]");
            let definition = tool.clone();
            let name = tool_name(&definition, &path)?.to_owned();
            if registry.entries.iter().any(|entry| entry.name == name) {
                return Err(format!(
                    "tool name \"{}\" is declared more than once",
                    short(&name)
                ));
            }
            let deferred = deferred_flag(&definition, &path)?;
            registry.entries.push(ToolEntry {
                name,
                definition,
                active: !deferred,
            });
        }
        Ok(registry)
    }

    fn activate(&mut self, name: &str) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.name == name) {
            entry.active = true;
            true
        } else {
            false
        }
    }

    fn remove(&mut self, name: &str) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.name == name) {
            entry.active = false;
            true
        } else {
            false
        }
    }

    fn is_active(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.name == name && entry.active)
    }

    fn apply_addition(&mut self, block: &Map<String, Value>, path: &str) -> Result<(), String> {
        let tool = block
            .get("tool")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{path}.tool must be an object"))?;
        let kind = required_str(tool, "type", &format!("{path}.tool"))?;
        match kind {
            "tool_definition" => {
                let definition = tool
                    .get("definition")
                    .ok_or_else(|| format!("{path}.tool.definition is required"))?;
                let definition = definition.clone();
                let name = tool_name(&definition, &format!("{path}.tool.definition"))?.to_owned();
                deferred_flag(&definition, &format!("{path}.tool.definition"))?;
                if let Some(entry) = self.entries.iter_mut().find(|entry| entry.name == name) {
                    entry.definition = definition;
                    entry.active = true;
                } else {
                    self.entries.push(ToolEntry {
                        name,
                        definition,
                        active: true,
                    });
                }
                Ok(())
            }
            "tool_reference" => {
                let name = required_str(tool, "name", &format!("{path}.tool"))?;
                if !self.activate(name) {
                    return Err(format!(
                        "{path} surfaces tool \"{}\", which is not declared in tools",
                        short(name)
                    ));
                }
                Ok(())
            }
            "mcp_tool_reference" => {
                if let Ok(name) = mcp_tool_name(tool, &format!("{path}.tool")) {
                    self.activate(&name);
                }
                Ok(())
            }
            "mcp_toolset_reference" => {
                if let Ok(prefix) = mcp_tool_prefix(tool, &format!("{path}.tool")) {
                    for entry in &mut self.entries {
                        if entry.name.starts_with(&prefix) {
                            entry.active = true;
                        }
                    }
                }
                Ok(())
            }
            other => Err(format!(
                "{path}.tool.type \"{other}\" is not a supported tool change"
            )),
        }
    }

    fn apply_removal(&mut self, block: &Map<String, Value>, path: &str) -> Result<(), String> {
        let tool = block
            .get("tool")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{path}.tool must be an object"))?;
        let kind = required_str(tool, "type", &format!("{path}.tool"))?;
        match kind {
            "tool_reference" => {
                let name = required_str(tool, "name", &format!("{path}.tool"))?;
                self.remove(name);
                Ok(())
            }
            "mcp_tool_reference" => {
                if let Ok(name) = mcp_tool_name(tool, &format!("{path}.tool")) {
                    self.remove(&name);
                }
                Ok(())
            }
            "mcp_toolset_reference" => {
                if let Ok(prefix) = mcp_tool_prefix(tool, &format!("{path}.tool")) {
                    for entry in &mut self.entries {
                        if entry.name.starts_with(&prefix) {
                            entry.active = false;
                        }
                    }
                }
                Ok(())
            }
            other => Err(format!(
                "{path}.tool.type \"{other}\" is not a supported tool change"
            )),
        }
    }

    fn into_responses(self) -> Result<Vec<Value>, String> {
        let mut tools = Vec::new();
        for entry in self.entries {
            if !entry.active {
                // Deferred tools that were never surfaced were never offered
                // to Claude either. They are not "dropped"; they stay inactive.
                continue;
            }
            tools.push(tool_to_responses(&entry.definition, &entry.name)?);
        }
        Ok(tools)
    }
}

fn tool_name<'a>(definition: &'a Value, path: &str) -> Result<&'a str, String> {
    let object = definition
        .as_object()
        .ok_or_else(|| format!("{path} must be an object"))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("{path} needs a non-empty name"))?;
    validate_tool_name(name, path)?;
    Ok(name)
}

fn deferred_flag(definition: &Value, path: &str) -> Result<bool, String> {
    let object = definition
        .as_object()
        .ok_or_else(|| format!("{path} must be an object"))?;
    match object.get("defer_loading") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("{path}.defer_loading must be a boolean")),
    }
}

fn tool_to_responses(definition: &Value, name: &str) -> Result<Value, String> {
    let object = definition
        .as_object()
        .ok_or_else(|| format!("tool \"{}\" must be an object", short(name)))?;
    if let Some(kind) = object.get("type").and_then(Value::as_str)
        && kind != "custom"
        && kind != "function"
    {
        return Err(format!(
            "tool \"{}\" is an Anthropic server tool of type \"{kind}\"; normalize or disable it before converting",
            short(name)
        ));
    }
    let schema = object
        .get("input_schema")
        .filter(|value| value.is_object())
        .ok_or_else(|| format!("tool \"{}\" needs an object input_schema", short(name)))?;
    let mut translated = Map::new();
    translated.insert("type".to_owned(), json!("function"));
    translated.insert("name".to_owned(), json!(name));
    if let Some(description) = object.get("description").filter(|value| !value.is_null()) {
        let description = description.as_str().ok_or_else(|| {
            format!(
                "tool \"{}\" has a description that is not a string",
                short(name)
            )
        })?;
        translated.insert("description".to_owned(), json!(description));
    }
    translated.insert("parameters".to_owned(), schema.clone());
    if let Some(strict) = object.get("strict").filter(|value| !value.is_null()) {
        if !strict.is_boolean() {
            return Err(format!("tool \"{}\".strict must be a boolean", short(name)));
        }
        translated.insert("strict".to_owned(), strict.clone());
    }
    Ok(Value::Object(translated))
}

fn mcp_tool_name(tool: &Map<String, Value>, path: &str) -> Result<String, String> {
    let server = required_str(tool, "server_name", path)?;
    let name = required_str(tool, "name", path)?;
    Ok(format!("mcp__{server}__{name}"))
}

fn mcp_tool_prefix(tool: &Map<String, Value>, path: &str) -> Result<String, String> {
    let server = required_str(tool, "server_name", path)?;
    Ok(format!("mcp__{server}__"))
}

fn translate_tool_choice(
    value: Option<&Value>,
    tools: &ToolRegistry,
) -> Result<(Option<Value>, Option<bool>), String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok((None, None));
    };
    let choice = value.as_object().ok_or("tool_choice must be an object")?;
    let kind = required_str(choice, "type", "tool_choice")?;
    let disable_parallel = match choice
        .get("disable_parallel_tool_use")
        .filter(|value| !value.is_null())
    {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("tool_choice.disable_parallel_tool_use must be a boolean".to_owned()),
    };

    let translated = match kind {
        "auto" => {
            if tools.entries.iter().any(|entry| entry.active) {
                Some(json!("auto"))
            } else {
                None
            }
        }
        "none" => Some(json!("none")),
        "any" => {
            if !tools.entries.iter().any(|entry| entry.active) {
                return Err("tool_choice is \"any\" but no tools are active".to_owned());
            }
            Some(json!("required"))
        }
        "tool" => {
            let name = required_str(choice, "name", "tool_choice")?;
            if !tools.is_active(name) {
                return Err(format!(
                    "tool_choice names \"{}\", which is not an active tool",
                    short(name)
                ));
            }
            Some(json!({ "type": "function", "name": name }))
        }
        other => {
            return Err(format!(
                "tool_choice.type \"{other}\" is not supported; use auto, none, any or tool"
            ));
        }
    };
    let parallel = if disable_parallel && tools.entries.iter().any(|entry| entry.active) {
        Some(false)
    } else {
        None
    };
    Ok((translated, parallel))
}

// ---------------------------------------------------------------------------
// Validation and formatting helpers
// ---------------------------------------------------------------------------

fn field_str<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{path}.{field} must be a string"))
}

fn required_str<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{path}.{field} must be a non-empty string"))
}

fn validate_tool_name(name: &str, path: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("{path} has an empty tool name"));
    }
    if name.len() > MAX_TOOL_NAME {
        return Err(format!(
            "{path} has a tool name longer than {MAX_TOOL_NAME} bytes"
        ));
    }
    Ok(())
}

fn validate_call_id(call_id: &str, path: &str) -> Result<(), String> {
    if call_id.is_empty() {
        return Err(format!("{path} has an empty tool call id"));
    }
    if call_id.len() > MAX_TOOL_CALL_ID {
        return Err(format!(
            "{path} has a tool call id longer than {MAX_TOOL_CALL_ID} bytes"
        ));
    }
    Ok(())
}

fn short(value: &str) -> String {
    const LIMIT: usize = 64;
    if value.chars().count() <= LIMIT {
        return value.to_owned();
    }
    let mut truncated: String = value.chars().take(LIMIT).collect();
    truncated.push('…');
    truncated
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stock_request() -> Value {
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 128000,
            "stream": true,
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                {"type": "text", "text": "\nYou are an agent working with the user.", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "list files", "cache_control": {"type": "ephemeral"}}
                ]}
            ],
            "tools": [
                {
                    "name": "Bash",
                    "description": "Run a shell command",
                    "input_schema": {
                        "type": "object",
                        "properties": {"command": {"type": "string"}},
                        "required": ["command"],
                        "additionalProperties": false
                    },
                    "eager_input_streaming": true,
                    "cache_control": {"type": "ephemeral"}
                }
            ],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "metadata": {"user_id": "{\"device_id\":\"d\",\"session_id\":\"s\"}"},
            "thinking": {"type": "adaptive", "display": "updates"},
            "context_management": {"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]},
            "output_config": {"effort": "medium"},
            "betas": ["claude-code-20250219"],
            "cache_control": {"type": "ephemeral"}
        })
    }

    #[test]
    fn converts_a_stock_claude_code_request() {
        let converted = into_responses(&stock_request(), "upstream-model").unwrap();
        assert_eq!(converted["model"], "upstream-model");
        assert_eq!(converted["max_output_tokens"], 128000);
        assert_eq!(converted["store"], false);
        assert_eq!(converted["stream"], true);
        assert_eq!(converted["parallel_tool_calls"], false);
        assert_eq!(converted["tool_choice"], "auto");
        assert_eq!(converted["reasoning"]["effort"], "medium");
        assert_eq!(
            converted["metadata"]["user_id"],
            "{\"device_id\":\"d\",\"session_id\":\"s\"}"
        );
        assert_eq!(
            converted["instructions"],
            "You are Claude Code, Anthropic's official CLI for Claude.\nYou are an agent working with the user."
        );

        let input = converted["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(
            input[0],
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "list files"}]
            })
        );

        let tools = converted["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "Bash");
        assert_eq!(tools[0]["description"], "Run a shell command");
        assert_eq!(
            tools[0]["parameters"],
            stock_request()["tools"][0]["input_schema"]
        );
        assert!(tools[0].get("eager_input_streaming").is_none());
        assert!(tools[0].get("cache_control").is_none());
        assert!(!converted.to_string().contains("signature"));
    }

    #[test]
    fn preserves_tool_schema_exactly() {
        let request = json!({
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "go"}],
            "tools": [{
                "name": "nested",
                "input_schema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {
                        "items": {"type": "array", "items": {"$ref": "#/$defs/item"}}
                    },
                    "$defs": {"item": {"type": "object", "properties": {"n": {"type": "integer"}}}},
                    "required": ["items"],
                    "additionalProperties": false
                }
            }]
        });
        let converted = into_responses(&request, "m").unwrap();
        assert_eq!(
            converted["tools"][0]["parameters"],
            request["tools"][0]["input_schema"]
        );
    }

    #[test]
    fn maps_tool_calls_and_results_with_their_ids() {
        let request = json!({
            "max_tokens": 10,
            "messages": [
                {"role": "user", "content": "run"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "opaque", "signature": "sig"},
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash",
                     "input": {"command": "ls"}, "caller": {"type": "direct"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "Read",
                     "input": {"file_path": "a.rs"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.rs\nb.rs"},
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": [
                        {"type": "text", "text": "fn main() {}"}
                    ], "is_error": false}
                ]}
            ]
        });
        let converted = into_responses(&request, "m").unwrap();
        let input = converted["input"].as_array().unwrap();
        assert_eq!(
            input[1],
            json!({
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "Checking."}]
            })
        );
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "toolu_1");
        assert_eq!(input[2]["name"], "Bash");
        assert_eq!(input[2]["arguments"], "{\"command\":\"ls\"}");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "toolu_2");
        assert_eq!(
            input[4],
            json!({
                "type": "function_call_output", "call_id": "toolu_1", "output": "a.rs\nb.rs"
            })
        );
        assert_eq!(
            input[5],
            json!({
                "type": "function_call_output", "call_id": "toolu_2", "output": "fn main() {}"
            })
        );
        assert!(!converted.to_string().contains("opaque"));
    }

    #[test]
    fn maps_images_in_user_messages_and_tool_results() {
        let request = json!({
            "max_tokens": 10,
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "what is this?"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.test/x.jpg"}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "s.png"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "text", "text": "screenshot"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "BBBB"}}
                    ]}
                ]}
            ]
        });
        let converted = into_responses(&request, "m").unwrap();
        let input = converted["input"].as_array().unwrap();
        assert_eq!(
            input[0]["content"],
            json!([
                {"type": "input_text", "text": "what is this?"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                {"type": "input_image", "image_url": "https://example.test/x.jpg"}
            ])
        );
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(
            input[2]["output"],
            json!([
                {"type": "input_text", "text": "screenshot"},
                {"type": "input_image", "image_url": "data:image/png;base64,BBBB"}
            ])
        );
    }

    #[test]
    fn maps_tool_choice_variants() {
        let base = json!({
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "go"}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object", "properties": {}}}]
        });
        let cases = [
            (json!({"type": "auto"}), json!("auto")),
            (json!({"type": "none"}), json!("none")),
            (json!({"type": "any"}), json!("required")),
            (
                json!({"type": "tool", "name": "Bash"}),
                json!({"type": "function", "name": "Bash"}),
            ),
        ];
        for (choice, expected) in cases {
            let mut request = base.clone();
            request["tool_choice"] = choice;
            let converted = into_responses(&request, "m").unwrap();
            assert_eq!(converted["tool_choice"], expected);
        }

        let mut request = base;
        request["tool_choice"] = json!({"type": "auto", "disable_parallel_tool_use": true});
        let converted = into_responses(&request, "m").unwrap();
        assert_eq!(converted["parallel_tool_calls"], false);
    }

    #[test]
    fn flattens_mid_conversation_tool_additions_and_removals() {
        let request = json!({
            "max_tokens": 10,
            "tools": [
                {"name": "Read", "input_schema": {"type": "object", "properties": {}}},
                {"name": "Deferred", "defer_loading": true,
                 "input_schema": {"type": "object", "properties": {}}}
            ],
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "A dynamic tool is available."},
                    {"type": "tool_addition", "tool": {"type": "tool_definition", "definition": {
                        "name": "mcp__srv__do",
                        "description": "Do something",
                        "input_schema": {"type": "object", "properties": {"x": {"type": "string"}}},
                        "eager_input_streaming": true
                    }}},
                    {"type": "tool_addition", "tool": {"type": "tool_reference", "name": "Deferred"}}
                ]},
                {"role": "user", "content": "go"},
                {"role": "system", "content": [
                    {"type": "text", "text": "The dynamic tool is gone."},
                    {"type": "tool_removal", "tool": {"type": "tool_reference", "name": "Deferred"}}
                ]}
            ],
            "tool_choice": {"type": "tool", "name": "mcp__srv__do"}
        });
        let converted = into_responses(&request, "m").unwrap();
        assert_eq!(converted["instructions"], "A dynamic tool is available.");
        let names = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Read", "mcp__srv__do"]);
        assert_eq!(
            converted["tool_choice"],
            json!({"type": "function", "name": "mcp__srv__do"})
        );
        // The mid-conversation system turn becomes a developer message.
        assert_eq!(converted["input"][1]["role"], "developer");
    }

    #[test]
    fn deferred_server_tool_is_not_offered_until_surfaced() {
        let request = json!({
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "go"}],
            "tools": [
                {"name": "Read", "input_schema": {"type": "object", "properties": {}}},
                {"type": "advisor_20260301", "name": "advisor", "defer_loading": true}
            ]
        });
        let converted = into_responses(&request, "m").unwrap();
        let names = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Read"]);
    }

    #[test]
    fn active_server_tool_is_rejected_not_dropped() {
        let request = json!({
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "go"}],
            "tools": [
                {"name": "Read", "input_schema": {"type": "object", "properties": {}}},
                {"type": "advisor_20260301", "name": "advisor"}
            ]
        });
        let error = into_responses(&request, "m").unwrap_err();
        assert!(error.contains("advisor"));
        assert!(error.contains("server tool"));
    }

    #[test]
    fn maps_tool_references_to_text_and_surfaces_deferred_tool() {
        let request = json!({
            "max_tokens": 10,
            "tools": [
                {"name": "ToolSearch", "input_schema": {"type": "object", "properties": {}}},
                {"name": "WebSearch", "defer_loading": true, "input_schema": {"type": "object", "properties": {}}}
            ],
            "messages": [
                {"role": "user", "content": "search"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "ToolSearch", "input": {"query": "web"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "tool_reference", "tool_name": "WebSearch"}
                    ]}
                ]}
            ]
        });
        let converted = into_responses(&request, "m").unwrap();
        assert_eq!(
            converted["input"][2]["output"],
            "[tool_reference: WebSearch]"
        );
        let names = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["ToolSearch", "WebSearch"]);
    }

    #[test]
    fn preserves_nonstreaming_and_omits_absent_stream() {
        let mut request = json!({
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "go"}]
        });
        let converted = into_responses(&request, "m").unwrap();
        assert!(converted.get("stream").is_none());

        request["stream"] = json!(false);
        let converted = into_responses(&request, "m").unwrap();
        assert_eq!(converted["stream"], false);
    }

    #[test]
    fn maps_sampling_stop_metadata_and_output_format() {
        let request = json!({
            "max_tokens": 10,
            "temperature": 0.5,
            "top_p": 0.9,
            "top_k": 5,
            "stop_sequences": ["STOP", "END"],
            "service_tier": "standard_only",
            "metadata": {"user_id": "u"},
            "output_config": {
                "effort": "high",
                "format": {"type": "json_schema", "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}}}
            },
            "messages": [{"role": "user", "content": "go"}]
        });
        let converted = into_responses(&request, "m").unwrap();
        assert_eq!(converted["temperature"], 0.5);
        assert_eq!(converted["top_p"], 0.9);
        assert!(converted.get("top_k").is_none());
        assert_eq!(converted["stop"], json!(["STOP", "END"]));
        assert_eq!(converted["service_tier"], "default");
        assert_eq!(converted["metadata"], json!({"user_id": "u"}));
        assert_eq!(converted["reasoning"], json!({"effort": "high"}));
        assert_eq!(
            converted["text"]["format"],
            json!({
                "type": "json_schema",
                "name": "response",
                "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}}
            })
        );
        let mut defaults = request;
        defaults["temperature"] = json!(1.0);
        defaults["top_p"] = json!(1.0);
        let converted = into_responses(&defaults, "m").unwrap();
        assert!(converted.get("temperature").is_none());
        assert!(converted.get("top_p").is_none());
    }

    #[test]
    fn concatenates_top_level_and_leading_system_text() {
        let request = json!({
            "max_tokens": 10,
            "system": "Top-level.",
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "Leading."}]},
                {"role": "user", "content": "go"}
            ]
        });
        let converted = into_responses(&request, "m").unwrap();
        assert_eq!(converted["instructions"], "Top-level.Leading.");
        assert_eq!(converted["input"].as_array().unwrap().len(), 1);
        assert_eq!(converted["input"][0]["role"], "user");
    }

    #[test]
    fn rejects_anthropic_file_image_sources() {
        let request = json!({
            "max_tokens": 10,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "file", "file_id": "file_123"}}
            ]}]
        });
        let error = into_responses(&request, "m").unwrap_err();
        assert!(error.contains("file id"));
    }

    #[test]
    fn rejects_malformed_or_unsafe_requests() {
        let base = json!({
            "max_tokens": 10,
            "messages": [{"role": "user", "content": "go"}]
        });

        let mut request = base.clone();
        request["max_tokens"] = json!(0);
        assert!(into_responses(&request, "m").is_err());

        let mut request = base.clone();
        request["mcp_servers"] = json!([{"type": "url", "url": "https://example.test"}]);
        assert!(into_responses(&request, "m").is_err());

        let mut request = base.clone();
        request["messages"] = json!([]);
        assert!(into_responses(&request, "m").is_err());

        let mut request = base.clone();
        request["messages"] = json!([{"role": "user", "content": [{"type": "document", "source": {"type": "text", "data": "x"}}]}]);
        assert!(into_responses(&request, "m").is_err());

        let mut request = base.clone();
        request["messages"] = json!([{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "missing", "content": "x"}
        ]}]);
        assert!(into_responses(&request, "m").is_err());

        let mut request = base;
        request["tools"] = json!([
            {"name": "Same", "input_schema": {"type": "object", "properties": {}}},
            {"name": "Same", "input_schema": {"type": "object", "properties": {}}}
        ]);
        assert!(into_responses(&request, "m").is_err());
    }

    #[test]
    fn rejects_unmatched_tool_calls_and_duplicate_results() {
        let unmatched = json!({
            "max_tokens": 10,
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}
                ]}
            ]
        });
        assert!(into_responses(&unmatched, "m").is_err());

        let duplicate = json!({
            "max_tokens": 10,
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a"}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "b"}
                ]}
            ]
        });
        assert!(into_responses(&duplicate, "m").is_err());
    }
}
