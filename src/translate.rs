//! Predictable, reversible normalization between the Responses request Codex
//! emits and the flat, function-only tool surface the translated adapters
//! accept.
//!
//! Codex 0.171 sends three kinds of tools that neither the Anthropic Messages
//! nor the Chat Completions adapter translates directly:
//!
//! * hosted `web_search` and `tool_search` tools (dropped here; the relay also
//!   disables web search in the Codex launch),
//! * `namespace` tools such as `multi_agent_v1`, whose nested functions are
//!   flattened to `<namespace>__<name>`,
//! * `custom` (freeform) tools such as `apply_patch`, which become a function
//!   with one string parameter named `input`.
//!
//! The reverse mapping is applied to streamed output items, so Codex receives
//! the same `namespace` and `custom_tool_call` shapes it uses with its native
//! provider. The mapping is derived only from the request's tool list and the
//! item names, never from heuristic string splitting: a function named
//! `multi_agent_v1__close_agent` in the original request is therefore
//! distinguishable from a flattened nested `close_agent`.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::adapters::anthropic::THINKING_ENVELOPE_PREFIX;
use crate::provider::Wire;

/// Keep visible conversation and tool history across a stock Codex model
/// switch. Reasoning before the most recent boundary belongs to the old model.
/// A native Responses endpoint also cannot read an Anthropic relay envelope.
/// Returns true on the first request after the boundary, before any model output.
pub fn sanitize_replayed_history(request: &mut Value, wire: Wire) -> bool {
    let Some(input) = request.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    let switch = input.iter().rposition(is_model_switch);
    let first_request = switch.is_some_and(|index| {
        input[..index]
            .iter()
            .any(|item| item["role"] == "assistant")
            && input[index + 1..].iter().all(|item| {
                item["type"] == "message"
                    && matches!(item["role"].as_str(), Some("user" | "developer"))
            })
    });
    let mut position = 0;
    input.retain(|item| {
        let old_model = switch.is_some_and(|switch| position < switch);
        position += 1;
        let anthropic_envelope = wire == Wire::Responses
            && item["encrypted_content"]
                .as_str()
                .is_some_and(|value| value.starts_with(THINKING_ENVELOPE_PREFIX));
        item["type"] != "reasoning" || !(old_model || anthropic_envelope)
    });
    first_request
}

fn is_model_switch(item: &Value) -> bool {
    if item["type"] != "message" || item["role"] != "developer" {
        return false;
    }
    let has_marker = |text: &str| text.lines().any(|line| line.trim() == "<model_switch>");
    let content = &item["content"];
    content.as_str().is_some_and(has_marker)
        || content.as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part["text"].as_str().is_some_and(has_marker))
        })
}

/// Longest tool name the adapters accept.
const MAX_TOOL_NAME: usize = 128;
/// Separator between a namespace and its nested tool name.
pub const NAMESPACE_SEPARATOR: &str = "__";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Function,
    Custom,
}

/// How one upstream tool name maps back to Codex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolTarget {
    pub namespace: Option<String>,
    pub name: String,
    pub kind: ToolKind,
}

/// Upstream-name to Codex-shape mapping built while normalizing the request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolMap {
    targets: BTreeMap<String, ToolTarget>,
}

impl ToolMap {
    pub fn target(&self, upstream_name: &str) -> Option<&ToolTarget> {
        self.targets.get(upstream_name)
    }

    fn insert(&mut self, upstream: String, target: ToolTarget) -> Result<(), String> {
        if self.targets.contains_key(&upstream) {
            return Err(format!(
                "two tools normalize to the same upstream name \"{upstream}\"; rename one of them"
            ));
        }
        self.targets.insert(upstream, target);
        Ok(())
    }
}

/// Tools and items dropped or rewritten while normalizing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Normalized {
    pub map: ToolMap,
    /// Hosted or unsupported tool types that were removed.
    pub dropped_tools: Vec<String>,
}

