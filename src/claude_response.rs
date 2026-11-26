//! Anthropic Messages output decoded from the relay's Responses SSE surface.
//!
//! The relay routes Chat Completions vendors through the existing
//! Chat-to-Responses stream adapter; native Anthropic Messages vendors are
//! passed straight through and never reach this module. What arrives here is
//! therefore a Responses event stream produced by the relay's existing
//! adapters, and what leaves is one Anthropic Messages SSE stream (or, for a
//! captured non-stream response, one Messages JSON object).
//!
//! Both halves are pure: no I/O, no clocks, no dependency beyond `serde_json`.
//! `push` takes already-framed SSE fields and returns complete Anthropic event
//! objects. Every object carries its Anthropic `type`; the caller serialises
//! each one as `event: <type>` / `data: <json>`.
//!
//! # Translation
//!
//! * `message_start` opens the message. Responses reports usage only in its
//!   terminal event, so `message_start.usage` is zero; the final usage (input,
//!   cache read and output tokens) is carried by `message_delta` and by
//!   [`MessagesStream::message`]. A client that reads input tokens only from
//!   `message_start` will therefore see zeros. Cache reads move from
//!   `input_tokens` into `cache_read_input_tokens`; Responses has no
//!   cache-creation field, so any cache-creation tokens stay folded into
//!   `input_tokens`.
//! * Every text or refusal content part becomes one `text` block; every
//!   `function_call` item becomes one `tool_use` block. Block indices are
//!   assigned in first-appearance order, never reused, and remain stable.
//! * Text and function-argument fragments are forwarded as they arrive. A
//!   `tool_use` block starts with `input: {}` and its `input_json_delta`
//!   fragments must concatenate to one JSON object; that object is validated
//!   when the block closes. Argument accumulation is bounded by
//!   [`MAX_TOOL_ARGUMENT_BYTES`]. Parallel tool calls may interleave their
//!   deltas; each event carries its own block index.
//! * Reasoning-summary deltas are ignored. A foreign vendor cannot supply
//!   Claude's signed thinking blocks, and this module never fabricates an
//!   Anthropic signature. The one exception is a finished reasoning item that
//!   wraps a Chat Completions `reasoning_content` (the chat adapter's
//!   `codeox.chat.reasoning.v1:` envelope): it becomes a `thinking` block
//!   whose `signature` is that ox-owned marker, which Claude Code returns
//!   verbatim with the assistant turn and `claude_request` turns back into
//!   `reasoning_content` for the provider. Other reasoning items are dropped.
//!   Native Claude thinking already bypasses this module.
//! * A completed response maps to `end_turn` or, when any tool block was
//!   emitted, `tool_use`. An incomplete response maps `max_output_tokens` to
//!   `max_tokens` and `content_filter` to `refusal`; an open tool block is
//!   closed on the wire but omitted from the captured message because its
//!   arguments may have been cut off. A failed response, an upstream error
//!   event, and an upstream that ends before a terminal event all become one
//!   Anthropic `error` event and produce no `message_stop`.
//! * An empty completed response is rendered as one empty `text` block so the
//!   stream still describes a well-formed Messages response.

use crate::adapters::chat_completions::CHAT_REASONING_PREFIX;
use serde_json::{Map, Value, json};

/// Ceiling for one tool call's accumulated JSON arguments.
pub(crate) const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;

/// Decoder from Responses streaming events to Anthropic Messages events.
#[derive(Debug)]
pub(crate) struct MessagesStream {
    model: String,
    message_id: String,
    started: bool,
    terminal: bool,
    failed: bool,
    stop_reason: Option<&'static str>,
    usage: Usage,
    blocks: Vec<Block>,
    items: Vec<Item>,
}

