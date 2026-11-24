//! Anthropic Messages adapter for the relay's OpenAI Responses surface.
//!
//! This module is deliberately dependency-free beyond `serde_json` so it can be
//! compiled on its own (see `tests/anthropic_adapter.rs`). Both halves are pure
//! and offline: no I/O and no clocks; the caller owns the response timestamp
//! ([`ResponseTranslator::with_created_at`]). It contains two pieces:
//!
//! * [`translate_request`] turns one Responses API request into one Anthropic
//!   Messages request.
//! * [`ResponseTranslator`] turns already-framed Anthropic SSE events back into
//!   Responses streaming events.
//!
//! # Replay contract
//!
//! Codex replays the whole conversation on every turn. This adapter keeps no
//! server-side conversation state, so:
//!
//! * `previous_response_id`, `conversation`, `background`, `prompt` (prompt
//!   templates) and `store: true` are rejected instead of being silently
//!   ignored.
//! * Every assistant output item must be replayed in its original order:
//!   `reasoning` items (with `encrypted_content`), then `message` items, then
//!   `function_call` items, followed by the matching `function_call_output`
//!   items.
//! * Claude binds signed `thinking` / `redacted_thinking` blocks to the prefix
//!   that produced them. Within a tool-use turn those blocks must be sent back
//!   verbatim. The adapter stores them in the reasoning item's opaque
//!   `encrypted_content` field using a versioned, provider-tagged envelope, and
//!   restores them only when the envelope's model and the turn's tool-call IDs
//!   match the turn being rebuilt.
//! * Do not edit, reorder, or prune assistant items while a tool call is
//!   outstanding, and do not strip `encrypted_content` from reasoning items.
//! * Construct the translator with the configured upstream model
//!   ([`ResponseTranslator::for_model`]). Anthropic may echo a dated snapshot
//!   in `message_start`, while the next request uses the configured alias;
//!   recording the configured name keeps envelope provenance stable. Model
//!   names that differ only by a dated suffix are also treated as compatible.
//!
//! # Prompt caching
//!
//! Anthropic caches only what a request marks, and Codex replays the whole
//! conversation every turn. The translated request therefore carries two
//! `cache_control` breakpoints: one on the system block (the cached prefix is
//! tools, then system) and one on the last cacheable block of the last
//! message. The translation is deterministic, so an unchanged prefix produces
//! the same bytes and the next turn reads it from the cache.
//!
//! # Known gaps in this slice
//!
//! Rejected explicitly: hosted/server tools, custom (freeform) tools, image and
//! file inputs, structured outputs (`text.format`), non-streaming requests,
//! `item_reference`, unknown content blocks, and citations.
//!
//! Current Codex sends its base instructions as a leading `developer` input
//! item rather than a top-level `instructions` string; both forms are mapped
//! to the Anthropic system prompt, and later developer/system messages keep
//! their text and position by becoming user content (Anthropic has no
//! mid-conversation system role). Codex does not send `max_output_tokens`, so
//! [`DEFAULT_MAX_TOKENS`] is used when no cap is present.
//!
//! Accepted but partly honoured: `parallel_tool_calls: false` has no Anthropic
//! equivalent, so Claude may still emit several `tool_use` blocks; the caller
//! must handle multiple `function_call` items. Reasoning `effort` maps to
//! `output_config.effort` (`minimal` becomes `low`, `none` disables thinking);
//! `reasoning.summary` and `text.verbosity` are ignored because Claude decides
//! its own thinking display and output style. `stream_options` and
//! `configuration_update` items are ignored (the former is an OpenAI stream
//! hint, the latter is per-request in Anthropic). Usage sums Anthropic's
//! cache-creation and cache-read input tokens into Responses `input_tokens`,
//! with cache reads in `input_tokens_details.cached_tokens`. A Claude
//! `refusal` stop reason becomes a completed response (any refusal text is a
//! normal text block) because retrying it as OpenAI `content_filter`
//! incomplete would loop. `pause_turn` has no Responses equivalent and is
//! reported as a terminal `response.failed` with code `pause_turn`. Forced
//! tool use (`tool_choice: any`/`tool`) is passed through, but Claude rejects
//! it while thinking is enabled; set `reasoning.effort: "none"` when forcing a
//! tool.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

/// Prefix of the opaque envelope stored in a reasoning item's
/// `encrypted_content` field.
pub const THINKING_ENVELOPE_PREFIX: &str = "codeox.anthropic.thinking.v1:";

const THINKING_ENVELOPE_VERSION: u64 = 1;

/// Anthropic requires `max_tokens`, while current Codex does not send
/// `max_output_tokens`. This universally accepted floor is used when the
/// request does not carry a cap. The relay may inject `max_output_tokens` (for
/// example from a per-model capability table) before calling
/// [`translate_request`] when a higher cap is known to be safe.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

// ---------------------------------------------------------------------------
// Public request translation
// ---------------------------------------------------------------------------

/// Marks the end of the conversation as a prompt-cache breakpoint.
///
/// Anthropic caches nothing unless the request asks for it, and Codex replays
/// the whole conversation on every turn. With this marker (and the one on the
/// system block) each turn reads the previous turn's prefix from the cache
/// instead of paying for, and waiting on, the full history again.
///
/// The marker goes on the last block of the last message that may carry one:
/// thinking blocks and empty text blocks may not.
fn mark_conversation_cache_breakpoint(messages: &mut [Value]) {
    let Some(blocks) = messages
        .last_mut()
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let cacheable = |block: &Value| match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty()),
        Some("tool_use" | "tool_result" | "image" | "document") => true,
        _ => false,
    };
    if let Some(block) = blocks.iter_mut().rev().find(|block| cacheable(block))
        && let Some(block) = block.as_object_mut()
    {
        block.insert("cache_control".to_owned(), json!({"type": "ephemeral"}));
    }
}