/// Rewrite a Responses request in place so both translated adapters accept it.
///
/// The model name is deliberately left alone; the adapters replace it with the
/// vendor's upstream model id.
pub fn normalize_request(request: &mut Value) -> Result<Normalized, String> {
    let request = request
        .as_object_mut()
        .ok_or("the request body must be a JSON object")?;
    let mut normalized = Normalized::default();
    let tools = match request.get_mut("tools") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(tools)) => std::mem::take(tools),
        Some(_) => return Err("tools must be an array".to_owned()),
    };
    let mut flat = Vec::with_capacity(tools.len());
    for (index, tool) in tools.into_iter().enumerate() {
        if let Some(tools) = normalize_tool(tool, index, &mut normalized)? {
            flat.extend(tools);
        }
    }
    if !flat.is_empty() {
        request.insert("tools".to_owned(), Value::Array(flat));
    } else {
        request.remove("tools");
    }
    normalize_tool_choice(request, &normalized.map)?;
    normalize_input(request, &mut normalized.map)?;
    Ok(normalized)
}

fn normalize_tool(
    tool: Value,
    index: usize,
    normalized: &mut Normalized,
) -> Result<Option<Vec<Value>>, String> {
    let tool = tool
        .as_object()
        .ok_or_else(|| format!("tool {index} must be a JSON object"))?;
    let kind = tool
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("tool {index} needs a type"))?;
    match kind {
        "function" => {
            let name = tool_name(tool, &format!("tool {index}"))?;
            record(&mut normalized.map, name, None, name, ToolKind::Function)?;
            Ok(Some(vec![Value::Object(tool.clone())]))
        }
        "custom" => {
            let name = tool_name(tool, &format!("custom tool {index}"))?;
            let description = tool.get("description").and_then(Value::as_str);
            record(&mut normalized.map, name, None, name, ToolKind::Custom)?;
            Ok(Some(vec![custom_as_function(name, description, None)]))
        }
        "namespace" => {
            let namespace = tool_name(tool, &format!("namespace tool {index}"))?;
            let namespace_description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let nested = tool
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("namespace tool \"{namespace}\" needs a tools array"))?;
            let mut flat = Vec::with_capacity(nested.len());
            for (nested_index, nested) in nested.iter().enumerate() {
                let nested = nested.as_object().ok_or_else(|| {
                    format!("namespace \"{namespace}\" tool {nested_index} must be an object")
                })?;
                let nested_kind = nested.get("type").and_then(Value::as_str).ok_or_else(|| {
                    format!("namespace \"{namespace}\" tool {nested_index} needs a type")
                })?;
                let nested_name = tool_name(
                    nested,
                    &format!("namespace \"{namespace}\" tool {nested_index}"),
                )?;
                let upstream = format!("{namespace}{NAMESPACE_SEPARATOR}{nested_name}");
                if upstream.len() > MAX_TOOL_NAME {
                    return Err(format!(
                        "namespace tool \"{namespace}\" produces an over-long flattened name"
                    ));
                }
                let description = nested_namespace_description(
                    nested.get("description").and_then(Value::as_str),
                    namespace_description,
                    namespace,
                );
                match nested_kind {
                    "function" => {
                        record(
                            &mut normalized.map,
                            &upstream,
                            Some(namespace),
                            nested_name,
                            ToolKind::Function,
                        )?;
                        let mut entry = Map::new();
                        entry.insert("type".into(), json!("function"));
                        entry.insert("name".into(), json!(upstream));
                        if let Some(description) = description {
                            entry.insert("description".into(), json!(description));
                        }
                        entry.insert(
                            "parameters".into(),
                            nested
                                .get("parameters")
                                .cloned()
                                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                        );
                        if let Some(strict) = nested.get("strict").filter(|value| !value.is_null())
                        {
                            entry.insert("strict".into(), strict.clone());
                        }
                        flat.push(Value::Object(entry));
                    }
                    "custom" => {
                        record(
                            &mut normalized.map,
                            &upstream,
                            Some(namespace),
                            nested_name,
                            ToolKind::Custom,
                        )?;
                        flat.push(custom_as_function(&upstream, description.as_deref(), None));
                    }
                    other => {
                        return Err(format!(
                            "namespace \"{namespace}\" contains an unsupported {other} tool"
                        ));
                    }
                }
            }
            Ok(Some(flat))
        }
        hosted => {
            // Codex's hosted search tools cannot run through a translated
            // vendor. They are excluded from the launch, and any stale request
            // is stripped rather than mistranslated.
            normalized.dropped_tools.push(hosted.to_owned());
            Ok(None)
        }
    }
}