impl MessagesStream {
    /// `model` is reported back to the Anthropic client. An empty value falls
    /// back to the model named by the Responses response.
    pub(crate) fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            message_id: "msg_codeox".to_owned(),
            started: false,
            terminal: false,
            failed: false,
            stop_reason: None,
            usage: Usage::default(),
            blocks: Vec::new(),
            items: Vec::new(),
        }
    }

    /// Push one already-framed SSE event. Returns zero or more Anthropic
    /// Messages event objects in emission order.
    pub(crate) fn push(&mut self, event: &str, data: &str) -> Result<Vec<Value>, String> {
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
            .map_err(|error| format!("invalid Responses SSE data for event {event:?}: {error}"))?;
        let object = value.as_object().ok_or_else(|| {
            format!("Responses SSE data for event {event:?} must be a JSON object")
        })?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .unwrap_or(event);
        match kind {
            "response.created" | "response.in_progress" => {
                Ok(self.ensure_started(object.get("response")))
            }
            "response.output_item.added" => self.output_item_added(object),
            "response.output_item.done" => self.output_item_done(object),
            "response.content_part.added" => self.content_part_added(object),
            "response.content_part.done" => self.content_part_done(object),
            "response.output_text.delta" | "response.refusal.delta" => self.text_delta(object),
            "response.output_text.done" | "response.refusal.done" => self.text_done(object),
            "response.function_call_arguments.delta" => self.tool_arguments_delta(object),
            "response.function_call_arguments.done" => self.tool_arguments_done(object),
            // Reasoning is deliberately dropped: no signed thinking block can
            // be invented for a foreign vendor.
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done" => Ok(Vec::new()),
            "response.completed" => self.complete(object, false),
            "response.incomplete" => self.complete(object, true),
            "response.failed" => self.failed_response(object),
            "error" => self.error_event(object),
            // Keepalives and event types this relay does not model are ignored,
            // matching the Responses clients' own forward-compatible behaviour.
            _ => Ok(Vec::new()),
        }
    }

    /// Finish a stream that ended without a terminal Responses event.
    ///
    /// A started stream receives one Anthropic `error` event; nothing is
    /// reported as a completed message. Idempotent once terminal.
    pub(crate) fn finish(&mut self) -> Result<Vec<Value>, String> {
        if self.terminal {
            return Ok(Vec::new());
        }
        if !self.started {
            return Err("the Responses stream ended before it started".to_owned());
        }
        let mut events = Vec::new();
        if let Ok(mut closed) = self.close_all_open(true) {
            events.append(&mut closed);
        }
        events.push(error_event(
            "api_error",
            "the upstream Responses stream ended before a terminal event",
        ));
        self.failed = true;
        self.terminal = true;
        Ok(events)
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// The captured Anthropic Messages object after a normal terminal event
    /// (`response.completed` or `response.incomplete`). `None` after an error
    /// or a truncated stream.
    pub(crate) fn message(&self) -> Option<Value> {
        if !self.terminal || self.failed {
            return None;
        }
        let stop_reason = self.stop_reason?;
        let mut content = self
            .blocks
            .iter()
            .filter_map(Block::content)
            .collect::<Vec<_>>();
        if content.is_empty() {
            content.push(json!({ "type": "text", "text": "" }));
        }
        Some(json!({
            "id": self.message_id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": content,
            "stop_reason": stop_reason,
            "stop_sequence": Value::Null,
            "usage": self.usage.to_json(),
        }))
    }

    // -- stream construction -------------------------------------------------

    fn ensure_started(&mut self, response: Option<&Value>) -> Vec<Value> {
        if self.started {
            return Vec::new();
        }
        let response = response.and_then(Value::as_object);
        if self.model.trim().is_empty() {
            self.model = response
                .and_then(|response| response.get("model"))
                .and_then(Value::as_str)
                .filter(|model| !model.trim().is_empty())
                .unwrap_or("unknown")
                .to_owned();
        }
        if let Some(usage) = response
            .and_then(|response| response.get("usage"))
            .filter(|usage| !usage.is_null())
        {
            self.usage = usage_from_responses(usage);
        }
        let raw_id = response
            .and_then(|response| response.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        self.message_id = anthropic_message_id(raw_id);
        self.started = true;
        vec![json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": self.usage.to_json(),
            },
        })]
    }

    fn output_item_added(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let item = object
            .get("item")
            .and_then(Value::as_object)
            .ok_or("response.output_item.added.item must be a JSON object")?;
        let item_type = required_string(item, "type", "response.output_item.added.item")?;
        let output_index = output_index_from(object, Some(item))
            .or_else(|| {
                self.find_item_by_id(&item_id_from(object, Some(item)))
                    .map(|pos| self.items[pos].output_index)
            })
            .unwrap_or_else(|| self.next_output_index());
        if self
            .items
            .iter()
            .any(|existing| existing.output_index == output_index)
        {
            return Err(format!(
                "response.output_index {output_index} was announced twice"
            ));
        }
        let item_id = item_id_from(object, Some(item));
        let mut events = self.ensure_started(None);

        match item_type {
            "message" => {
                self.items.push(Item {
                    output_index,
                    item_id,
                    kind: ItemKind::Message { parts: Vec::new() },
                });
                let position = self.items.len() - 1;
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for (fallback, part) in content.iter().enumerate() {
                        let Some(part) = part.as_object() else {
                            return Err(
                                "response.output_item.added.item.content[] must be JSON objects"
                                    .to_owned(),
                            );
                        };
                        let Some(text) = text_part_text(part) else {
                            continue;
                        };
                        let content_index =
                            content_index_from(None, Some(part)).unwrap_or(fallback as u64);
                        events.extend(self.ensure_text_block(position, content_index, text)?);
                    }
                }
            }
            "function_call" => {
                let name = required_string(item, "name", "response.output_item.added.item")?;
                if name.trim().is_empty() {
                    return Err("a function_call item has an empty name".to_owned());
                }
                let tool_id = tool_id_from(item, output_index);
                self.reject_duplicate_tool_id(&tool_id)?;
                let arguments =
                    arguments_field(item, "response.output_item.added.item")?.to_owned();
                let block = self.blocks.len();
                self.blocks.push(Block {
                    kind: BlockKind::Tool {
                        id: tool_id.clone(),
                        name: name.to_owned(),
                        arguments: String::new(),
                        input: None,
                    },
                    closed: false,
                    valid: true,
                });
                self.items.push(Item {
                    output_index,
                    item_id,
                    kind: ItemKind::Tool { block },
                });
                events.push(tool_block_start(block, &tool_id, name));
                if !arguments.is_empty() {
                    events.extend(self.append_tool_arguments(block, &arguments)?);
                }
            }
            // Reasoning and output kinds with no Anthropic equivalent are
            // tracked only so their later deltas and done events are ignored.
            _ => self.items.push(Item {
                output_index,
                item_id,
                kind: ItemKind::Ignored,
            }),
        }
        Ok(events)
    }

    fn content_part_added(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let part = object
            .get("part")
            .and_then(Value::as_object)
            .ok_or("response.content_part.added.part must be a JSON object")?;
        let Some(text) = text_part_text(part) else {
            return Ok(self.ensure_started(None));
        };
        let mut events = self.ensure_started(None);
        let (output_index, item_id) = self.event_item_key(object);
        let position = self.find_or_create_message(output_index, item_id)?;
        let content_index = content_index_from(Some(object), Some(part)).unwrap_or(0);
        events.extend(self.ensure_text_block(position, content_index, text)?);
        Ok(events)
    }

    fn content_part_done(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let part = object
            .get("part")
            .and_then(Value::as_object)
            .ok_or("response.content_part.done.part must be a JSON object")?;
        let Some(text) = text_part_text(part) else {
            return Ok(self.ensure_started(None));
        };
        let mut events = self.ensure_started(None);
        let (output_index, item_id) = self.event_item_key(object);
        let position = self.find_or_create_message(output_index, item_id)?;
        let content_index = content_index_from(Some(object), Some(part)).unwrap_or(0);
        events.extend(self.ensure_text_block(position, content_index, text)?);
        let block = self.part_block(position, content_index);
        if let Some(block) = block {
            events.extend(self.close_block(block));
        }
        Ok(events)
    }

    fn text_delta(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let delta = required_string(object, "delta", "text delta event")?;
        let mut events = self.ensure_started(None);
        if delta.is_empty() {
            return Ok(events);
        }
        let (output_index, item_id) = self.event_item_key(object);
        let position = self.find_or_create_message(output_index, item_id)?;
        let content_index = content_index_from(Some(object), None).unwrap_or(0);
        events.extend(self.ensure_text_block(position, content_index, "")?);
        let block = self.part_block(position, content_index).ok_or_else(|| {
            format!(
                "text delta arrived for content index {content_index} without an open text block"
            )
        })?;
        events.extend(self.append_text(block, delta)?);
        Ok(events)
    }

    fn text_done(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let text = match object.get("text").or_else(|| object.get("refusal")) {
            None | Some(Value::Null) => "",
            Some(Value::String(text)) => text.as_str(),
            Some(_) => return Err("a text done event needs a string text value".to_owned()),
        };
        let mut events = self.ensure_started(None);
        let (output_index, item_id) = self.event_item_key(object);
        let position = self.find_or_create_message(output_index, item_id)?;
        let content_index = content_index_from(Some(object), None).unwrap_or(0);
        events.extend(self.ensure_text_block(position, content_index, "")?);
        let block = self.part_block(position, content_index).ok_or_else(|| {
            format!(
                "text done arrived for content index {content_index} without an open text block"
            )
        })?;
        let current = self.text_of(block).unwrap_or_default();
        if !text.is_empty() {
            if current.is_empty() {
                events.extend(self.append_text(block, text)?);
            } else if current != text {
                return Err(
                    "the final text disagrees with the text streamed for its content part"
                        .to_owned(),
                );
            }
        }
        Ok(events)
    }

    fn tool_arguments_delta(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let delta = required_string(object, "delta", "function argument delta event")?;
        let mut events = self.ensure_started(None);
        let block = self.tool_block_for_event(object)?;
        if !delta.is_empty() {
            events.extend(self.append_tool_arguments(block, delta)?);
        }
        Ok(events)
    }

    fn tool_arguments_done(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let done = arguments_field(object, "function argument done event")?;
        let mut events = self.ensure_started(None);
        let block = self.tool_block_for_event(object)?;
        events.extend(self.reconcile_tool_arguments(block, done)?);
        events.extend(self.finish_tool_block(block, true)?);
        Ok(events)
    }

    fn output_item_done(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let item = object
            .get("item")
            .and_then(Value::as_object)
            .ok_or("response.output_item.done.item must be a JSON object")?;
        let item_type = required_string(item, "type", "response.output_item.done.item")?;
        let mut events = self.ensure_started(None);
        let output_index = output_index_from(object, Some(item))
            .or_else(|| {
                self.find_item_by_id(&item_id_from(object, Some(item)))
                    .map(|pos| self.items[pos].output_index)
            })
            .unwrap_or_else(|| self.next_output_index());
        let item_id = item_id_from(object, Some(item));
        let position = self.find_item(output_index, &item_id);

        match item_type {
            "message" => {
                let position = match position {
                    Some(position) => {
                        if !matches!(self.items[position].kind, ItemKind::Message { .. }) {
                            return Err(format!(
                                "output item {output_index} finished as a message after starting as another kind"
                            ));
                        }
                        position
                    }
                    None => {
                        self.items.push(Item {
                            output_index,
                            item_id: item_id.clone(),
                            kind: ItemKind::Message { parts: Vec::new() },
                        });
                        self.items.len() - 1
                    }
                };
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for (fallback, part) in content.iter().enumerate() {
                        let Some(part) = part.as_object() else {
                            return Err(
                                "response.output_item.done.item.content[] must be JSON objects"
                                    .to_owned(),
                            );
                        };
                        let Some(text) = text_part_text(part) else {
                            continue;
                        };
                        let content_index =
                            content_index_from(None, Some(part)).unwrap_or(fallback as u64);
                        events.extend(self.ensure_text_block(position, content_index, text)?);
                    }
                }
                let blocks = self.message_blocks(position);
                for block in blocks {
                    events.extend(self.close_block(block));
                }
            }
            "function_call" => {
                let block = match position {
                    Some(position) => match self.items[position].kind {
                        ItemKind::Tool { block, .. } => block,
                        _ => {
                            return Err(format!(
                                "output item {output_index} finished as a function call after starting as another kind"
                            ));
                        }
                    },
                    None => {
                        let name = required_string(item, "name", "response.output_item.done.item")?;
                        if name.trim().is_empty() {
                            return Err("a function_call item has an empty name".to_owned());
                        }
                        let tool_id = tool_id_from(item, output_index);
                        self.reject_duplicate_tool_id(&tool_id)?;
                        let block = self.blocks.len();
                        self.blocks.push(Block {
                            kind: BlockKind::Tool {
                                id: tool_id.clone(),
                                name: name.to_owned(),
                                arguments: String::new(),
                                input: None,
                            },
                            closed: false,
                            valid: true,
                        });
                        self.items.push(Item {
                            output_index,
                            item_id: item_id.clone(),
                            kind: ItemKind::Tool { block },
                        });
                        events.push(tool_block_start(block, &tool_id, name));
                        block
                    }
                };
                let arguments = arguments_field(item, "response.output_item.done.item")?;
                events.extend(self.reconcile_tool_arguments(block, arguments)?);
                events.extend(self.finish_tool_block(block, true)?);
            }
            other => {
                if position.is_none() {
                    self.items.push(Item {
                        output_index,
                        item_id,
                        kind: ItemKind::Ignored,
                    });
                }
                // A Chat Completions `reasoning_content` is carried to the
                // client as a thinking block whose signature is the adapter's
                // own marker (never an Anthropic signature), so the next
                // request can restore it. Other reasoning stays dropped.
                if other == "reasoning"
                    && let Some(marker) = item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .filter(|raw| {
                            raw.strip_prefix(CHAT_REASONING_PREFIX)
                                .is_some_and(|text| !text.is_empty())
                        })
                {
                    let text = marker[CHAT_REASONING_PREFIX.len()..].to_owned();
                    let block = self.blocks.len();
                    self.blocks.push(Block {
                        kind: BlockKind::Thinking {
                            text: text.clone(),
                            signature: marker.to_owned(),
                        },
                        closed: true,
                        valid: true,
                    });
                    events.push(json!({
                        "type": "content_block_start",
                        "index": block,
                        "content_block": { "type": "thinking", "thinking": "", "signature": "" },
                    }));
                    events.push(json!({
                        "type": "content_block_delta",
                        "index": block,
                        "delta": { "type": "thinking_delta", "thinking": text },
                    }));
                    events.push(json!({
                        "type": "content_block_delta",
                        "index": block,
                        "delta": { "type": "signature_delta", "signature": marker },
                    }));
                    events.push(block_stop(block));
                }
            }
        }
        Ok(events)
    }

    fn complete(
        &mut self,
        object: &Map<String, Value>,
        incomplete: bool,
    ) -> Result<Vec<Value>, String> {
        let response = object.get("response").and_then(Value::as_object);
        let mut events = self.ensure_started(object.get("response"));
        let incomplete = incomplete
            || response
                .and_then(|response| response.get("status"))
                .and_then(Value::as_str)
                == Some("incomplete");
        events.extend(self.close_all_open(!incomplete)?);
        if self.blocks.is_empty()
            && let Some(output) = response
                .and_then(|response| response.get("output"))
                .and_then(Value::as_array)
        {
            // Some Responses implementations only disclose their output in the
            // terminal event. Nothing was streamed as a block in that case, so
            // build the content from the final object instead of dropping it.
            events.extend(self.ingest_final_output(output)?);
        }
        if self.blocks.is_empty() {
            self.blocks.push(Block {
                kind: BlockKind::Text {
                    text: String::new(),
                },
                closed: false,
                valid: true,
            });
            events.push(text_block_start(0));
            events.extend(self.close_block(0));
        }
        self.usage = response
            .and_then(|response| response.get("usage"))
            .filter(|usage| !usage.is_null())
            .map(usage_from_responses)
            .unwrap_or_default();
        let stop_reason = if incomplete {
            incomplete_stop_reason(response)
        } else if self.has_tool_block() {
            "tool_use"
        } else {
            "end_turn"
        };
        self.stop_reason = Some(stop_reason);
        events.push(message_delta(stop_reason, &self.usage));
        events.push(json!({ "type": "message_stop" }));
        self.terminal = true;
        Ok(events)
    }

    /// Build blocks from a terminal `response.output` array when no block was
    /// streamed. Used only for the empty-stream fallback in [`Self::complete`],
    /// so it cannot duplicate content that was already forwarded.
    fn ingest_final_output(&mut self, output: &[Value]) -> Result<Vec<Value>, String> {
        let mut events = Vec::new();
        for (index, item) in output.iter().enumerate() {
            let item = item
                .as_object()
                .ok_or_else(|| "each response.output item must be a JSON object".to_owned())?;
            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                "message" => {
                    let Some(parts) = item.get("content").and_then(Value::as_array) else {
                        continue;
                    };
                    for part in parts {
                        let Some(part) = part.as_object() else {
                            return Err("message content parts must be JSON objects".to_owned());
                        };
                        let Some(text) = text_part_text(part) else {
                            continue;
                        };
                        let block = self.blocks.len();
                        self.blocks.push(Block {
                            kind: BlockKind::Text {
                                text: String::new(),
                            },
                            closed: false,
                            valid: true,
                        });
                        events.push(text_block_start(block));
                        if !text.is_empty() {
                            events.extend(self.append_text(block, text)?);
                        }
                        events.extend(self.close_block(block));
                    }
                }
                "function_call" => {
                    let what = format!("response.output[{index}]");
                    let name = required_string(item, "name", &what)?;
                    if name.trim().is_empty() {
                        return Err(format!("{what} has an empty tool name"));
                    }
                    let tool_id = tool_id_from(item, index as u64);
                    self.reject_duplicate_tool_id(&tool_id)?;
                    let arguments = arguments_field(item, &what)?;
                    let block = self.blocks.len();
                    self.blocks.push(Block {
                        kind: BlockKind::Tool {
                            id: tool_id.clone(),
                            name: name.to_owned(),
                            arguments: String::new(),
                            input: None,
                        },
                        closed: false,
                        valid: true,
                    });
                    events.push(tool_block_start(block, &tool_id, name));
                    if !arguments.is_empty() {
                        events.extend(self.append_tool_arguments(block, arguments)?);
                    }
                    events.extend(self.finish_tool_block(block, true)?);
                }
                _ => {}
            }
        }
        Ok(events)
    }

    fn failed_response(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let response = object.get("response").and_then(Value::as_object);
        let mut events = Vec::new();
        if self.started
            && let Ok(mut closed) = self.close_all_open(true)
        {
            events.append(&mut closed);
        }
        let (kind, message) = response
            .and_then(|response| response.get("error"))
            .filter(|error| !error.is_null())
            .map(error_details)
            .unwrap_or_else(|| {
                (
                    "api_error".to_owned(),
                    "the upstream response failed".to_owned(),
                )
            });
        events.push(error_event(&kind, &message));
        self.failed = true;
        self.terminal = true;
        Ok(events)
    }

    fn error_event(&mut self, object: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let mut events = Vec::new();
        if self.started
            && let Ok(mut closed) = self.close_all_open(true)
        {
            events.append(&mut closed);
        }
        let (kind, message) = object
            .get("error")
            .filter(|error| !error.is_null())
            .map(error_details)
            .unwrap_or_else(|| {
                let kind = object
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("api_error");
                let message = object
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the upstream reported an error");
                (anthropic_error_type(kind).to_owned(), message.to_owned())
            });
        events.push(error_event(&kind, &message));
        self.failed = true;
        self.terminal = true;
        Ok(events)
    }

    // -- item and block helpers ---------------------------------------------

    fn find_item(&self, output_index: u64, item_id: &str) -> Option<usize> {
        self.items
            .iter()
            .position(|item| item.output_index == output_index)
            .or_else(|| self.find_item_by_id(item_id))
    }

    fn find_item_by_id(&self, item_id: &str) -> Option<usize> {
        if item_id.is_empty() {
            return None;
        }
        self.items.iter().position(|item| item.item_id == item_id)
    }

    fn next_output_index(&self) -> u64 {
        self.items
            .iter()
            .map(|item| item.output_index.saturating_add(1))
            .max()
            .unwrap_or(0)
    }

    fn event_item_key(&self, object: &Map<String, Value>) -> (u64, String) {
        let item_id = item_id_from(object, None);
        let output_index = output_index_from(object, None)
            .or_else(|| {
                self.find_item_by_id(&item_id)
                    .map(|pos| self.items[pos].output_index)
            })
            .unwrap_or_else(|| self.next_output_index());
        (output_index, item_id)
    }

    fn find_or_create_message(
        &mut self,
        output_index: u64,
        item_id: String,
    ) -> Result<usize, String> {
        if let Some(position) = self.find_item(output_index, &item_id) {
            if matches!(self.items[position].kind, ItemKind::Message { .. }) {
                return Ok(position);
            }
            return Err(format!(
                "content arrived for output item {output_index}, which is not a message"
            ));
        }
        self.items.push(Item {
            output_index,
            item_id,
            kind: ItemKind::Message { parts: Vec::new() },
        });
        Ok(self.items.len() - 1)
    }

    fn ensure_text_block(
        &mut self,
        position: usize,
        content_index: u64,
        initial: &str,
    ) -> Result<Vec<Value>, String> {
        if let Some(block) = self.part_block(position, content_index) {
            let mut events = Vec::new();
            if !initial.is_empty() {
                let current = self.text_of(block).unwrap_or_default();
                if current.is_empty() {
                    if self.blocks[block].closed {
                        return Err(format!(
                            "content block {block} was stopped before its text arrived"
                        ));
                    }
                    events.extend(self.append_text(block, initial)?);
                } else if current != initial {
                    return Err(
                        "a message content part disagrees with the text streamed for it".to_owned(),
                    );
                }
            }
            return Ok(events);
        }
        let block = self.blocks.len();
        self.blocks.push(Block {
            kind: BlockKind::Text {
                text: String::new(),
            },
            closed: false,
            valid: true,
        });
        let mut events = vec![text_block_start(block)];
        if !initial.is_empty() {
            events.extend(self.append_text(block, initial)?);
        }
        if let ItemKind::Message { parts, .. } = &mut self.items[position].kind {
            parts.push(Part {
                content_index,
                block,
            });
        }
        Ok(events)
    }

    fn part_block(&self, position: usize, content_index: u64) -> Option<usize> {
        match &self.items[position].kind {
            ItemKind::Message { parts, .. } => parts
                .iter()
                .find(|part| part.content_index == content_index)
                .map(|part| part.block),
            _ => None,
        }
    }

    fn message_blocks(&self, position: usize) -> Vec<usize> {
        match &self.items[position].kind {
            ItemKind::Message { parts, .. } => parts.iter().map(|part| part.block).collect(),
            _ => Vec::new(),
        }
    }

    fn tool_block_for_event(&mut self, object: &Map<String, Value>) -> Result<usize, String> {
        let (output_index, item_id) = self.event_item_key(object);
        let position = self.find_item(output_index, &item_id).ok_or_else(|| {
            format!("function arguments arrived for unknown output item {output_index}")
        })?;
        match self.items[position].kind {
            ItemKind::Tool { block, .. } => Ok(block),
            _ => Err(format!(
                "function arguments arrived for output item {output_index}, which is not a function call"
            )),
        }
    }

    fn append_text(&mut self, block: usize, text: &str) -> Result<Vec<Value>, String> {
        if self.blocks[block].closed {
            return Err(format!("content block {block} was already stopped"));
        }
        match &mut self.blocks[block].kind {
            BlockKind::Text { text: current } => {
                current.push_str(text);
                Ok(vec![text_delta(block, text)])
            }
            BlockKind::Tool { .. } | BlockKind::Thinking { .. } => Err(format!(
                "text arrived for content block {block}, which is not text"
            )),
        }
    }

    fn text_of(&self, block: usize) -> Option<String> {
        match &self.blocks[block].kind {
            BlockKind::Text { text } => Some(text.clone()),
            BlockKind::Tool { .. } | BlockKind::Thinking { .. } => None,
        }
    }

    fn append_tool_arguments(&mut self, block: usize, partial: &str) -> Result<Vec<Value>, String> {
        if self.blocks[block].closed {
            return Err(format!("content block {block} was already stopped"));
        }
        match &mut self.blocks[block].kind {
            BlockKind::Tool {
                name,
                arguments,
                input,
                ..
            } => {
                if input.is_some() {
                    return Err(format!(
                        "tool \"{name}\" received arguments after its input was finalized"
                    ));
                }
                let total = arguments
                    .len()
                    .checked_add(partial.len())
                    .ok_or_else(|| "tool arguments overflowed".to_owned())?;
                if total > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(format!(
                        "tool \"{name}\" arguments exceeded the {MAX_TOOL_ARGUMENT_BYTES}-byte limit"
                    ));
                }
                arguments.push_str(partial);
                Ok(vec![tool_argument_delta(block, partial)])
            }
            BlockKind::Text { .. } | BlockKind::Thinking { .. } => Err(format!(
                "tool arguments arrived for content block {block}, which is text"
            )),
        }
    }

    fn reconcile_tool_arguments(
        &mut self,
        block: usize,
        final_arguments: &str,
    ) -> Result<Vec<Value>, String> {
        if final_arguments.is_empty() {
            return Ok(Vec::new());
        }
        let (name, current) = match &self.blocks[block].kind {
            BlockKind::Tool {
                name, arguments, ..
            } => (name.clone(), arguments.clone()),
            BlockKind::Text { .. } | BlockKind::Thinking { .. } => {
                return Err(format!(
                    "tool arguments arrived for content block {block}, which is text"
                ));
            }
        };
        if current.is_empty() {
            self.append_tool_arguments(block, final_arguments)
        } else if current == final_arguments {
            Ok(Vec::new())
        } else {
            Err(format!(
                "tool \"{name}\" final arguments disagree with the streamed fragments"
            ))
        }
    }

    fn finish_tool_block(&mut self, block: usize, strict: bool) -> Result<Vec<Value>, String> {
        if self.blocks[block].closed {
            return Ok(Vec::new());
        }
        let (name, arguments, input) = match &self.blocks[block].kind {
            BlockKind::Tool {
                name,
                arguments,
                input,
                ..
            } => (name.clone(), arguments.clone(), input.clone()),
            BlockKind::Text { .. } | BlockKind::Thinking { .. } => {
                return Err(format!("content block {block} is text, not a tool call"));
            }
        };
        if input.is_none() {
            match parse_tool_input(&name, &arguments) {
                Ok(parsed) => {
                    if let BlockKind::Tool { input, .. } = &mut self.blocks[block].kind {
                        *input = Some(parsed);
                    }
                }
                Err(error) => {
                    if strict {
                        return Err(error);
                    }
                    self.blocks[block].valid = false;
                }
            }
        }
        Ok(self.close_block(block))
    }

    fn close_block(&mut self, block: usize) -> Vec<Value> {
        if self.blocks[block].closed {
            return Vec::new();
        }
        self.blocks[block].closed = true;
        vec![block_stop(block)]
    }

    fn close_all_open(&mut self, strict: bool) -> Result<Vec<Value>, String> {
        let mut events = Vec::new();
        for block in 0..self.blocks.len() {
            if self.blocks[block].closed {
                continue;
            }
            if matches!(&self.blocks[block].kind, BlockKind::Tool { .. }) {
                if strict {
                    events.extend(self.finish_tool_block(block, true)?);
                } else {
                    // The upstream explicitly reported an incomplete response:
                    // an open tool call may have been cut off. Close it for the
                    // wire but omit it from the captured message.
                    self.blocks[block].valid = false;
                    events.extend(self.close_block(block));
                }
            } else {
                events.extend(self.close_block(block));
            }
        }
        Ok(events)
    }

    fn has_tool_block(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(&block.kind, BlockKind::Tool { .. }))
    }

    fn reject_duplicate_tool_id(&self, tool_id: &str) -> Result<(), String> {
        if self
            .blocks
            .iter()
            .any(|block| matches!(&block.kind, BlockKind::Tool { id, .. } if id == tool_id))
        {
            return Err(format!("duplicate tool call id \"{tool_id}\""));
        }
        Ok(())
    }
}