/// Translate one OpenAI Responses API request body into an Anthropic Messages
/// request body.
///
/// The upstream model always replaces the model name sent by Codex, matching
/// the relay's configured provider model.
pub fn translate_request(input: &Value, upstream_model: &str) -> Result<Value, String> {
    if upstream_model.trim().is_empty() {
        return Err("upstream model must not be empty".to_owned());
    }
    let request = expect_object(input, "request")?;
    reject_unsupported_request_fields(request)?;

    let max_tokens = match request.get("max_output_tokens") {
        None | Some(Value::Null) => DEFAULT_MAX_TOKENS,
        Some(value) => value
            .as_u64()
            .filter(|tokens| *tokens > 0)
            .ok_or("max_output_tokens must be a positive integer when present")?,
    };

    let mut system_parts = Vec::new();
    if let Some(instructions) = string_field(request, "instructions", "request")?
        && !instructions.trim().is_empty()
    {
        system_parts.push(instructions.to_owned());
    }

    let mut builder = ConversationBuilder::default();
    match request.get("input").ok_or("input is required")? {
        Value::String(text) => builder.push_text("user", text)?,
        Value::Array(items) => {
            if items.is_empty() {
                return Err("input must contain at least one item".to_owned());
            }
            for (index, item) in items.iter().enumerate() {
                translate_input_item(
                    item,
                    &format!("input[{index}]"),
                    &mut builder,
                    &mut system_parts,
                    upstream_model,
                )?;
            }
        }
        Value::Null => return Err("input is required".to_owned()),
        _ => return Err("input must be a string or an array of items".to_owned()),
    }
    builder.flush()?;

    validate_conversation(&builder.messages)?;

    let (tools, tool_names) = translate_tools(request.get("tools"))?;
    let mut translated = Map::new();
    translated.insert("model".to_owned(), json!(upstream_model));
    translated.insert("max_tokens".to_owned(), json!(max_tokens));
    if !system_parts.is_empty() {
        // A block rather than a string, so it can carry the cache breakpoint
        // that covers the tools and the system prompt.
        translated.insert(
            "system".to_owned(),
            json!([{
                "type": "text",
                "text": system_parts.join("\n\n"),
                "cache_control": {"type": "ephemeral"},
            }]),
        );
    }
    let mut messages = std::mem::take(&mut builder.messages);
    mark_conversation_cache_breakpoint(&mut messages);
    translated.insert("messages".to_owned(), Value::Array(messages));
    if !tools.is_empty() {
        translated.insert("tools".to_owned(), Value::Array(tools));
        if let Some(choice) = request.get("tool_choice").filter(|value| !value.is_null())
            && let Some(choice) = translate_tool_choice(choice, &tool_names, true)?
        {
            translated.insert("tool_choice".to_owned(), choice);
        }
    } else if let Some(choice) = request.get("tool_choice").filter(|value| !value.is_null()) {
        // Without tools only "none" and "auto" are meaningful, and both mean
        // "no tools" to Anthropic, so the field is simply omitted.
        let _ = translate_tool_choice(choice, &tool_names, false)?;
    }

    if let Some(temperature) = number_field(request, "temperature", "request")? {
        if !(0.0..=1.0).contains(&temperature) {
            return Err("temperature must be between 0 and 1 for Anthropic".to_owned());
        }
        translated.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = number_field(request, "top_p", "request")? {
        if !(0.0..=1.0).contains(&top_p) {
            return Err("top_p must be between 0 and 1 for Anthropic".to_owned());
        }
        translated.insert("top_p".to_owned(), json!(top_p));
    }
    if let Some(stop) = translate_stop_sequences(request.get("stop"))? {
        translated.insert("stop_sequences".to_owned(), stop);
    }

    let reasoning = translate_reasoning(request.get("reasoning"))?;
    if let Some(effort) = reasoning.effort {
        translated.insert("output_config".to_owned(), json!({ "effort": effort }));
    }
    if reasoning.disabled {
        translated.insert("thinking".to_owned(), json!({ "type": "disabled" }));
    }

    // Codex always streams, and this adapter is streaming-only.
    translated.insert("stream".to_owned(), json!(true));
    Ok(Value::Object(translated))
}

fn reject_unsupported_request_fields(request: &Map<String, Value>) -> Result<(), String> {
    for field in [
        "previous_response_id",
        "conversation",
        "background",
        "prompt",
        "max_tool_calls",
        "seed",
        "response_format",
        "output_config",
    ] {
        if request.get(field).is_some_and(|value| !value.is_null()) {
            return Err(format!(
                "{field} is not supported by the Anthropic adapter; it requires provider-side state or behaviour Anthropic Messages does not provide"
            ));
        }
    }
    if let Some(truncation) = request.get("truncation").filter(|value| !value.is_null())
        && truncation.as_str() != Some("disabled")
    {
        return Err("truncation is a server-side option and is not supported".to_owned());
    }
    if request.get("store").and_then(Value::as_bool) == Some(true) {
        return Err(
            "store=true requires server-side response state; the Anthropic adapter is stateless, send store=false"
                .to_owned(),
        );
    }
    if request.get("stream").and_then(Value::as_bool) != Some(true) {
        return Err("this adapter only supports streaming requests; send stream=true".to_owned());
    }
    if let Some(tier) = request.get("service_tier").filter(|value| !value.is_null()) {
        let tier = tier.as_str().ok_or("service_tier must be a string")?;
        if !matches!(tier, "auto" | "default") {
            return Err(format!(
                "service_tier \"{tier}\" is not supported; Anthropic Messages has no service-tier selection"
            ));
        }
    }
    if let Some(value) = request
        .get("parallel_tool_calls")
        .filter(|value| !value.is_null())
        && !value.is_boolean()
    {
        return Err("parallel_tool_calls must be a boolean".to_owned());
    }
    for field in ["frequency_penalty", "presence_penalty"] {
        if request
            .get(field)
            .is_some_and(|value| value.as_f64().unwrap_or(0.0) != 0.0)
        {
            return Err(format!("{field} is not supported by Anthropic Messages"));
        }
    }
    if let Some(text) = request.get("text").filter(|value| !value.is_null()) {
        let text = expect_object(text, "text")?;
        if text.get("format").is_some_and(|value| !value.is_null()) {
            return Err("structured outputs (text.format) are not supported".to_owned());
        }
    }
    if let Some(include) = request.get("include").filter(|value| !value.is_null()) {
        let include = include
            .as_array()
            .ok_or("include must be an array of strings")?;
        for entry in include {
            let entry = entry
                .as_str()
                .ok_or("include must be an array of strings")?;
            if entry != "reasoning.encrypted_content" {
                return Err(format!("include \"{entry}\" is not supported"));
            }
        }
    }
    Ok(())
}

struct ReasoningConfig {
    effort: Option<&'static str>,
    disabled: bool,
}

fn translate_reasoning(value: Option<&Value>) -> Result<ReasoningConfig, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(ReasoningConfig {
            effort: None,
            disabled: false,
        });
    };
    let reasoning = expect_object(value, "reasoning")?;
    let Some(effort) = reasoning.get("effort").filter(|value| !value.is_null()) else {
        return Ok(ReasoningConfig {
            effort: None,
            disabled: false,
        });
    };
    let effort = effort.as_str().ok_or("reasoning.effort must be a string")?;
    match effort {
        "none" => Ok(ReasoningConfig {
            effort: None,
            disabled: true,
        }),
        "minimal" => Ok(ReasoningConfig {
            effort: Some("low"),
            disabled: false,
        }),
        "low" => Ok(ReasoningConfig {
            effort: Some("low"),
            disabled: false,
        }),
        "medium" => Ok(ReasoningConfig {
            effort: Some("medium"),
            disabled: false,
        }),
        "high" => Ok(ReasoningConfig {
            effort: Some("high"),
            disabled: false,
        }),
        "xhigh" => Ok(ReasoningConfig {
            effort: Some("xhigh"),
            disabled: false,
        }),
        "max" => Ok(ReasoningConfig {
            effort: Some("max"),
            disabled: false,
        }),
        other => Err(format!(
            "reasoning.effort \"{other}\" is not supported; use none, low, medium, high, xhigh or max"
        )),
    }
}