fn nested_namespace_description(
    nested: Option<&str>,
    group: &str,
    namespace: &str,
) -> Option<String> {
    match nested {
        Some(text) if !text.is_empty() => Some(format!("[{namespace}] {text}")),
        _ if !group.trim().is_empty() => Some(format!("[{namespace}] {group}")),
        _ => Some(format!(
            "[{namespace}] Tool from the {namespace} tools group"
        )),
    }
}

fn tool_name<'a>(tool: &'a Map<String, Value>, path: &str) -> Result<&'a str, String> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("{path} needs a name"))?;
    if name.len() > MAX_TOOL_NAME {
        return Err(format!("{path} has an over-long name"));
    }
    Ok(name)
}

fn record(
    map: &mut ToolMap,
    upstream: &str,
    namespace: Option<&str>,
    name: &str,
    kind: ToolKind,
) -> Result<(), String> {
    map.insert(
        upstream.to_owned(),
        ToolTarget {
            namespace: namespace.map(str::to_owned),
            name: name.to_owned(),
            kind,
        },
    )
}

fn custom_as_function(name: &str, description: Option<&str>, namespace: Option<&str>) -> Value {
    let mut entry = Map::new();
    entry.insert("type".into(), json!("function"));
    entry.insert("name".into(), json!(name));
    let mut description = description.unwrap_or("Freeform tool input").to_owned();
    if let Some(namespace) = namespace {
        description = format!("[{namespace}] {description}");
    }
    description.push_str(" Pass the complete freeform tool input in the `input` string.");
    entry.insert("description".into(), json!(description));
    entry.insert(
        "parameters".into(),
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The complete freeform tool input."
                }
            },
            "required": ["input"],
            "additionalProperties": false
        }),
    );
    Value::Object(entry)
}

fn normalize_tool_choice(request: &mut Map<String, Value>, map: &ToolMap) -> Result<(), String> {
    let Some(choice) = request.get_mut("tool_choice") else {
        return Ok(());
    };
    if choice.is_null() {
        return Ok(());
    }
    let Some(object) = choice.as_object_mut() else {
        // "auto", "none" and "required" are wire-neutral.
        return Ok(());
    };
    let Some(kind) = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(());
    };
    if kind != "function" && kind != "custom" {
        return Ok(());
    }
    let Some(name) = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Err("a tool_choice object needs a name".to_owned());
    };
    if !map.targets.contains_key(&name) {
        return Err(format!(
            "tool_choice names \"{name}\", which is not one of the translated tools"
        ));
    }
    object.insert("type".into(), json!("function"));
    Ok(())
}