/// Build one Anthropic Messages object from a final Responses response (or an
/// event carrying one). This is the non-streaming counterpart of
/// [`MessagesStream::message`]; the same block and usage rules apply.
///
/// The relay's gateway currently captures non-streaming replies by draining
/// the converted stream, so this helper is optional; it exists for callers that
/// already hold a final Responses object.
#[allow(dead_code)] // optional non-streaming helper; the gateway uses `message()`
pub(crate) fn message_from_response(input: &Value, model: &str) -> Result<Value, String> {
    let response = if input.get("object").and_then(Value::as_str) == Some("response") {
        input.as_object()
    } else {
        input
            .get("response")
            .and_then(Value::as_object)
            .or_else(|| input.as_object())
    }
    .ok_or("input must be a Responses response object or an event carrying one")?;

    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    if status == "failed" {
        let (kind, message) = response
            .get("error")
            .filter(|error| !error.is_null())
            .map(error_details)
            .unwrap_or_else(|| {
                (
                    "api_error".to_owned(),
                    "the upstream response failed".to_owned(),
                )
            });
        return Err(format!("{kind}: {message}"));
    }

    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or("response.output must be an array")?;
    let mut content = Vec::new();
    let mut tool_ids: Vec<String> = Vec::new();
    for (index, item) in output.iter().enumerate() {
        let item = item
            .as_object()
            .ok_or("each response.output item must be a JSON object")?;
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => {
                let Some(parts) = item.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for part in parts {
                    let Some(part) = part.as_object() else {
                        return Err("message content parts must be JSON objects".to_owned());
                    };
                    if let Some(text) = text_part_text(part) {
                        content.push(json!({ "type": "text", "text": text }));
                    }
                }
            }
            "function_call" => {
                let name = required_string(item, "name", &format!("response.output[{index}]"))?;
                if name.trim().is_empty() {
                    return Err(format!("response.output[{index}] has an empty tool name"));
                }
                let tool_id = tool_id_from(item, index as u64);
                if tool_ids.iter().any(|existing| existing == &tool_id) {
                    return Err(format!("duplicate tool call id \"{tool_id}\""));
                }
                tool_ids.push(tool_id.clone());
                let arguments = arguments_field(item, &format!("response.output[{index}]"))?;
                let input = match parse_tool_input(name, arguments) {
                    Ok(input) => input,
                    // An incomplete turn may carry a tool call cut off mid-JSON;
                    // it cannot become an executable tool_use block.
                    Err(_) if status == "incomplete" => continue,
                    Err(error) => return Err(error),
                };
                content.push(json!({
                    "type": "tool_use",
                    "id": tool_id,
                    "name": name,
                    "input": input,
                }));
            }
            // Reasoning and output kinds with no Anthropic equivalent.
            _ => {}
        }
    }
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }

    let stop_reason = if status == "incomplete" {
        incomplete_stop_reason(Some(response))
    } else if content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        "tool_use"
    } else {
        "end_turn"
    };
    let model = if model.trim().is_empty() {
        response
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .unwrap_or("unknown")
    } else {
        model
    };
    let usage = response
        .get("usage")
        .filter(|usage| !usage.is_null())
        .map(usage_from_responses)
        .unwrap_or_default();
    let raw_id = response.get("id").and_then(Value::as_str).unwrap_or("");
    Ok(json!({
        "id": anthropic_message_id(raw_id),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": usage.to_json(),
    }))
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Block {
    kind: BlockKind,
    closed: bool,
    /// False when an incomplete response forced an open tool block to stop.
    /// Such a block stays in the stream (it was already forwarded) but is
    /// omitted from the captured non-streaming message.
    valid: bool,
}