fn translate_stop_sequences(value: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match value {
        Value::String(sequence) => Ok(Some(json!([sequence]))),
        Value::Array(sequences) => {
            let mut translated = Vec::with_capacity(sequences.len());
            for sequence in sequences {
                let sequence = sequence.as_str().ok_or("stop entries must be strings")?;
                if sequence.is_empty() {
                    return Err("stop entries must not be empty".to_owned());
                }
                translated.push(json!(sequence));
            }
            if translated.is_empty() {
                Ok(None)
            } else {
                Ok(Some(Value::Array(translated)))
            }
        }
        _ => Err("stop must be a string or an array of strings".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Input items
// ---------------------------------------------------------------------------

fn translate_input_item(
    item: &Value,
    path: &str,
    builder: &mut ConversationBuilder,
    system_parts: &mut Vec<String>,
    upstream_model: &str,
) -> Result<(), String> {
    let item = expect_object(item, path)?;
    match string_field(item, "type", path)? {
        None => {
            let role = required_string(item, "role", path)?;
            translate_message_item(item, role, path, builder, system_parts)
        }
        Some("message") => {
            let role = required_string(item, "role", path)?;
            translate_message_item(item, role, path, builder, system_parts)
        }
        Some("function_call") => {
            let call_id = required_string(item, "call_id", path)?;
            if call_id.is_empty() {
                return Err(format!("{path}.call_id must not be empty"));
            }
            let name = required_string(item, "name", path)?;
            validate_tool_name(name, path)?;
            if item.get("namespace").is_some_and(|value| !value.is_null()) {
                return Err(format!(
                    "{path}.namespace is not supported; Anthropic tools are flat names"
                ));
            }
            let raw_arguments = string_field(item, "arguments", path)?.unwrap_or("{}");
            let raw_arguments = if raw_arguments.trim().is_empty() {
                "{}"
            } else {
                raw_arguments
            };
            let arguments: Value = serde_json::from_str(raw_arguments)
                .map_err(|error| format!("{path}.arguments is not valid JSON: {error}"))?;
            if !arguments.is_object() {
                return Err(format!("{path}.arguments must encode a JSON object"));
            }
            builder.push_tool(
                json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": arguments,
                }),
                call_id.to_owned(),
            )
        }
        Some("function_call_output") => {
            let call_id = required_string(item, "call_id", path)?;
            if call_id.is_empty() {
                return Err(format!("{path}.call_id must not be empty"));
            }
            if item.get("namespace").is_some_and(|value| !value.is_null()) {
                return Err(format!(
                    "{path}.namespace is not supported; Anthropic tools are flat names"
                ));
            }
            let output = item.get("output").ok_or_else(|| {
                format!("{path}.output is required for a function_call_output item")
            })?;
            let content = tool_result_content(output, path)?;
            builder.push_block(
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": content,
                }),
            )
        }
        Some("reasoning") => {
            if let Some((blocks, meta)) = decode_reasoning_item(item, upstream_model, path)? {
                builder.push_reasoning(blocks, meta)?;
            }
            Ok(())
        }
        Some("configuration_update") => {
            // Anthropic reasoning effort is per-request, not a durable history
            // item. The top-level `reasoning` field is authoritative.
            Ok(())
        }
        Some("additional_tools") => Err(format!(
            "{path} adds tools mid-conversation; Anthropic tool declarations are per-request"
        )),
        Some("compaction" | "context_compaction" | "compaction_trigger") => Err(format!(
            "{path} is provider-side compaction state; Anthropic compaction must happen client-side"
        )),
        Some("agent_message") => Err(format!(
            "{path} is a multi-agent message, which this adapter slice does not support"
        )),
        Some("item_reference") => Err(format!(
            "{path} is an item_reference, which needs server-side response state; replay the full conversation instead"
        )),
        Some("custom_tool_call" | "custom_tool_call_output") => Err(format!(
            "{path} uses a custom (freeform) tool, which this adapter slice does not support"
        )),
        Some(kind) if is_hosted_item_type(kind) => Err(format!(
            "{path} uses hosted tool type \"{kind}\"; only text and client function tools are supported"
        )),
        Some(kind) => Err(format!("unsupported input item type \"{kind}\" at {path}")),
    }
}

fn translate_message_item(
    item: &Map<String, Value>,
    role: &str,
    path: &str,
    builder: &mut ConversationBuilder,
    system_parts: &mut Vec<String>,
) -> Result<(), String> {
    let content = item
        .get("content")
        .ok_or_else(|| format!("{path}.content is required"))?;
    match role {
        "system" | "developer" => {
            let texts = content_texts(content, path)?;
            if builder.has_content() {
                // Anthropic has no mid-conversation system role. Keep the text
                // and its position by emitting it as user content instead of
                // silently moving it to the top of the system prompt.
                for text in texts {
                    if !text.is_empty() {
                        builder.push_block("user", json!({ "type": "text", "text": text }))?;
                    }
                }
            } else {
                for text in texts {
                    if !text.trim().is_empty() {
                        system_parts.push(text);
                    }
                }
            }
            Ok(())
        }
        "user" => {
            for block in content_blocks(content, path)? {
                builder.push_block("user", block)?;
            }
            Ok(())
        }
        "assistant" => {
            for block in content_blocks(content, path)? {
                builder.push_block("assistant", block)?;
            }
            Ok(())
        }
        other => Err(format!(
            "{path}.role \"{other}\" is not supported; use user, assistant, system or developer"
        )),
    }
}

fn content_texts(content: &Value, path: &str) -> Result<Vec<String>, String> {
    match content {
        Value::String(text) => Ok(vec![text.clone()]),
        Value::Array(parts) => {
            let mut texts = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                let part_path = format!("{path}.content[{index}]");
                let part = expect_object(part, &part_path)?;
                match required_string(part, "type", &part_path)? {
                    "input_text" | "output_text" | "text" => {
                        texts.push(required_string(part, "text", &part_path)?.to_owned());
                    }
                    "refusal" => {
                        texts.push(required_string(part, "refusal", &part_path)?.to_owned());
                    }
                    "reasoning_text" | "summary_text" => {}
                    other => {
                        return Err(format!(
                            "{part_path} has content type \"{other}\", which cannot be used in a system message"
                        ));
                    }
                }
            }
            Ok(texts)
        }
        Value::Null => Err(format!("{path}.content must not be null")),
        _ => Err(format!("{path}.content must be a string or an array")),
    }
}

fn content_blocks(content: &Value, path: &str) -> Result<Vec<Value>, String> {
    match content {
        Value::String(text) => Ok(text_blocks(text)),
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                let part_path = format!("{path}.content[{index}]");
                let part = expect_object(part, &part_path)?;
                match required_string(part, "type", &part_path)? {
                    "input_text" | "output_text" | "text" => {
                        blocks.extend(text_blocks(required_string(part, "text", &part_path)?));
                    }
                    "refusal" => {
                        blocks.extend(text_blocks(required_string(part, "refusal", &part_path)?));
                    }
                    "input_image" | "input_file" | "output_audio" | "audio" => {
                        return Err(format!(
                            "{part_path} has content type \"{}\", which this adapter slice does not support",
                            required_string(part, "type", &part_path)?
                        ));
                    }
                    "reasoning_text" | "summary_text" => {}
                    other => {
                        return Err(format!(
                            "unsupported content part type \"{other}\" at {part_path}"
                        ));
                    }
                }
            }
            Ok(blocks)
        }
        Value::Null => Err(format!("{path}.content must not be null")),
        _ => Err(format!("{path}.content must be a string or an array")),
    }
}

fn text_blocks(text: &str) -> Vec<Value> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "text", "text": text })]
    }
}

fn tool_result_content(output: &Value, path: &str) -> Result<Value, String> {
    match output {
        Value::String(text) => Ok(json!(text)),
        Value::Null => Ok(json!("")),
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                let part_path = format!("{path}.output[{index}]");
                if part.is_string() {
                    blocks.extend(text_blocks(part.as_str().unwrap_or_default()));
                    continue;
                }
                let part = expect_object(part, &part_path)?;
                match required_string(part, "type", &part_path)? {
                    "input_text" | "output_text" | "text" => {
                        blocks.extend(text_blocks(required_string(part, "text", &part_path)?));
                    }
                    "input_image" => {
                        return Err(format!(
                            "{part_path} is an image, which this adapter slice does not support in tool results"
                        ));
                    }
                    other => {
                        return Err(format!(
                            "unsupported tool result content type \"{other}\" at {part_path}"
                        ));
                    }
                }
            }
            if blocks.is_empty() {
                Ok(json!(""))
            } else {
                Ok(Value::Array(blocks))
            }
        }
        other => Ok(json!(other.to_string())),
    }
}

fn is_hosted_item_type(kind: &str) -> bool {
    matches!(
        kind,
        "web_search_call"
            | "file_search_call"
            | "computer_call"
            | "computer_call_output"
            | "code_interpreter_call"
            | "image_generation_call"
            | "mcp_call"
            | "mcp_list_tools"
            | "mcp_approval_request"
            | "mcp_approval_response"
            | "local_shell_call"
            | "local_shell_call_output"
            | "shell_call"
            | "shell_call_output"
            | "apply_patch_call"
            | "apply_patch_call_output"
            | "tool_search_call"
            | "tool_search_output"
    )
}