fn normalize_input(request: &mut Map<String, Value>, map: &mut ToolMap) -> Result<(), String> {
    let Some(input) = request.get_mut("input") else {
        return Ok(());
    };
    let Value::Array(items) = input else {
        return Ok(());
    };
    for (index, item) in items.iter_mut().enumerate() {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        let Some(kind) = object
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        match kind.as_str() {
            "custom_tool_call" => {
                let name = object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| format!("input[{index}].name is required"))?
                    .to_owned();
                let input = object
                    .get("input")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("input[{index}].input must be a string"))?
                    .to_owned();
                let upstream = flattened_name(&name, object, map);
                if map.target(&upstream).is_none() {
                    map.insert(
                        upstream.clone(),
                        ToolTarget {
                            namespace: object
                                .get("namespace")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            name: name.clone(),
                            kind: ToolKind::Custom,
                        },
                    )?;
                }
                object.insert("type".into(), json!("function_call"));
                object.insert("name".into(), json!(upstream));
                object.insert(
                    "arguments".into(),
                    json!(json!({ "input": input }).to_string()),
                );
                object.remove("namespace");
            }
            "custom_tool_call_output" => {
                object.insert("type".into(), json!("function_call_output"));
            }
            "function_call" => {
                if let Some(namespace) = object.get("namespace").and_then(Value::as_str) {
                    let nested = object
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("input[{index}].name is required"))?;
                    let upstream =
                        if nested.starts_with(&format!("{namespace}{NAMESPACE_SEPARATOR}")) {
                            nested.to_owned()
                        } else {
                            format!("{namespace}{NAMESPACE_SEPARATOR}{nested}")
                        };
                    if upstream.len() > MAX_TOOL_NAME {
                        return Err(format!(
                            "input[{index}] produces an over-long flattened name"
                        ));
                    }
                    object.insert("name".into(), json!(upstream));
                    object.remove("namespace");
                }
            }
            "additional_tools" => {
                return Err(format!(
                    "input[{index}] is an additional_tools item, which translated providers do not support; disable code-mode/dynamic tools for this model"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn flattened_name(name: &str, object: &Map<String, Value>, _map: &ToolMap) -> String {
    if let Some(namespace) = object.get("namespace").and_then(Value::as_str)
        && !namespace.is_empty()
        && !name.starts_with(&format!("{namespace}{NAMESPACE_SEPARATOR}"))
    {
        return format!("{namespace}{NAMESPACE_SEPARATOR}{name}");
    }
    name.to_owned()
}

// ---------------------------------------------------------------------------
// Streamed output rewriting
// ---------------------------------------------------------------------------

/// Rewrites translated stream events into the exact Responses shapes Codex
/// uses for namespace and freeform tools, and renumbers events monotonically so
/// a synthetic failure can follow translated events safely.
#[derive(Debug)]
pub struct EventRewriter {
    map: ToolMap,
    custom_items: BTreeMap<String, (String, Option<String>, String)>,
    next_sequence: u64,
}

impl EventRewriter {
    pub fn new(map: ToolMap) -> Self {
        Self {
            map,
            custom_items: BTreeMap::new(),
            next_sequence: 0,
        }
    }

    pub fn sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        sequence
    }

    pub fn rewrite(&mut self, event: Value) -> Result<Vec<Value>, String> {
        let mut event = event;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let result = match kind.as_str() {
            "response.output_item.added" => {
                let custom = event
                    .get("item")
                    .and_then(|item| item.get("name"))
                    .and_then(Value::as_str)
                    .and_then(|name| self.map.target(name))
                    .is_some_and(|target| target.kind == ToolKind::Custom);
                if let Some(item) = event.get_mut("item") {
                    self.rewrite_item(item, true)?;
                }
                let _ = custom;
                vec![event]
            }
            "response.output_item.done" => {
                let custom = event
                    .get("item")
                    .and_then(|item| item.get("name"))
                    .and_then(Value::as_str)
                    .and_then(|name| self.map.target(name))
                    .is_some_and(|target| target.kind == ToolKind::Custom);
                if let Some(item) = event.get_mut("item") {
                    self.rewrite_item(item, false)?;
                }
                if custom {
                    let (item_id, input) = event
                        .get("item")
                        .map(|item| {
                            (
                                item.get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                item.get("input")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                            )
                        })
                        .unwrap_or_default();
                    let sequence = self.sequence();
                    let mut done = custom_event(
                        "response.custom_tool_call_input.done",
                        &item_id,
                        "input",
                        &input,
                    );
                    if let Some(object) = done.as_object_mut() {
                        object.insert("sequence_number".into(), json!(sequence));
                    }
                    copy_output_index(&event, &mut done);
                    vec![done, event]
                } else {
                    vec![event]
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if self.custom_items.contains_key(item_id) {
                    Vec::new()
                } else {
                    vec![event]
                }
            }
            "response.function_call_arguments.done" => {
                let item_id = event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if self.custom_items.contains_key(&item_id) {
                    let arguments = event
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    let input = custom_argument_input(arguments)?;
                    let sequence = self.sequence();
                    let mut delta = custom_event(
                        "response.custom_tool_call_input.delta",
                        &item_id,
                        "delta",
                        &input,
                    );
                    if let Some(object) = delta.as_object_mut() {
                        object.insert("sequence_number".into(), json!(sequence));
                    }
                    copy_output_index(&event, &mut delta);
                    vec![delta]
                } else {
                    vec![event]
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(output) = event
                    .get_mut("response")
                    .and_then(|response| response.get_mut("output"))
                    .and_then(Value::as_array_mut)
                {
                    for item in output.iter_mut() {
                        rewrite_item_value(item, &self.map)?;
                    }
                }
                vec![event]
            }
            _ => vec![event],
        };
        let mut result = result;
        for event in &mut result {
            if let Some(object) = event.as_object_mut() {
                object.insert("sequence_number".into(), json!(self.sequence()));
            }
        }
        Ok(result)
    }

    fn rewrite_item(&mut self, item: &mut Value, added: bool) -> Result<(), String> {
        let Some(object) = item.as_object_mut() else {
            return Ok(());
        };
        let Some(name) = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Ok(());
        };
        let Some(target) = self.map.target(&name).cloned() else {
            return Ok(());
        };
        if let Some(namespace) = &target.namespace {
            object.insert("namespace".into(), json!(namespace));
            object.insert("name".into(), json!(target.name));
        }
        if target.kind == ToolKind::Custom {
            let item_id = object
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let arguments = object
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_owned();
            let input = if added {
                String::new()
            } else {
                custom_argument_input(&arguments)?
            };
            object.insert("type".into(), json!("custom_tool_call"));
            object.remove("arguments");
            object.insert("input".into(), json!(input));
            object.insert("name".into(), json!(target.name));
            self.custom_items.entry(item_id).or_insert((
                target.name.clone(),
                target.namespace.clone(),
                input,
            ));
        }
        Ok(())
    }
}

fn rewrite_item_value(item: &mut Value, map: &ToolMap) -> Result<(), String> {
    let Some(object) = item.as_object_mut() else {
        return Ok(());
    };
    let Some(name) = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(());
    };
    let Some(target) = map.target(&name) else {
        return Ok(());
    };
    if let Some(namespace) = &target.namespace {
        object.insert("namespace".into(), json!(namespace));
        object.insert("name".into(), json!(target.name));
    }
    if target.kind == ToolKind::Custom {
        let arguments = object
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}")
            .to_owned();
        let input = custom_argument_input(&arguments)?;
        object.insert("type".into(), json!("custom_tool_call"));
        object.remove("arguments");
        object.insert("input".into(), json!(input));
    }
    Ok(())
}

fn custom_argument_input(arguments: &str) -> Result<String, String> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|_| "a freeform tool call streamed invalid JSON arguments".to_owned())?;
    match value {
        Value::Object(mut object) => match object.remove("input") {
            Some(Value::String(input)) => Ok(input),
            Some(_) => Err("a freeform tool call needs a string `input` argument".to_owned()),
            None if object.is_empty() => Ok(String::new()),
            None => Err("a freeform tool call is missing its `input` argument".to_owned()),
        },
        Value::String(input) => Ok(input),
        _ => Err("a freeform tool call must stream a JSON object with `input`".to_owned()),
    }
}