impl Block {
    fn content(&self) -> Option<Value> {
        if !self.valid {
            return None;
        }
        match &self.kind {
            BlockKind::Text { text } => Some(json!({ "type": "text", "text": text })),
            BlockKind::Thinking { text, signature } => {
                Some(json!({ "type": "thinking", "thinking": text, "signature": signature }))
            }
            BlockKind::Tool {
                id, name, input, ..
            } => Some(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input.clone().unwrap_or_else(|| json!({})),
            })),
        }
    }
}

#[derive(Debug)]
enum BlockKind {
    Text {
        text: String,
    },
    /// A replayable Chat Completions reasoning text; see `output_item_done`.
    Thinking {
        text: String,
        signature: String,
    },
    Tool {
        id: String,
        name: String,
        arguments: String,
        input: Option<Value>,
    },
}

#[derive(Debug)]
struct Item {
    output_index: u64,
    item_id: String,
    kind: ItemKind,
}

#[derive(Debug)]
enum ItemKind {
    Message { parts: Vec<Part> },
    Tool { block: usize },
    Ignored,
}

#[derive(Debug)]
struct Part {
    content_index: u64,
    block: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct Usage {
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
}

impl Usage {
    fn to_json(self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "cache_creation_input_tokens": self.cache_creation_input_tokens,
            "cache_read_input_tokens": self.cache_read_input_tokens,
            "output_tokens": self.output_tokens,
        })
    }
}