// ---------------------------------------------------------------------------
// Conversation assembly and validation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ConversationBuilder {
    messages: Vec<Value>,
    role: Option<&'static str>,
    blocks: Vec<Value>,
    envelopes: Vec<EnvelopeMeta>,
    tool_ids: Vec<String>,
}

struct EnvelopeMeta {
    index: usize,
    count: usize,
    call_ids: Vec<String>,
}

impl ConversationBuilder {
    fn has_content(&self) -> bool {
        !self.messages.is_empty() || self.role.is_some() || !self.blocks.is_empty()
    }

    fn push_text(&mut self, role: &'static str, text: &str) -> Result<(), String> {
        for block in text_blocks(text) {
            self.push_block(role, block)?;
        }
        Ok(())
    }

    fn push_block(&mut self, role: &'static str, block: Value) -> Result<(), String> {
        if self.role != Some(role) {
            self.flush()?;
            self.role = Some(role);
        }
        self.blocks.push(block);
        Ok(())
    }

    fn push_tool(&mut self, block: Value, call_id: String) -> Result<(), String> {
        self.push_block("assistant", block)?;
        self.tool_ids.push(call_id);
        Ok(())
    }

    fn push_reasoning(&mut self, blocks: Vec<Value>, meta: EnvelopeMeta) -> Result<(), String> {
        for block in blocks {
            self.push_block("assistant", block)?;
        }
        self.envelopes.push(meta);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        let Some(role) = self.role.take() else {
            return Ok(());
        };
        if self.blocks.is_empty() {
            self.envelopes.clear();
            self.tool_ids.clear();
            return Ok(());
        }
        if role == "assistant" && !self.envelopes.is_empty() {
            let count = self.envelopes.len();
            for (position, meta) in self.envelopes.iter().enumerate() {
                if meta.index != position || meta.count != count {
                    return Err(
                        "reasoning items for this assistant turn are incomplete or out of order; replay every reasoning item from the turn with its encrypted_content"
                            .to_owned(),
                    );
                }
                if meta.call_ids != self.tool_ids {
                    return Err(
                        "reasoning item provenance does not match the tool calls in this assistant turn; do not edit or reorder assistant items while a tool call is outstanding"
                            .to_owned(),
                    );
                }
            }
        }
        if role == "user" {
            // Anthropic requires tool_result blocks before any other content in
            // the user message that answers a tool call.
            let mut results = Vec::new();
            let mut others = Vec::new();
            for block in self.blocks.drain(..) {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    results.push(block);
                } else {
                    others.push(block);
                }
            }
            results.extend(others);
            self.blocks = results;
        }
        self.messages.push(json!({
            "role": role,
            "content": std::mem::take(&mut self.blocks),
        }));
        self.envelopes.clear();
        self.tool_ids.clear();
        Ok(())
    }
}