fn custom_event(kind: &str, item_id: &str, field: &str, value: &str) -> Value {
    let mut event = Map::new();
    event.insert("type".into(), json!(kind));
    event.insert("item_id".into(), json!(item_id));
    event.insert(field.to_owned(), json!(value));
    Value::Object(event)
}

fn copy_output_index(from: &Value, to: &mut Value) {
    if let Some(index) = from.get("output_index")
        && let Some(object) = to.as_object_mut()
    {
        object.insert("output_index".into(), index.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_switch_keeps_visible_history_and_current_reasoning() {
        let mut request = json!({"input": [
            {"type": "reasoning", "encrypted_content": "opaque-old"},
            {"type": "message", "role": "assistant", "content": "previous result"},
            {"type": "function_call_output", "call_id": "c1", "output": "prior tool result"},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<model_switch>\nSwitch\n</model_switch>"}]},
            {"type": "message", "role": "user", "content": "continue"},
            {"type": "reasoning", "encrypted_content": "opaque-current"}
        ]});
        assert!(!sanitize_replayed_history(&mut request, Wire::Responses));
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["content"], "previous result");
        assert_eq!(input[1]["output"], "prior tool result");
        assert_eq!(input[4]["encrypted_content"], "opaque-current");
        request["input"].as_array_mut().unwrap().pop();
        assert!(sanitize_replayed_history(&mut request, Wire::Responses));
    }

    #[test]
    fn responses_only_removes_anthropic_envelopes_without_a_switch() {
        let mut request = json!({"input": [
            {"type": "reasoning", "encrypted_content": "native-opaque"},
            {"type": "reasoning", "encrypted_content": format!("{THINKING_ENVELOPE_PREFIX}opaque")},
            {"type": "message", "role": "user", "content": "<model_switch>"}
        ]});
        assert!(!sanitize_replayed_history(&mut request, Wire::Responses));
        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["encrypted_content"], "native-opaque");
    }

    fn request() -> Value {
        json!({
            "model": "alpha/claude",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ],
            "tools": [
                {"type": "function", "name": "exec_command", "parameters": {"type": "object"}},
                {"type": "custom", "name": "apply_patch", "description": "Patch files"},
                {
                    "type": "namespace",
                    "name": "multi_agent_v1",
                    "description": "Agents",
                    "tools": [
                        {"type": "function", "name": "spawn_agent", "parameters": {"type": "object", "properties": {}}},
                        {"type": "custom", "name": "send_input", "description": "Send text"}
                    ]
                },
                {"type": "web_search", "external_web_access": false}
            ],
            "tool_choice": "auto"
        })
    }

    #[test]
    fn flattens_namespaces_and_custom_tools_and_drops_hosted_search() {
        let mut request = request();
        let normalized = normalize_request(&mut request).unwrap();
        assert_eq!(normalized.dropped_tools, ["web_search"]);
        let names = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "exec_command",
                "apply_patch",
                "multi_agent_v1__spawn_agent",
                "multi_agent_v1__send_input"
            ]
        );
        let apply = &request["tools"][1];
        assert_eq!(apply["parameters"]["required"], json!(["input"]));
        assert_eq!(
            normalized.map.target("apply_patch").unwrap().kind,
            ToolKind::Custom
        );
        let spawn = normalized
            .map
            .target("multi_agent_v1__spawn_agent")
            .unwrap();
        assert_eq!(spawn.namespace.as_deref(), Some("multi_agent_v1"));
        assert_eq!(spawn.name, "spawn_agent");
    }

    #[test]
    fn custom_and_namespaced_history_is_flattened() {
        let mut request = request();
        request["input"] = json!([
            {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "apply_patch", "input": "*** Begin Patch\n"},
            {"type": "custom_tool_call_output", "id": "cto_1", "call_id": "call_1", "output": "done"},
            {"type": "function_call", "id": "fc_1", "call_id": "call_2", "namespace": "multi_agent_v1", "name": "spawn_agent", "arguments": "{}"},
            {"type": "function_call_output", "id": "fco_1", "call_id": "call_2", "output": "ok"}
        ]);
        normalize_request(&mut request).unwrap();
        let items = request["input"].as_array().unwrap();
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["name"], "apply_patch");
        assert_eq!(
            serde_json::from_str::<Value>(items[0]["arguments"].as_str().unwrap()).unwrap(),
            json!({"input": "*** Begin Patch\n"})
        );
        assert_eq!(items[1]["type"], "function_call_output");
        assert_eq!(items[2]["name"], "multi_agent_v1__spawn_agent");
        assert!(items[2].get("namespace").is_none());
    }

    #[test]
    fn additional_tools_items_are_rejected() {
        let mut request = request();
        request["input"] = json!([{"type": "additional_tools", "tools": []}]);
        let error = normalize_request(&mut request).unwrap_err();
        assert!(error.contains("additional_tools"), "{error}");
    }

    #[test]
    fn namespace_tool_calls_are_rewritten_back_to_the_namespace_shape() {
        let mut request = request();
        let normalized = normalize_request(&mut request).unwrap();
        let mut rewriter = EventRewriter::new(normalized.map);
        let events = rewriter
            .rewrite(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "fc_call_1",
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "multi_agent_v1__spawn_agent",
                    "arguments": "{}"
                }
            }))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["item"]["namespace"], "multi_agent_v1");
        assert_eq!(events[0]["item"]["name"], "spawn_agent");
        assert_eq!(events[0]["sequence_number"], 0);
    }

    #[test]
    fn custom_tool_calls_become_custom_items_with_input_and_no_argument_deltas() {
        let mut request = request();
        let normalized = normalize_request(&mut request).unwrap();
        let mut rewriter = EventRewriter::new(normalized.map);
        let added = rewriter
            .rewrite(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "fc_call_9",
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": "call_9",
                    "name": "apply_patch",
                    "arguments": ""
                }
            }))
            .unwrap();
        assert_eq!(added[0]["item"]["type"], "custom_tool_call");
        assert!(
            rewriter
                .rewrite(json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": "fc_call_9",
                    "delta": "{\"in"
                }))
                .unwrap()
                .is_empty()
        );
        let done = rewriter
            .rewrite(json!({
                "type": "response.function_call_arguments.done",
                "item_id": "fc_call_9",
                "output_index": 0,
                "arguments": "{\"input\":\"*** Begin Patch\"}"
            }))
            .unwrap();
        assert_eq!(done[0]["type"], "response.custom_tool_call_input.delta");
        assert_eq!(done[0]["delta"], "*** Begin Patch");
        let item = rewriter
            .rewrite(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "fc_call_9",
                    "type": "function_call",
                    "call_id": "call_9",
                    "name": "apply_patch",
                    "arguments": "{\"input\":\"*** Begin Patch\"}"
                }
            }))
            .unwrap();
        let kinds = item
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "response.custom_tool_call_input.done",
                "response.output_item.done"
            ]
        );
        assert_eq!(item[1]["item"]["type"], "custom_tool_call");
        assert_eq!(item[1]["item"]["input"], "*** Begin Patch");
        assert_eq!(item[1]["item"]["name"], "apply_patch");
    }

    #[test]
    fn terminal_output_items_are_rewritten_too() {
        let mut request = request();
        let normalized = normalize_request(&mut request).unwrap();
        let mut rewriter = EventRewriter::new(normalized.map);
        let events = rewriter
            .rewrite(json!({
                "type": "response.completed",
                "response": {
                    "output": [
                        {"type": "function_call", "call_id": "c1", "name": "multi_agent_v1__spawn_agent", "arguments": "{}"},
                        {"type": "function_call", "call_id": "c2", "name": "apply_patch", "arguments": "{\"input\":\"x\"}"}
                    ]
                }
            }))
            .unwrap();
        let output = &events[0]["response"]["output"];
        assert_eq!(output[0]["namespace"], "multi_agent_v1");
        assert_eq!(output[0]["name"], "spawn_agent");
        assert_eq!(output[1]["type"], "custom_tool_call");
        assert_eq!(output[1]["input"], "x");
    }
}