// ---------------------------------------------------------------------------
// Event constructors
// ---------------------------------------------------------------------------

fn text_block_start(index: usize) -> Value {
    json!({
        "type": "content_block_start",
        "index": index,
        "content_block": { "type": "text", "text": "" },
    })
}

fn tool_block_start(index: usize, id: &str, name: &str) -> Value {
    json!({
        "type": "content_block_start",
        "index": index,
        "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} },
    })
}

fn text_delta(index: usize, text: &str) -> Value {
    json!({
        "type": "content_block_delta",
        "index": index,
        "delta": { "type": "text_delta", "text": text },
    })
}

fn tool_argument_delta(index: usize, partial_json: &str) -> Value {
    json!({
        "type": "content_block_delta",
        "index": index,
        "delta": { "type": "input_json_delta", "partial_json": partial_json },
    })
}

fn block_stop(index: usize) -> Value {
    json!({ "type": "content_block_stop", "index": index })
}

fn message_delta(stop_reason: &str, usage: &Usage) -> Value {
    json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
        "usage": usage.to_json(),
    })
}

fn error_event(kind: &str, message: &str) -> Value {
    json!({
        "type": "error",
        "error": { "type": kind, "message": message },
    })
}

// ---------------------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------------------

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<&'a str, String> {
    string_field(object, key).ok_or_else(|| format!("{what}.{key} is required"))
}

fn arguments_field<'a>(object: &'a Map<String, Value>, what: &str) -> Result<&'a str, String> {
    match object.get("arguments") {
        None | Some(Value::Null) => Ok(""),
        Some(Value::String(arguments)) => Ok(arguments.as_str()),
        Some(_) => Err(format!("{what}.arguments must be a string")),
    }
}

fn output_index_from(
    object: &Map<String, Value>,
    item: Option<&Map<String, Value>>,
) -> Option<u64> {
    object
        .get("output_index")
        .and_then(Value::as_u64)
        .or_else(|| {
            item.and_then(|item| item.get("output_index"))
                .and_then(Value::as_u64)
        })
}