fn validate_conversation(messages: &[Value]) -> Result<(), String> {
    let Some(first) = messages.first() else {
        return Err("input did not contain any user or assistant content".to_owned());
    };
    if first.get("role").and_then(Value::as_str) != Some("user") {
        return Err("the first Anthropic message must have role user".to_owned());
    }
    for (index, message) in messages.iter().enumerate() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let blocks = message
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        match role {
            "assistant" => {
                let tool_ids = tool_use_ids(blocks);
                if !tool_ids.is_empty() {
                    let next = messages.get(index + 1).ok_or_else(|| {
                        format!(
                            "tool call {} has no matching function_call_output",
                            tool_ids[0]
                        )
                    })?;
                    if next.get("role").and_then(Value::as_str) != Some("user") {
                        return Err(format!(
                            "tool call {} must be followed by a user message containing its function_call_output",
                            tool_ids[0]
                        ));
                    }
                    let results = next
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|blocks| tool_result_ids(blocks))
                        .unwrap_or_default();
                    for id in &tool_ids {
                        if !results.iter().any(|result| result == id) {
                            return Err(format!(
                                "tool call {id} is missing its function_call_output"
                            ));
                        }
                    }
                    for result in &results {
                        if !tool_ids.iter().any(|id| id == result) {
                            return Err(format!(
                                "tool_result {result} does not match a tool_use in the preceding assistant turn"
                            ));
                        }
                    }
                }
            }
            "user" => {
                let results = tool_result_ids(blocks);
                if results.is_empty() {
                    continue;
                }
                let previous_ids = index
                    .checked_sub(1)
                    .and_then(|previous| messages.get(previous))
                    .filter(|previous| {
                        previous.get("role").and_then(Value::as_str) == Some("assistant")
                    })
                    .and_then(|previous| previous.get("content"))
                    .and_then(Value::as_array)
                    .map(|blocks| tool_use_ids(blocks))
                    .unwrap_or_default();
                for result in &results {
                    if !previous_ids.iter().any(|id| id == result) {
                        return Err(format!(
                            "tool_result {result} has no preceding tool_use; replay the function_call item before its output"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn tool_use_ids(blocks: &[Value]) -> Vec<String> {
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| block.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn tool_result_ids(blocks: &[Value]) -> Vec<String> {
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------------------
// Thinking envelope
// ---------------------------------------------------------------------------

fn encode_envelope(envelope: &Value) -> String {
    format!(
        "{THINKING_ENVELOPE_PREFIX}{}",
        hex_encode(envelope.to_string().as_bytes())
    )
}

fn decode_envelope(value: &str) -> Result<Option<Value>, String> {
    let Some(encoded) = value.strip_prefix(THINKING_ENVELOPE_PREFIX) else {
        return Ok(None);
    };
    let bytes = hex_decode(encoded)
        .ok_or("malformed thinking envelope: encrypted_content is not valid hex")?;
    let envelope = serde_json::from_slice(&bytes)
        .map_err(|error| format!("malformed thinking envelope: {error}"))?;
    Ok(Some(envelope))
}

fn decode_reasoning_item(
    item: &Map<String, Value>,
    upstream_model: &str,
    path: &str,
) -> Result<Option<(Vec<Value>, EnvelopeMeta)>, String> {
    let Some(raw) = item.get("encrypted_content").and_then(Value::as_str) else {
        // Reasoning from another provider, or a display-only reasoning item.
        return Ok(None);
    };
    let Some(envelope) = decode_envelope(raw)? else {
        return Ok(None);
    };
    let envelope = expect_object(&envelope, "thinking envelope")?;
    if envelope.get("v").and_then(Value::as_u64) != Some(THINKING_ENVELOPE_VERSION) {
        return Err(format!(
            "{path}.encrypted_content uses an unsupported thinking envelope version"
        ));
    }
    if envelope.get("provider").and_then(Value::as_str) != Some("anthropic") {
        return Err(format!(
            "{path}.encrypted_content is not an Anthropic thinking envelope"
        ));
    }
    let model = required_string(envelope, "model", "thinking envelope")?;
    if !models_compatible(model, upstream_model) {
        // A model switch is not a request error, but Claude will not accept
        // another model's signed thinking. Drop it rather than send bad
        // provenance; a mid-turn model switch may still be rejected upstream.
        return Ok(None);
    }
    let index = envelope
        .get("reasoning_index")
        .and_then(Value::as_u64)
        .ok_or("thinking envelope is missing reasoning_index")? as usize;
    let count = envelope
        .get("reasoning_count")
        .and_then(Value::as_u64)
        .ok_or("thinking envelope is missing reasoning_count")? as usize;
    if count == 0 || index >= count {
        return Err("thinking envelope has an invalid reasoning_index".to_owned());
    }
    let call_ids = envelope
        .get("call_ids")
        .and_then(Value::as_array)
        .ok_or("thinking envelope is missing call_ids")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or("thinking envelope call_ids must be strings".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let blocks = envelope
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or("thinking envelope is missing blocks")?;
    let mut translated = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.iter().enumerate() {
        let block_path = format!("{path}.encrypted_content.blocks[{index}]");
        let block = expect_object(block, &block_path)?;
        match required_string(block, "type", &block_path)? {
            "thinking" => {
                translated.push(json!({
                    "type": "thinking",
                    "thinking": required_string(block, "thinking", &block_path)?,
                    "signature": required_string(block, "signature", &block_path)?,
                }));
            }
            "redacted_thinking" => {
                translated.push(json!({
                    "type": "redacted_thinking",
                    "data": required_string(block, "data", &block_path)?,
                }));
            }
            other => {
                return Err(format!(
                    "unsupported thinking block type \"{other}\" at {block_path}"
                ));
            }
        }
    }
    if translated.is_empty() {
        return Ok(None);
    }
    Ok(Some((
        translated,
        EnvelopeMeta {
            index,
            count,
            call_ids,
        },
    )))
}

fn models_compatible(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long.strip_prefix(short)
        .is_some_and(|suffix| suffix.starts_with('-'))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        decoded.push(((high << 4) | low) as u8);
    }
    Some(decoded)
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn translate_tools(value: Option<&Value>) -> Result<(Vec<Value>, BTreeSet<String>), String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok((Vec::new(), BTreeSet::new()));
    };
    let tools = value.as_array().ok_or("tools must be an array")?;
    let mut translated = Vec::with_capacity(tools.len());
    let mut names = BTreeSet::new();
    for (index, tool) in tools.iter().enumerate() {
        let path = format!("tools[{index}]");
        let tool = expect_object(tool, &path)?;
        let kind = required_string(tool, "type", &path)?;
        if kind != "function" {
            return Err(format!(
                "{path}.type \"{kind}\" is not supported; only client function tools are translated (hosted tools need provider-side execution)"
            ));
        }
        let name = required_string(tool, "name", &path)?;
        validate_tool_name(name, &path)?;
        if !names.insert(name.to_owned()) {
            return Err(format!("duplicate tool name \"{name}\""));
        }
        let mut entry = Map::new();
        entry.insert("name".to_owned(), json!(name));
        if let Some(description) = string_field(tool, "description", &path)? {
            entry.insert("description".to_owned(), json!(description));
        }
        let parameters = match tool.get("parameters") {
            None | Some(Value::Null) => json!({ "type": "object", "properties": {} }),
            Some(value) if value.is_object() => value.clone(),
            Some(_) => return Err(format!("{path}.parameters must be a JSON object")),
        };
        entry.insert("input_schema".to_owned(), parameters);
        if let Some(strict) = bool_field(tool, "strict", &path)? {
            entry.insert("strict".to_owned(), json!(strict));
        }
        translated.push(Value::Object(entry));
    }
    Ok((translated, names))
}

fn translate_tool_choice(
    value: &Value,
    tool_names: &BTreeSet<String>,
    has_tools: bool,
) -> Result<Option<Value>, String> {
    match value {
        Value::String(choice) => match choice.as_str() {
            "auto" => Ok(has_tools.then(|| json!({ "type": "auto" }))),
            "none" => Ok(has_tools.then(|| json!({ "type": "none" }))),
            "required" => {
                if !has_tools {
                    Err("tool_choice \"required\" needs at least one function tool".to_owned())
                } else {
                    Ok(Some(json!({ "type": "any" })))
                }
            }
            other => Err(format!("unsupported tool_choice \"{other}\"")),
        },
        Value::Object(choice) => {
            let kind = required_string(choice, "type", "tool_choice")?;
            match kind {
                "function" => {
                    let name = required_string(choice, "name", "tool_choice")?;
                    if !tool_names.contains(name) {
                        return Err(format!("tool_choice names unknown tool \"{name}\""));
                    }
                    Ok(Some(json!({ "type": "tool", "name": name })))
                }
                "allowed_tools" | "mcp" | "custom" | "types" => {
                    Err(format!("tool_choice type \"{kind}\" is not supported"))
                }
                other => Err(format!("unsupported tool_choice type \"{other}\"")),
            }
        }
        _ => Err("tool_choice must be a string or an object".to_owned()),
    }
}

fn validate_tool_name(name: &str, path: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 128 {
        return Err(format!("{path}.name must be 1 to 128 characters long"));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(format!(
            "{path}.name \"{name}\" must contain only letters, digits, _ or - (Anthropic tool-name rules)"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming response translation
// ---------------------------------------------------------------------------

/// Translates one Anthropic Messages SSE stream into Responses streaming
/// events. Feed every framed event to [`push_event`](Self::push_event) and call
/// [`finish`](Self::finish) when the upstream body ends.
///
/// Deltas are forwarded live, but every `response.output_item.done` is emitted
/// in `output_index` order when `message_stop` arrives. Codex records history in
/// done-event order, and Claude requires the replayed assistant turn to keep
/// the original thinking/text/tool_use order.
#[derive(Debug)]
pub struct ResponseTranslator {
    created_at: u64,
    sequence: u64,
    envelope_model: Option<String>,
    state: Option<StreamState>,
    terminal: bool,
}

impl Default for ResponseTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseTranslator {
    /// Constructs a translator with a zero `created_at`. Both halves of this
    /// module are pure; the caller owns real timestamps.
    pub fn new() -> Self {
        Self::with_created_at(0)
    }

    /// Constructor for callers that own the response timestamp.
    pub fn with_created_at(created_at: u64) -> Self {
        Self {
            created_at,
            sequence: 0,
            envelope_model: None,
            state: None,
            terminal: false,
        }
    }

    /// Constructor for the relay: records `upstream_model`, the name used in
    /// the next request, in thinking envelopes instead of the dated snapshot
    /// Anthropic may echo in `message_start`.
    #[allow(dead_code)] // convenience constructors used by the adapter contract test
    pub fn for_model(model: impl Into<String>) -> Self {
        let mut translator = Self::new();
        translator.envelope_model = Some(model.into());
        translator
    }

    /// Deterministic variant of [`for_model`](Self::for_model).
    #[allow(dead_code)]
    pub fn with_created_at_and_model(created_at: u64, model: impl Into<String>) -> Self {
        let mut translator = Self::for_model(model);
        translator.created_at = created_at;
        translator
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    #[allow(dead_code)]
    pub fn response_id(&self) -> Option<&str> {
        self.state.as_ref().map(|state| state.response_id.as_str())
    }

    /// Push one already-framed SSE event. Returns zero or more Responses event
    /// objects in emission order.
    pub fn push_event(&mut self, event: &str, data: &str) -> Result<Vec<Value>, String> {
        if self.terminal {
            return Ok(Vec::new());
        }
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            return self.finish();
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|error| format!("invalid SSE data for event {event:?}: {error}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| format!("SSE data for event {event:?} must be a JSON object"))?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .unwrap_or(event);
        match kind {
            "ping" => Ok(Vec::new()),
            "message_start" => self.message_start(object),
            "content_block_start" => self.content_block_start(object),
            "content_block_delta" => self.content_block_delta(object),
            "content_block_stop" => self.content_block_stop(object),
            "message_delta" => self.message_delta(object),
            "message_stop" => self.message_stop(),
            "error" => self.error_event(object),
            // Anthropic asks clients to ignore unknown event types.
            _ => Ok(Vec::new()),
        }
    }

    /// Mark an abruptly ended upstream stream as a terminal failure. Safe to
    /// call after a terminal event; it then returns no events.
    pub fn finish(&mut self) -> Result<Vec<Value>, String> {
        if self.terminal {
            return Ok(Vec::new());
        }
        if self.state.is_none() {
            return Err("Anthropic stream ended before message_start".to_owned());
        }
        let event = self.failed_event(
            "stream_interrupted",
            "Anthropic stream ended before message_stop",
        );
        self.terminal = true;
        Ok(vec![event])
    }

    fn next_sequence(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence += 1;
        sequence
    }

    fn state_mut(&mut self) -> Result<&mut StreamState, String> {
        self.state
            .as_mut()
            .ok_or_else(|| "received an Anthropic event before message_start".to_owned())
    }

    fn message_start(&mut self, data: &Map<String, Value>) -> Result<Vec<Value>, String> {
        if self.state.is_some() {
            return Err("received a second message_start".to_owned());
        }
        let message = expect_object(
            data.get("message")
                .ok_or("message_start.message is required")?,
            "message_start.message",
        )?;
        let id = required_string(message, "id", "message_start.message")?;
        let model = required_string(message, "model", "message_start.message")?;
        let response_id = if id.starts_with("resp_") {
            id.to_owned()
        } else {
            format!("resp_{id}")
        };
        let mut state = StreamState::new(response_id, model.to_owned());
        if let Some(usage) = message.get("usage").filter(|value| !value.is_null()) {
            absorb_usage(&mut state, usage);
        }
        self.state = Some(state);

        Ok(vec![
            self.response_event(
                "response.created",
                "in_progress",
                json!([]),
                Value::Null,
                None,
                None,
            ),
            self.response_event(
                "response.in_progress",
                "in_progress",
                json!([]),
                Value::Null,
                None,
                None,
            ),
        ])
    }

    fn content_block_start(&mut self, data: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let index = content_index(data)?;
        let content_block = expect_object(
            data.get("content_block")
                .ok_or("content_block_start.content_block is required")?,
            "content_block_start.content_block",
        )?;
        let kind = required_string(content_block, "type", "content_block_start.content_block")?;
        {
            let state = self.state_mut()?;
            if state.blocks.contains_key(&index) || state.completed.contains_key(&index) {
                return Err(format!("content block index {index} was started twice"));
            }
        }

        match kind {
            "text" => {
                let initial =
                    string_field(content_block, "text", "content_block_start.content_block")?
                        .unwrap_or_default();
                let item_id = format!("msg_{index}");
                self.state_mut()?.blocks.insert(
                    index,
                    BlockState {
                        kind: BlockKind::Text {
                            text: initial.to_owned(),
                            item_id: item_id.clone(),
                        },
                        closed: false,
                    },
                );
                let mut events = Vec::with_capacity(3);
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.output_item.added",
                    "sequence_number": sequence,
                    "output_index": index,
                    "item": {
                        "id": item_id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": [],
                    },
                }));
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.content_part.added",
                    "sequence_number": sequence,
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] },
                }));
                if !initial.is_empty() {
                    let sequence = self.next_sequence();
                    events.push(json!({
                        "type": "response.output_text.delta",
                        "sequence_number": sequence,
                        "item_id": item_id,
                        "output_index": index,
                        "content_index": 0,
                        "delta": initial,
                    }));
                }
                Ok(events)
            }
            "tool_use" => {
                let call_id =
                    required_string(content_block, "id", "content_block_start.content_block")?;
                if call_id.is_empty() {
                    return Err("tool_use block has an empty id".to_owned());
                }
                let name =
                    required_string(content_block, "name", "content_block_start.content_block")?;
                validate_tool_name(name, "content_block_start.content_block")?;
                {
                    let state = self.state_mut()?;
                    if state.blocks.values().any(|block| {
                        matches!(&block.kind, BlockKind::Tool { call_id: existing, .. } if existing == call_id)
                    }) {
                        return Err(format!("duplicate tool_use id \"{call_id}\""));
                    }
                }
                let initial = match content_block.get("input") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::Object(input)) if input.is_empty() => String::new(),
                    Some(Value::Object(_)) => content_block["input"].to_string(),
                    Some(_) => {
                        return Err("tool_use content_block.input must be a JSON object".to_owned());
                    }
                };
                let item_id = format!("fc_{call_id}");
                self.state_mut()?.blocks.insert(
                    index,
                    BlockState {
                        kind: BlockKind::Tool {
                            call_id: call_id.to_owned(),
                            name: name.to_owned(),
                            arguments: initial.clone(),
                            item_id: item_id.clone(),
                        },
                        closed: false,
                    },
                );
                let mut events = Vec::with_capacity(2);
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.output_item.added",
                    "sequence_number": sequence,
                    "output_index": index,
                    "item": {
                        "id": item_id,
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": "",
                    },
                }));
                if !initial.is_empty() {
                    let sequence = self.next_sequence();
                    events.push(json!({
                        "type": "response.function_call_arguments.delta",
                        "sequence_number": sequence,
                        "item_id": item_id,
                        "output_index": index,
                        "delta": initial,
                    }));
                }
                Ok(events)
            }
            "thinking" => {
                let initial = string_field(
                    content_block,
                    "thinking",
                    "content_block_start.content_block",
                )?
                .unwrap_or_default();
                let item_id = format!("rs_{index}");
                let has_initial = !initial.is_empty();
                self.state_mut()?.blocks.insert(
                    index,
                    BlockState {
                        kind: BlockKind::Thinking {
                            text: initial.to_owned(),
                            signature: String::new(),
                            item_id: item_id.clone(),
                            part_started: has_initial,
                        },
                        closed: false,
                    },
                );
                let mut events = Vec::new();
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.output_item.added",
                    "sequence_number": sequence,
                    "output_index": index,
                    "item": {
                        "id": item_id,
                        "type": "reasoning",
                        "status": "in_progress",
                        "summary": [],
                    },
                }));
                if has_initial {
                    let sequence = self.next_sequence();
                    events.push(json!({
                        "type": "response.reasoning_summary_part.added",
                        "sequence_number": sequence,
                        "item_id": item_id,
                        "output_index": index,
                        "summary_index": 0,
                        "part": { "type": "summary_text", "text": "" },
                    }));
                    let sequence = self.next_sequence();
                    events.push(json!({
                        "type": "response.reasoning_summary_text.delta",
                        "sequence_number": sequence,
                        "item_id": item_id,
                        "output_index": index,
                        "summary_index": 0,
                        "delta": initial,
                    }));
                }
                Ok(events)
            }
            "redacted_thinking" => {
                let data =
                    required_string(content_block, "data", "content_block_start.content_block")?;
                let item_id = format!("rs_{index}");
                self.state_mut()?.blocks.insert(
                    index,
                    BlockState {
                        kind: BlockKind::Redacted {
                            data: data.to_owned(),
                            item_id: item_id.clone(),
                        },
                        closed: false,
                    },
                );
                let sequence = self.next_sequence();
                Ok(vec![json!({
                    "type": "response.output_item.added",
                    "sequence_number": sequence,
                    "output_index": index,
                    "item": {
                        "id": item_id,
                        "type": "reasoning",
                        "status": "in_progress",
                        "summary": [],
                    },
                })])
            }
            other => Err(format!(
                "unsupported content block type \"{other}\"; server tools and non-text Claude output are not supported by this adapter slice"
            )),
        }
    }

    fn content_block_delta(&mut self, data: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let index = content_index(data)?;
        let delta = expect_object(
            data.get("delta")
                .ok_or("content_block_delta.delta is required")?,
            "content_block_delta.delta",
        )?;
        let kind = required_string(delta, "type", "content_block_delta.delta")?;
        if kind == "citations_delta" {
            return Err("citations_delta is not supported by this adapter slice".to_owned());
        }
        let action = {
            let state = self.state_mut()?;
            let block = state
                .blocks
                .get_mut(&index)
                .ok_or_else(|| format!("content_block_delta for unknown content block {index}"))?;
            match (&mut block.kind, kind) {
                (BlockKind::Text { text, item_id }, "text_delta") => {
                    let delta_text = required_string(delta, "text", "content_block_delta.delta")?;
                    text.push_str(delta_text);
                    DeltaAction::Text {
                        item_id: item_id.clone(),
                        delta: delta_text.to_owned(),
                    }
                }
                (
                    BlockKind::Tool {
                        arguments, item_id, ..
                    },
                    "input_json_delta",
                ) => {
                    let partial =
                        required_string(delta, "partial_json", "content_block_delta.delta")?;
                    arguments.push_str(partial);
                    DeltaAction::Arguments {
                        item_id: item_id.clone(),
                        delta: partial.to_owned(),
                    }
                }
                (
                    BlockKind::Thinking {
                        text,
                        item_id,
                        part_started,
                        ..
                    },
                    "thinking_delta",
                ) => {
                    let thinking = required_string(delta, "thinking", "content_block_delta.delta")?;
                    text.push_str(thinking);
                    let start_part = !*part_started;
                    *part_started = true;
                    DeltaAction::Thinking {
                        item_id: item_id.clone(),
                        delta: thinking.to_owned(),
                        start_part,
                    }
                }
                (BlockKind::Thinking { signature, .. }, "signature_delta") => {
                    signature.push_str(required_string(
                        delta,
                        "signature",
                        "content_block_delta.delta",
                    )?);
                    DeltaAction::Signature
                }
                _ => {
                    return Err(format!(
                        "delta type \"{kind}\" does not match content block {index}"
                    ));
                }
            }
        };

        match action {
            DeltaAction::Text { item_id, delta } => {
                let sequence = self.next_sequence();
                Ok(vec![json!({
                    "type": "response.output_text.delta",
                    "sequence_number": sequence,
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "delta": delta,
                })])
            }
            DeltaAction::Arguments { item_id, delta } => {
                let sequence = self.next_sequence();
                Ok(vec![json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": sequence,
                    "item_id": item_id,
                    "output_index": index,
                    "delta": delta,
                })])
            }
            DeltaAction::Thinking {
                item_id,
                delta,
                start_part,
            } => {
                let mut events = Vec::with_capacity(2);
                if start_part {
                    let sequence = self.next_sequence();
                    events.push(json!({
                        "type": "response.reasoning_summary_part.added",
                        "sequence_number": sequence,
                        "item_id": item_id,
                        "output_index": index,
                        "summary_index": 0,
                        "part": { "type": "summary_text", "text": "" },
                    }));
                }
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.reasoning_summary_text.delta",
                    "sequence_number": sequence,
                    "item_id": item_id,
                    "output_index": index,
                    "summary_index": 0,
                    "delta": delta,
                }));
                Ok(events)
            }
            DeltaAction::Signature => Ok(Vec::new()),
        }
    }

    fn content_block_stop(&mut self, data: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let index = content_index(data)?;
        let action = {
            let state = self.state_mut()?;
            let block = state
                .blocks
                .get_mut(&index)
                .ok_or_else(|| format!("content_block_stop for unknown content block {index}"))?;
            if block.closed {
                return Err(format!("content block {index} was stopped twice"));
            }
            block.closed = true;
            match &block.kind {
                BlockKind::Text { item_id, text } => StopAction::Text {
                    item_id: item_id.clone(),
                    text: text.clone(),
                },
                BlockKind::Tool {
                    call_id,
                    name,
                    arguments,
                    item_id,
                } => StopAction::Tool {
                    item_id: item_id.clone(),
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                },
                BlockKind::Thinking {
                    item_id,
                    text,
                    part_started,
                    ..
                } => StopAction::Thinking {
                    item_id: item_id.clone(),
                    text: text.clone(),
                    part_started: *part_started,
                },
                BlockKind::Redacted { .. } => StopAction::Redacted,
            }
        };

        match action {
            StopAction::Text { item_id, text } => {
                let part = json!({ "type": "output_text", "text": text, "annotations": [] });
                let item = json!({
                    "id": item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [part.clone()],
                });
                self.state_mut()?.completed.insert(index, item.clone());
                let mut events = Vec::with_capacity(3);
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.output_text.done",
                    "sequence_number": sequence,
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "text": text,
                }));
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.content_part.done",
                    "sequence_number": sequence,
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "part": part,
                }));
                Ok(events)
            }
            StopAction::Tool {
                item_id,
                call_id,
                name,
                arguments,
            } => {
                let arguments = if arguments.trim().is_empty() {
                    "{}".to_owned()
                } else {
                    arguments
                };
                let parsed: Value = serde_json::from_str(&arguments).map_err(|error| {
                    format!("tool \"{name}\" streamed invalid JSON arguments: {error}")
                })?;
                if !parsed.is_object() {
                    return Err(format!(
                        "tool \"{name}\" streamed arguments that are not a JSON object"
                    ));
                }
                let item = json!({
                    "id": item_id,
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                });
                {
                    let state = self.state_mut()?;
                    state.tool_ids.push(call_id.clone());
                    state.completed.insert(index, item.clone());
                }
                let mut events = Vec::with_capacity(2);
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.function_call_arguments.done",
                    "sequence_number": sequence,
                    "item_id": item_id,
                    "output_index": index,
                    "arguments": arguments,
                }));
                Ok(events)
            }
            StopAction::Thinking {
                item_id,
                text,
                part_started,
            } => {
                if !part_started {
                    return Ok(Vec::new());
                }
                let mut events = Vec::with_capacity(2);
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.reasoning_summary_text.done",
                    "sequence_number": sequence,
                    "item_id": item_id,
                    "output_index": index,
                    "summary_index": 0,
                    "text": text,
                }));
                let sequence = self.next_sequence();
                events.push(json!({
                    "type": "response.reasoning_summary_part.done",
                    "sequence_number": sequence,
                    "item_id": item_id,
                    "output_index": index,
                    "summary_index": 0,
                    "part": { "type": "summary_text", "text": text },
                }));
                Ok(events)
            }
            StopAction::Redacted => Ok(Vec::new()),
        }
    }

    fn message_delta(&mut self, data: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let state = self.state_mut()?;
        let delta = expect_object(
            data.get("delta").ok_or("message_delta.delta is required")?,
            "message_delta.delta",
        )?;
        if let Some(stop_reason) = string_field(delta, "stop_reason", "message_delta.delta")? {
            state.stop_reason = Some(stop_reason.to_owned());
        }
        if let Some(usage) = data.get("usage").filter(|value| !value.is_null()) {
            absorb_usage(state, usage);
        }
        Ok(Vec::new())
    }

    fn message_stop(&mut self) -> Result<Vec<Value>, String> {
        let configured_model = self.envelope_model.clone();
        {
            let state = self
                .state
                .as_mut()
                .ok_or("received message_stop before message_start")?;
            if let Some((index, _)) = state.blocks.iter().find(|(_, block)| !block.closed) {
                return Err(format!(
                    "message_stop before content_block_stop for content block {index}"
                ));
            }
            let reasoning_indices: Vec<usize> = state
                .blocks
                .iter()
                .filter(|(_, block)| {
                    matches!(
                        &block.kind,
                        BlockKind::Thinking { .. } | BlockKind::Redacted { .. }
                    )
                })
                .map(|(index, _)| *index)
                .collect();
            let reasoning_count = reasoning_indices.len();
            let tool_ids = state.tool_ids.clone();
            let model = configured_model.unwrap_or_else(|| state.model.clone());
            for (reasoning_index, block_index) in reasoning_indices.into_iter().enumerate() {
                let (item_id, block_value, summary_text) = {
                    let block = state
                        .blocks
                        .get(&block_index)
                        .expect("reasoning block exists");
                    match &block.kind {
                        BlockKind::Thinking {
                            text,
                            signature,
                            item_id,
                            ..
                        } => (
                            item_id.clone(),
                            json!({ "type": "thinking", "thinking": text, "signature": signature }),
                            text.clone(),
                        ),
                        BlockKind::Redacted { data, item_id } => (
                            item_id.clone(),
                            json!({ "type": "redacted_thinking", "data": data }),
                            String::new(),
                        ),
                        _ => unreachable!("filtered to reasoning blocks"),
                    }
                };
                let envelope = json!({
                    "v": THINKING_ENVELOPE_VERSION,
                    "provider": "anthropic",
                    "model": model.clone(),
                    "reasoning_index": reasoning_index,
                    "reasoning_count": reasoning_count,
                    "call_ids": &tool_ids,
                    "blocks": [block_value],
                });
                let encrypted_content = encode_envelope(&envelope);
                let summary = if summary_text.is_empty() {
                    json!([])
                } else {
                    json!([{ "type": "summary_text", "text": summary_text }])
                };
                let item = json!({
                    "id": item_id,
                    "type": "reasoning",
                    "status": "completed",
                    "summary": summary,
                    "encrypted_content": encrypted_content,
                });
                state.completed.insert(block_index, item);
            }
        }

        let (event_type, status, incomplete, error) = {
            let stop_reason = self
                .state
                .as_ref()
                .and_then(|state| state.stop_reason.as_deref());
            match stop_reason {
                Some("max_tokens") | Some("model_context_window_exceeded") => (
                    "response.incomplete",
                    "incomplete",
                    Some(json!({ "reason": "max_output_tokens" })),
                    None,
                ),
                // A Claude refusal is a final model answer, not filtered
                // output; retrying it as an OpenAI content_filter incomplete
                // would loop. Any refusal text was emitted as a text block.
                Some("refusal") => ("response.completed", "completed", None, None),
                Some("pause_turn") => (
                    "response.failed",
                    "failed",
                    None,
                    Some(json!({
                        "code": "pause_turn",
                        "message": "Anthropic paused the turn; resend the full conversation to continue",
                    })),
                ),
                _ => ("response.completed", "completed", None, None),
            }
        };

        // Close every item in output order. Codex builds conversation history
        // from `response.output_item.done` in arrival order, so completing a
        // reasoning item after a later tool call would reorder the assistant
        // turn on replay and break Claude's thinking-prefix check.
        let (output, usage, items) = {
            let state = self.state.as_ref().expect("state exists");
            if status == "failed" {
                (json!([]), usage_value(state), Vec::new())
            } else {
                (
                    Value::Array(state.completed.values().cloned().collect()),
                    usage_value(state),
                    state
                        .completed
                        .iter()
                        .map(|(index, item)| (*index, item.clone()))
                        .collect::<Vec<_>>(),
                )
            }
        };
        let mut events = Vec::with_capacity(items.len() + 1);
        for (index, item) in items {
            let sequence = self.next_sequence();
            events.push(json!({
                "type": "response.output_item.done",
                "sequence_number": sequence,
                "output_index": index,
                "item": item,
            }));
        }
        let event = self.response_event(event_type, status, output, usage, error, incomplete);
        self.terminal = true;
        events.push(event);
        Ok(events)
    }

    fn error_event(&mut self, data: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let error = expect_object(
            data.get("error")
                .ok_or("error event needs an error object")?,
            "error",
        )?;
        let code = string_field(error, "type", "error")?.unwrap_or("api_error");
        let message =
            string_field(error, "message", "error")?.unwrap_or("Anthropic returned an error");
        let event = self.failed_event(code, message);
        self.terminal = true;
        Ok(vec![event])
    }

    fn failed_event(&mut self, code: &str, message: &str) -> Value {
        self.response_event(
            "response.failed",
            "failed",
            json!([]),
            Value::Null,
            Some(json!({ "code": code, "message": message })),
            None,
        )
    }

    fn response_event(
        &mut self,
        event_type: &str,
        status: &str,
        output: Value,
        usage: Value,
        error: Option<Value>,
        incomplete: Option<Value>,
    ) -> Value {
        let sequence = self.next_sequence();
        let (id, model) = match &self.state {
            Some(state) => (state.response_id.clone(), state.model.clone()),
            None => ("resp_codeox".to_owned(), "unknown".to_owned()),
        };
        json!({
            "type": event_type,
            "sequence_number": sequence,
            "response": {
                "id": id,
                "object": "response",
                "created_at": self.created_at,
                "status": status,
                "model": model,
                "output": output,
                "parallel_tool_calls": true,
                "tool_choice": "auto",
                "tools": [],
                "error": error,
                "incomplete_details": incomplete,
                "usage": usage,
            },
        })
    }
}