fn item_id_from(object: &Map<String, Value>, item: Option<&Map<String, Value>>) -> String {
    object
        .get("item_id")
        .and_then(Value::as_str)
        .or_else(|| item.and_then(|item| item.get("id")).and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

fn content_index_from(
    object: Option<&Map<String, Value>>,
    part: Option<&Map<String, Value>>,
) -> Option<u64> {
    object
        .and_then(|object| object.get("content_index"))
        .and_then(Value::as_u64)
        .or_else(|| {
            part.and_then(|part| part.get("content_index"))
                .and_then(Value::as_u64)
        })
}

fn text_part_text(part: &Map<String, Value>) -> Option<&str> {
    match part.get("type").and_then(Value::as_str)? {
        "output_text" => Some(part.get("text").and_then(Value::as_str).unwrap_or("")),
        "refusal" => Some(part.get("refusal").and_then(Value::as_str).unwrap_or("")),
        _ => None,
    }
}

fn tool_id_from(item: &Map<String, Value>, output_index: u64) -> String {
    let call_id = string_field(item, "call_id").unwrap_or("");
    if !call_id.trim().is_empty() {
        return call_id.to_owned();
    }
    let id = string_field(item, "id").unwrap_or("");
    if !id.trim().is_empty() {
        return id.to_owned();
    }
    format!("toolu_{output_index}")
}

fn anthropic_message_id(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return "msg_codeox".to_owned();
    }
    if raw.starts_with("msg_") {
        raw.to_owned()
    } else if let Some(rest) = raw.strip_prefix("resp_") {
        format!("msg_{rest}")
    } else {
        format!("msg_{raw}")
    }
}

fn parse_tool_input(name: &str, arguments: &str) -> Result<Value, String> {
    if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
        return Err(format!(
            "tool \"{name}\" arguments exceeded the {MAX_TOOL_ARGUMENT_BYTES}-byte limit"
        ));
    }
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Ok(json!({}));
    }
    let parsed: Value = serde_json::from_str(trimmed)
        .map_err(|error| format!("tool \"{name}\" streamed invalid JSON arguments: {error}"))?;
    if !parsed.is_object() {
        return Err(format!(
            "tool \"{name}\" streamed arguments that are not a JSON object"
        ));
    }
    Ok(parsed)
}

fn usage_from_responses(usage: &Value) -> Usage {
    let total_input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(total_input);
    Usage {
        // Responses folds cache reads into input_tokens; Anthropic reports them
        // separately. Cache creation has no Responses field and stays folded
        // into input_tokens.
        input_tokens: total_input.saturating_sub(cached),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

fn incomplete_stop_reason(response: Option<&Map<String, Value>>) -> &'static str {
    let reason = response
        .and_then(|response| response.get("incomplete_details"))
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str);
    match reason {
        Some("content_filter") => "refusal",
        _ => "max_tokens",
    }
}

fn error_details(error: &Value) -> (String, String) {
    if let Some(error) = error.as_object() {
        let code = error
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| error.get("code").and_then(Value::as_str))
            .unwrap_or("api_error");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.get("detail").and_then(Value::as_str))
            .unwrap_or("the upstream reported an error");
        (anthropic_error_type(code).to_owned(), message.to_owned())
    } else if let Some(message) = error.as_str() {
        ("api_error".to_owned(), message.to_owned())
    } else {
        (
            "api_error".to_owned(),
            "the upstream reported an error".to_owned(),
        )
    }
}