#[derive(Debug)]
struct StreamState {
    response_id: String,
    model: String,
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
    thinking_tokens: u64,
    stop_reason: Option<String>,
    blocks: BTreeMap<usize, BlockState>,
    completed: BTreeMap<usize, Value>,
    tool_ids: Vec<String>,
}

impl StreamState {
    fn new(response_id: String, model: String) -> Self {
        Self {
            response_id,
            model,
            input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 0,
            thinking_tokens: 0,
            stop_reason: None,
            blocks: BTreeMap::new(),
            completed: BTreeMap::new(),
            tool_ids: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct BlockState {
    kind: BlockKind,
    closed: bool,
}

#[derive(Debug)]
enum BlockKind {
    Text {
        text: String,
        item_id: String,
    },
    Tool {
        call_id: String,
        name: String,
        arguments: String,
        item_id: String,
    },
    Thinking {
        text: String,
        signature: String,
        item_id: String,
        part_started: bool,
    },
    Redacted {
        data: String,
        item_id: String,
    },
}

enum DeltaAction {
    Text {
        item_id: String,
        delta: String,
    },
    Arguments {
        item_id: String,
        delta: String,
    },
    Thinking {
        item_id: String,
        delta: String,
        start_part: bool,
    },
    Signature,
}

enum StopAction {
    Text {
        item_id: String,
        text: String,
    },
    Tool {
        item_id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    Thinking {
        item_id: String,
        text: String,
        part_started: bool,
    },
    Redacted,
}

fn content_index(data: &Map<String, Value>) -> Result<usize, String> {
    data.get("index")
        .and_then(Value::as_u64)
        .map(|index| index as usize)
        .ok_or_else(|| "event.index must be a non-negative integer".to_owned())
}

fn absorb_usage(state: &mut StreamState, usage: &Value) {
    if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
        state.input_tokens = value;
    }
    if let Some(value) = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
    {
        state.cache_creation_input_tokens = value;
    }
    if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
        state.cache_read_input_tokens = value;
    }
    if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
        state.output_tokens = value;
    }
    if let Some(value) = usage
        .pointer("/output_tokens_details/thinking_tokens")
        .and_then(Value::as_u64)
    {
        state.thinking_tokens = value;
    }
}

fn usage_value(state: &StreamState) -> Value {
    let input_tokens = state
        .input_tokens
        .saturating_add(state.cache_creation_input_tokens)
        .saturating_add(state.cache_read_input_tokens);
    json!({
        "input_tokens": input_tokens,
        "input_tokens_details": { "cached_tokens": state.cache_read_input_tokens },
        "output_tokens": state.output_tokens,
        "output_tokens_details": { "reasoning_tokens": state.thinking_tokens },
        "total_tokens": input_tokens.saturating_add(state.output_tokens),
    })
}

// ---------------------------------------------------------------------------
// Small JSON helpers
// ---------------------------------------------------------------------------

fn expect_object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} must be a JSON object"))
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<Option<&'a str>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(format!("{what}.{key} must be a string")),
    }
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<&'a str, String> {
    string_field(object, key, what)?.ok_or_else(|| format!("{what}.{key} is required"))
}

fn number_field(object: &Map<String, Value>, key: &str, what: &str) -> Result<Option<f64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("{what}.{key} must be a number")),
        Some(_) => Err(format!("{what}.{key} must be a number")),
    }
}

fn bool_field(object: &Map<String, Value>, key: &str, what: &str) -> Result<Option<bool>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{what}.{key} must be a boolean")),
    }
}