fn anthropic_error_type(code: &str) -> &'static str {
    match code {
        "rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota" => "rate_limit_error",
        "authentication_error" | "invalid_api_key" => "authentication_error",
        "permission_error" => "permission_error",
        "not_found_error" => "not_found_error",
        "request_too_large" => "request_too_large",
        "overloaded_error" | "overloaded" => "overloaded_error",
        "invalid_request_error" | "invalid_prompt" | "context_length_exceeded" => {
            "invalid_request_error"
        }
        _ => "api_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn push(stream: &mut MessagesStream, event: Value) -> Result<Vec<Value>, String> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        stream.push(&kind, &event.to_string())
    }

    fn push_ok(stream: &mut MessagesStream, event: Value) -> Vec<Value> {
        push(stream, event).expect("event translates")
    }

    fn kinds(events: &[Value]) -> Vec<&str> {
        events
            .iter()
            .map(|event| event["type"].as_str().expect("event type"))
            .collect()
    }

    fn of_kind<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
        events
            .iter()
            .filter(|event| event["type"].as_str() == Some(kind))
            .collect()
    }

    fn created() -> Value {
        json!({
            "type": "response.created",
            "response": {
                "id": "resp_abc",
                "object": "response",
                "model": "upstream-model",
                "status": "in_progress",
                "output": []
            }
        })
    }

    fn message_item(output_index: u64, item_id: &str) -> Value {
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        })
    }

    fn text_delta_event(output_index: u64, item_id: &str, delta: &str) -> Value {
        json!({
            "type": "response.output_text.delta",
            "item_id": item_id,
            "output_index": output_index,
            "content_index": 0,
            "delta": delta
        })
    }

    fn message_done(output_index: u64, item_id: &str, text: &str) -> Value {
        json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            }
        })
    }

    fn tool_item(output_index: u64, call_id: &str, name: &str) -> Value {
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": format!("fc_{call_id}"),
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": ""
            }
        })
    }

    fn tool_delta(output_index: u64, call_id: &str, delta: &str) -> Value {
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": format!("fc_{call_id}"),
            "output_index": output_index,
            "delta": delta
        })
    }

    fn tool_done(output_index: u64, call_id: &str, arguments: &str) -> Value {
        json!({
            "type": "response.function_call_arguments.done",
            "item_id": format!("fc_{call_id}"),
            "output_index": output_index,
            "arguments": arguments
        })
    }

    fn tool_item_done(output_index: u64, call_id: &str, name: &str, arguments: &str) -> Value {
        json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": {
                "id": format!("fc_{call_id}"),
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": name,
                "arguments": arguments
            }
        })
    }

    fn completed(output: Value, usage: Value) -> Value {
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_abc",
                "object": "response",
                "model": "upstream-model",
                "status": "completed",
                "output": output,
                "usage": usage
            }
        })
    }

    #[test]
    fn streams_text_blocks_and_converts_usage() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = Vec::new();
        events.extend(push_ok(&mut stream, created()));
        events.extend(push_ok(&mut stream, message_item(0, "msg_0")));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.content_part.added",
                "item_id": "msg_0",
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}
            }),
        ));
        events.extend(push_ok(&mut stream, text_delta_event(0, "msg_0", "Hello")));
        events.extend(push_ok(
            &mut stream,
            text_delta_event(0, "msg_0", ", world"),
        ));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.output_text.done",
                "item_id": "msg_0",
                "output_index": 0,
                "content_index": 0,
                "text": "Hello, world"
            }),
        ));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.content_part.done",
                "item_id": "msg_0",
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "Hello, world", "annotations": []}
            }),
        ));
        events.extend(push_ok(
            &mut stream,
            message_done(0, "msg_0", "Hello, world"),
        ));
        events.extend(push_ok(
            &mut stream,
            completed(
                json!([{
                    "id": "msg_0",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Hello, world", "annotations": []}]
                }]),
                json!({
                    "input_tokens": 100,
                    "input_tokens_details": {"cached_tokens": 40},
                    "output_tokens": 7,
                    "total_tokens": 107
                }),
            ),
        ));

        assert_eq!(
            kinds(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[0]["message"]["id"], "msg_abc");
        assert_eq!(events[0]["message"]["model"], "relay-model");
        assert_eq!(events[0]["message"]["role"], "assistant");
        assert_eq!(events[0]["message"]["content"], json!([]));
        assert_eq!(
            events[1]["content_block"],
            json!({"type": "text", "text": ""})
        );
        assert_eq!(
            events[2]["delta"],
            json!({"type": "text_delta", "text": "Hello"})
        );
        assert_eq!(events[5]["delta"]["stop_reason"], "end_turn");
        assert_eq!(
            events[5]["usage"],
            json!({
                "input_tokens": 60,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 40,
                "output_tokens": 7
            })
        );
        assert!(stream.is_terminal());

        let message = stream.message().expect("captured message");
        assert_eq!(
            message["content"],
            json!([{"type": "text", "text": "Hello, world"}])
        );
        assert_eq!(message["stop_reason"], "end_turn");
        assert_eq!(message["usage"]["input_tokens"], 60);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 40);
    }

    #[test]
    fn a_terminal_event_closes_an_open_text_block() {
        // Mirrors the gateway's incremental path, which forwards deltas as they
        // arrive and only sees `response.completed` at the end.
        let mut stream = MessagesStream::new("vendor/upstream-model");
        let mut events = push_ok(&mut stream, created());
        events.extend(push_ok(&mut stream, message_item(0, "msg_0")));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.content_part.added",
                "item_id": "msg_0",
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}
            }),
        ));
        events.extend(push_ok(&mut stream, text_delta_event(0, "msg_0", "Hello")));
        events.extend(push_ok(
            &mut stream,
            completed(
                json!([{
                    "id": "msg_0",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Hello", "annotations": []}]
                }]),
                json!({
                    "input_tokens": 12,
                    "input_tokens_details": {"cached_tokens": 2},
                    "output_tokens": 3,
                    "total_tokens": 15
                }),
            ),
        ));

        assert_eq!(
            kinds(&events),
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
        let message = stream.message().expect("captured message");
        assert_eq!(
            message["content"],
            json!([{"type": "text", "text": "Hello"}])
        );
        assert_eq!(message["usage"]["input_tokens"], 10);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 2);
        assert_eq!(message["usage"]["output_tokens"], 3);
    }

    #[test]
    fn streams_parallel_fragmented_tool_arguments() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = Vec::new();
        events.extend(push_ok(&mut stream, created()));
        events.extend(push_ok(&mut stream, tool_item(0, "call_a", "shell")));
        events.extend(push_ok(&mut stream, tool_item(1, "call_b", "read_file")));
        // The Chat adapter can interleave chunks for parallel tool calls.
        events.extend(push_ok(&mut stream, tool_delta(0, "call_a", "{\"cmd\":")));
        events.extend(push_ok(&mut stream, tool_delta(1, "call_b", "{\"path\":")));
        events.extend(push_ok(&mut stream, tool_delta(0, "call_a", "\"ls\"}")));
        events.extend(push_ok(&mut stream, tool_delta(1, "call_b", "\"/tmp\"}")));
        events.extend(push_ok(
            &mut stream,
            tool_done(0, "call_a", "{\"cmd\":\"ls\"}"),
        ));
        events.extend(push_ok(
            &mut stream,
            tool_done(1, "call_b", "{\"path\":\"/tmp\"}"),
        ));
        events.extend(push_ok(
            &mut stream,
            tool_item_done(0, "call_a", "shell", "{\"cmd\":\"ls\"}"),
        ));
        events.extend(push_ok(
            &mut stream,
            tool_item_done(1, "call_b", "read_file", "{\"path\":\"/tmp\"}"),
        ));
        events.extend(push_ok(&mut stream, completed(json!([]), Value::Null)));

        let starts = of_kind(&events, "content_block_start");
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["index"], 0);
        assert_eq!(
            starts[0]["content_block"],
            json!({"type": "tool_use", "id": "call_a", "name": "shell", "input": {}})
        );
        assert_eq!(starts[1]["index"], 1);
        assert_eq!(starts[1]["content_block"]["id"], "call_b");
        let deltas = of_kind(&events, "content_block_delta");
        assert_eq!(deltas.len(), 4);
        assert_eq!(deltas[0]["index"], 0);
        assert_eq!(
            deltas[0]["delta"],
            json!({"type": "input_json_delta", "partial_json": "{\"cmd\":"})
        );
        assert_eq!(deltas[1]["index"], 1);
        let stops = of_kind(&events, "content_block_stop");
        assert_eq!(stops.len(), 2);
        let final_event = events.last().expect("terminal event");
        assert_eq!(final_event["type"], "message_stop");
        assert_eq!(
            of_kind(&events, "message_delta")[0]["delta"]["stop_reason"],
            "tool_use"
        );

        let message = stream.message().expect("captured message");
        assert_eq!(
            message["content"],
            json!([
                {"type": "tool_use", "id": "call_a", "name": "shell", "input": {"cmd": "ls"}},
                {"type": "tool_use", "id": "call_b", "name": "read_file", "input": {"path": "/tmp"}}
            ])
        );
    }

    #[test]
    fn ignores_reasoning_without_inventing_signed_thinking() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = Vec::new();
        events.extend(push_ok(&mut stream, created()));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "rs_0", "type": "reasoning", "summary": []}
            }),
        ));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": "rs_0",
                "output_index": 0,
                "summary_index": 0,
                "delta": "private chain of thought"
            }),
        ));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"id": "rs_0", "type": "reasoning", "summary": []}
            }),
        ));
        events.extend(push_ok(&mut stream, message_item(1, "msg_0")));
        events.extend(push_ok(&mut stream, text_delta_event(1, "msg_0", "answer")));
        events.extend(push_ok(&mut stream, message_done(1, "msg_0", "answer")));
        events.extend(push_ok(&mut stream, completed(json!([]), Value::Null)));

        assert_eq!(of_kind(&events, "content_block_start").len(), 1);
        assert_eq!(
            of_kind(&events, "content_block_start")[0]["index"],
            0,
            "ignored items must not consume Anthropic block indices"
        );
        let message = stream.message().expect("captured message");
        assert_eq!(
            message["content"],
            json!([{"type": "text", "text": "answer"}])
        );
    }

    #[test]
    fn maps_refusal_parts_to_text_blocks() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = Vec::new();
        events.extend(push_ok(&mut stream, created()));
        events.extend(push_ok(&mut stream, message_item(0, "msg_0")));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.refusal.delta",
                "item_id": "msg_0",
                "output_index": 0,
                "content_index": 0,
                "delta": "I cannot"
            }),
        ));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.content_part.done",
                "item_id": "msg_0",
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "refusal", "refusal": "I cannot"}
            }),
        ));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "msg_0",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "refusal", "refusal": "I cannot"}]
                }
            }),
        ));
        events.extend(push_ok(&mut stream, completed(json!([]), Value::Null)));

        assert_eq!(of_kind(&events, "content_block_start").len(), 1);
        let message = stream.message().expect("captured message");
        assert_eq!(
            message["content"],
            json!([{"type": "text", "text": "I cannot"}])
        );
    }

    #[test]
    fn a_failed_response_is_one_error_event_without_message_stop() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = push_ok(&mut stream, created());
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.failed",
                "response": {
                    "id": "resp_abc",
                    "object": "response",
                    "status": "failed",
                    "error": {"code": "server_error", "message": "upstream exploded"}
                }
            }),
        ));

        assert_eq!(kinds(&events), ["message_start", "error"]);
        assert_eq!(
            events[1]["error"],
            json!({"type": "api_error", "message": "upstream exploded"})
        );
        assert!(stream.is_terminal());
        assert!(stream.message().is_none());
        assert!(
            stream
                .push(
                    "response.output_text.delta",
                    "{\"type\":\"response.output_text.delta\",\"delta\":\"late\"}"
                )
                .unwrap()
                .is_empty(),
            "events after termination are ignored"
        );
    }

    #[test]
    fn a_stream_that_ends_early_becomes_an_error_event() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = push_ok(&mut stream, created());
        events.extend(push_ok(&mut stream, message_item(0, "msg_0")));
        events.extend(push_ok(
            &mut stream,
            text_delta_event(0, "msg_0", "partial"),
        ));

        let tail = stream.finish().expect("truncation is reported as an event");
        assert_eq!(kinds(&tail), ["content_block_stop", "error"]);
        assert!(stream.is_terminal());
        assert!(stream.message().is_none());
        assert!(stream.finish().expect("idempotent").is_empty());

        let mut untouched = MessagesStream::new("relay-model");
        assert!(
            untouched.finish().is_err(),
            "a stream with no message_start cannot be completed"
        );
    }

    #[test]
    fn malformed_tool_json_is_rejected_at_block_close() {
        let mut stream = MessagesStream::new("relay-model");
        push_ok(&mut stream, created());
        push_ok(&mut stream, tool_item(0, "call_a", "shell"));
        push_ok(&mut stream, tool_delta(0, "call_a", "{\"cmd\":"));
        let error = push(&mut stream, tool_done(0, "call_a", "{\"cmd\":"))
            .expect_err("truncated JSON must fail");
        assert!(error.contains("invalid JSON"), "{error}");

        let mut stream = MessagesStream::new("relay-model");
        push_ok(&mut stream, created());
        push_ok(&mut stream, tool_item(0, "call_a", "shell"));
        push_ok(&mut stream, tool_delta(0, "call_a", "[]"));
        let error =
            push(&mut stream, tool_done(0, "call_a", "[]")).expect_err("non-object JSON must fail");
        assert!(error.contains("not a JSON object"), "{error}");
    }

    #[test]
    fn tool_arguments_are_bounded() {
        let mut stream = MessagesStream::new("relay-model");
        push_ok(&mut stream, created());
        push_ok(&mut stream, tool_item(0, "call_a", "shell"));
        let oversized = "a".repeat(MAX_TOOL_ARGUMENT_BYTES + 1);
        let error = push(&mut stream, tool_delta(0, "call_a", &oversized))
            .expect_err("oversized tool JSON must fail");
        assert!(error.contains("limit"), "{error}");
    }

    #[test]
    fn duplicate_tool_ids_are_rejected() {
        let mut stream = MessagesStream::new("relay-model");
        push_ok(&mut stream, created());
        push_ok(&mut stream, tool_item(0, "call_same", "shell"));
        let error = push(&mut stream, tool_item(1, "call_same", "read_file"))
            .expect_err("a duplicate Anthropic tool_use id must fail");
        assert!(error.contains("duplicate tool call id"), "{error}");
    }

    #[test]
    fn incomplete_responses_report_anthropic_stop_reasons() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = push_ok(&mut stream, created());
        events.extend(push_ok(&mut stream, message_item(0, "msg_0")));
        events.extend(push_ok(
            &mut stream,
            text_delta_event(0, "msg_0", "partial"),
        ));
        events.extend(push_ok(&mut stream, message_done(0, "msg_0", "partial")));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.incomplete",
                "response": {
                    "id": "resp_abc",
                    "object": "response",
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "output": [],
                    "usage": {"input_tokens": 10, "output_tokens": 3}
                }
            }),
        ));

        assert_eq!(
            of_kind(&events, "message_delta")[0]["delta"]["stop_reason"],
            "max_tokens"
        );
        let message = stream
            .message()
            .expect("truncated messages are still captured");
        assert_eq!(message["stop_reason"], "max_tokens");
        assert_eq!(
            message["content"],
            json!([{"type": "text", "text": "partial"}])
        );
        assert_eq!(message["usage"]["input_tokens"], 10);
        assert_eq!(message["usage"]["output_tokens"], 3);

        let mut filtered = MessagesStream::new("relay-model");
        let mut events = push_ok(&mut filtered, created());
        events.extend(push_ok(
            &mut filtered,
            json!({
                "type": "response.incomplete",
                "response": {
                    "id": "resp_abc",
                    "status": "incomplete",
                    "incomplete_details": {"reason": "content_filter"},
                    "output": []
                }
            }),
        ));
        assert_eq!(
            of_kind(&events, "message_delta")[0]["delta"]["stop_reason"],
            "refusal"
        );
    }

    #[test]
    fn incomplete_open_tool_calls_are_closed_but_omitted() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = push_ok(&mut stream, created());
        events.extend(push_ok(&mut stream, message_item(0, "msg_0")));
        events.extend(push_ok(
            &mut stream,
            text_delta_event(0, "msg_0", "partial answer"),
        ));
        events.extend(push_ok(&mut stream, tool_item(1, "call_a", "shell")));
        events.extend(push_ok(&mut stream, tool_delta(1, "call_a", "{\"cmd\":")));
        events.extend(push_ok(
            &mut stream,
            json!({
                "type": "response.incomplete",
                "response": {
                    "id": "resp_abc",
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "output": [],
                    "usage": {"input_tokens": 4, "output_tokens": 2}
                }
            }),
        ));

        assert_eq!(
            kinds(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "the truncated tool block is still closed on the wire"
        );
        assert_eq!(
            of_kind(&events, "message_delta")[0]["delta"]["stop_reason"],
            "max_tokens"
        );
        let message = stream.message().expect("incomplete message is captured");
        assert_eq!(
            message["content"],
            json!([{"type": "text", "text": "partial answer"}]),
            "a cut-off tool call must not become an executable tool_use block"
        );
        assert_eq!(message["stop_reason"], "max_tokens");
    }

    #[test]
    fn an_empty_completed_response_gets_one_empty_text_block() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = push_ok(&mut stream, created());
        events.extend(push_ok(&mut stream, completed(json!([]), Value::Null)));

        assert_eq!(
            kinds(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(
            events[1]["content_block"],
            json!({"type": "text", "text": ""})
        );
        assert_eq!(
            events[3]["usage"],
            json!({
                "input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
                "output_tokens": 0
            })
        );
        let message = stream.message().expect("captured message");
        assert_eq!(message["content"], json!([{"type": "text", "text": ""}]));
        assert_eq!(message["stop_reason"], "end_turn");
    }

    #[test]
    fn rejects_malformed_sse_payloads() {
        let mut stream = MessagesStream::new("relay-model");
        assert!(
            stream
                .push("response.output_text.delta", "{not json")
                .is_err()
        );
        assert!(
            stream
                .push("response.output_text.delta", "\"a string\"")
                .is_err(),
            "a Responses event must be an object"
        );
    }

    #[test]
    fn terminal_only_output_is_still_captured() {
        let mut stream = MessagesStream::new("relay-model");
        let mut events = push_ok(&mut stream, created());
        events.extend(push_ok(
            &mut stream,
            completed(
                json!([
                    {
                        "id": "msg_0",
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "done", "annotations": []}]
                    },
                    {
                        "id": "fc_call_a",
                        "type": "function_call",
                        "status": "completed",
                        "call_id": "call_a",
                        "name": "shell",
                        "arguments": "{\"cmd\":\"ls\"}"
                    }
                ]),
                json!({"input_tokens": 5, "output_tokens": 2}),
            ),
        ));

        assert_eq!(
            kinds(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let message = stream.message().expect("captured message");
        assert_eq!(
            message["content"],
            json!([
                {"type": "text", "text": "done"},
                {"type": "tool_use", "id": "call_a", "name": "shell", "input": {"cmd": "ls"}}
            ])
        );
        assert_eq!(message["stop_reason"], "tool_use");
    }

    #[test]
    fn non_string_tool_arguments_are_rejected() {
        let mut stream = MessagesStream::new("relay-model");
        push_ok(&mut stream, created());
        let error = push(
            &mut stream,
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "fc_call_a",
                    "type": "function_call",
                    "call_id": "call_a",
                    "name": "shell",
                    "arguments": 5
                }
            }),
        )
        .expect_err("non-string arguments must fail");
        assert!(error.contains("arguments must be a string"), "{error}");
    }

    #[test]
    fn nonstream_capture_matches_the_streamed_message() {
        let response = json!({
            "id": "resp_xyz",
            "object": "response",
            "status": "completed",
            "model": "upstream-model",
            "output": [
                {
                    "id": "msg_0",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello", "annotations": []}]
                },
                {
                    "id": "fc_call_a",
                    "type": "function_call",
                    "status": "completed",
                    "call_id": "call_a",
                    "name": "shell",
                    "arguments": "{\"cmd\":\"ls\"}"
                }
            ],
            "usage": {
                "input_tokens": 50,
                "input_tokens_details": {"cached_tokens": 20},
                "output_tokens": 8
            }
        });

        let message = message_from_response(&response, "relay-model").expect("translated");
        assert_eq!(message["id"], "msg_xyz");
        assert_eq!(message["model"], "relay-model");
        assert_eq!(
            message["content"],
            json!([
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "call_a", "name": "shell", "input": {"cmd": "ls"}}
            ])
        );
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["usage"]["input_tokens"], 30);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 20);
        assert_eq!(message["usage"]["output_tokens"], 8);

        let wrapped = json!({
            "type": "response.completed",
            "response": response.clone()
        });
        assert_eq!(
            message_from_response(&wrapped, "relay-model").expect("event wrapper"),
            message
        );

        let failed = json!({
            "id": "resp_bad",
            "status": "failed",
            "error": {"code": "server_error", "message": "boom"},
            "output": []
        });
        let error = message_from_response(&failed, "relay-model").expect_err("failed response");
        assert!(error.contains("boom"), "{error}");

        let malformed = json!({
            "id": "resp_bad",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_a",
                "name": "shell",
                "arguments": "{oops"
            }]
        });
        let error = message_from_response(&malformed, "relay-model").expect_err("bad arguments");
        assert!(error.contains("invalid JSON"), "{error}");
    }
}
