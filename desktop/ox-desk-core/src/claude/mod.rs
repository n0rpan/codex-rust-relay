//! Decode stock Claude Code stream-json and normalized, read-only history records.
pub mod catalog;
pub mod live;
mod events;
mod tasks;
mod tools;
use crate::transcript::item::{Content, Item};
pub use events::*;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, io};
use tools::Tool;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn required(row: &Value, key: &str) -> io::Result<String> {
    row[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid("Missing Claude stream identifier"))
}
fn parsed<'a, T: Deserialize<'a>>(value: &'a Value) -> io::Result<T> {
    T::deserialize(value).map_err(|_| invalid("Malformed Claude stream event"))
}

#[derive(Default)]
struct Message {
    id: String,
    next_index: u64,
    last_uuid: Option<String>,
}
#[derive(Default)]
pub struct Decoder {
    session: Option<String>,
    agents: HashMap<(String, String), tasks::Agent>,
    messages: HashMap<Scope, Message>,
    tools: HashMap<(Scope, String), Tool>,
}
impl Decoder {
    pub fn decode(&mut self, line: &[u8]) -> io::Result<Vec<Event>> {
        self.decode_value(
            serde_json::from_slice(line).map_err(|_| invalid("Invalid Claude stream JSON"))?,
        )
    }
    pub fn decode_value(&mut self, row: Value) -> io::Result<Vec<Event>> {
        let mut events = Vec::new();
        match row["type"]
            .as_str()
            .ok_or_else(|| invalid("Missing Claude frame type"))?
        {
            "system" if row["subtype"] == "session_title_changed" => {
                if let Some(name) = row["title"].as_str().filter(|n| !n.trim().is_empty()) {
                    events.push(Event::Title {
                        session_id: required(&row, "session_id")?,
                        name: name.chars().take(256).collect(),
                    });
                }
            }
            "system" if row["subtype"] == "init" => {
                let init: Init = parsed(&row)?;
                self.session = Some(init.session_id.clone());
                events.push(Event::Init(init));
            }
            "system" => self.task(&row, &mut events),
            "control_response" => {
                let r = &row["response"];
                let payload = match r["subtype"].as_str() {
                    Some("success") => Ok(r["response"].clone()),
                    Some("error") => Err(r["error"]
                        .as_str()
                        .unwrap_or("Claude rejected the control request")
                        .to_owned()),
                    _ => return Err(invalid("Unknown Claude control response")),
                };
                events.push(Event::ControlReply(ControlReply {
                    request_id: required(r, "request_id")?,
                    payload,
                }));
            }
            "control_request" => {
                let request_id = required(&row, "request_id")?;
                let subtype = required(&row["request"], "subtype")?;
                if subtype == "can_use_tool"
                    && let Ok(mut permission) = Permission::parse(&row, request_id.clone())
                {
                    if permission.parent_tool_use_id.is_none()
                        && let Some((session, agent)) =
                            self.session.as_ref().zip(permission.agent_id.as_ref())
                        && let Some(parent) = self.agents.get(&(session.clone(), agent.clone()))
                    {
                        permission.parent_tool_use_id = Some(parent.tool_id.clone());
                    }
                    events.push(Event::Ask(permission));
                } else {
                    // The transport can reject unsupported or malformed asks and keep reading.
                    events.push(Event::UnsupportedRequest {
                        request_id,
                        subtype,
                    });
                }
            }
            "control_cancel_request" => {
                events.push(Event::CancelAsk(required(&row, "request_id")?))
            }
            "result" => {
                events.push(Event::Result(parsed(&row)?));
            }
            "assistant" | "user" | "stream_event" => {
                let scope: Scope = parsed(&row)?;
                if scope.session_id.is_empty() {
                    return Err(invalid("Missing Claude session id"));
                }
                match row["type"].as_str() {
                    Some("stream_event") => self.partial(scope, &row, &mut events)?,
                    Some("assistant") => self.assistant(scope, &row, &mut events)?,
                    _ => self.user(scope, &row, &mut events)?,
                }
            }
            _ => {}
        }
        Ok(events)
    }
    fn message(&mut self, scope: &Scope, id: &str) -> &mut Message {
        let m = self.messages.entry(scope.clone()).or_default();
        if m.id != id {
            *m = Message {
                id: id.to_owned(),
                ..Default::default()
            };
        }
        m
    }
    fn partial(&mut self, scope: Scope, row: &Value, out: &mut Vec<Event>) -> io::Result<()> {
        let e = &row["event"];
        if e["type"] == "message_start" {
            self.message(&scope, &required(&e["message"], "id")?);
            return Ok(());
        }
        let Some(index) = e["index"].as_u64() else {
            return Ok(());
        };
        let msg = row["api_message_id"]
            .as_str()
            .or_else(|| self.messages.get(&scope).map(|m| m.id.as_str()));
        let Some(msg) = msg else { return Ok(()) };
        let id = format!("{msg}:{index}");
        match e["type"].as_str() {
            Some("content_block_start") => {
                let block = &e["content_block"];
                if let Some(item) = text_item(block) {
                    out.push(Event::Item {
                        scope,
                        id,
                        item,
                        completed: false,
                        output: None,
                    });
                }
            }
            Some("content_block_delta") => {
                let delta = &e["delta"];
                let (target, key) = match delta["type"].as_str() {
                    Some("text_delta") => (DeltaTarget::Text, "text"),
                    Some("thinking_delta") => (DeltaTarget::Thinking, "thinking"),
                    _ => return Ok(()),
                };
                if let Some(text) = delta[key].as_str().filter(|t| !t.is_empty()) {
                    out.push(Event::Delta {
                        scope,
                        id,
                        target,
                        text: text.to_owned(),
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn assistant(&mut self, scope: Scope, row: &Value, out: &mut Vec<Event>) -> io::Result<()> {
        let msg = &row["message"];
        let id = msg["id"]
            .as_str()
            .or(row["uuid"].as_str())
            .ok_or_else(|| invalid("Missing Claude message identity"))?;
        let blocks = msg["content"]
            .as_array()
            .ok_or_else(|| invalid("Malformed Claude assistant message"))?;
        let cursor = self.message(&scope, id);
        let uuid = row["uuid"].as_str();
        if uuid.is_some() && cursor.last_uuid.as_deref() == uuid {
            return Ok(());
        }
        cursor.last_uuid = uuid.map(str::to_owned);
        let start = cursor.next_index;
        cursor.next_index = cursor.next_index.saturating_add(blocks.len() as u64);
        for (index, b) in blocks.iter().enumerate() {
            if b["type"] == "tool_use" {
                let id = required(b, "id")?;
                let tool = Tool {
                    name: required(b, "name")?,
                    input: b["input"].clone(),
                };
                let item = tool.item(
                    scope
                        .parent_tool_use_id
                        .as_deref()
                        .unwrap_or(&scope.session_id),
                    &id,
                    "inProgress",
                    &Value::Null,
                    None,
                );
                self.tools.insert((scope.clone(), id.clone()), tool);
                out.push(Event::Item {
                    scope: scope.clone(),
                    id,
                    item,
                    completed: false,
                    output: None,
                });
            } else if let Some(item) = text_item(b) {
                out.push(Event::Item {
                    scope: scope.clone(),
                    id: format!("{id}:{}", start + index as u64),
                    item,
                    completed: true,
                    output: None,
                });
            }
        }
        Ok(())
    }
    fn user(&mut self, scope: Scope, row: &Value, out: &mut Vec<Event>) -> io::Result<()> {
        let content = &row["message"]["content"];
        if let Some(blocks) = content.as_array() {
            let n_results = blocks.iter().filter(|b| b["type"] == "tool_result").count();
            for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                let id = required(b, "tool_use_id")?;
                let tool = self
                    .tools
                    .remove(&(scope.clone(), id.clone()))
                    .unwrap_or_else(|| Tool {
                        name: "Tool result".into(),
                        input: Value::Null,
                    });
                let result = if n_results == 1 {
                    &row["tool_use_result"]
                } else {
                    &Value::Null
                };
                let async_agent = matches!(tool.name.as_str(), "Agent" | "Task")
                    && result["status"] == "async_launched";
                let status = if b["is_error"] == true {
                    "failed"
                } else if async_agent {
                    "inProgress"
                } else if result["interrupted"] == true {
                    "interrupted"
                } else {
                    "completed"
                };
                let output = tools::text(&b["content"]);
                let item = tool.item(
                    scope
                        .parent_tool_use_id
                        .as_deref()
                        .unwrap_or(&scope.session_id),
                    &id,
                    status,
                    result,
                    output.as_deref(),
                );
                out.push(Event::Item {
                    scope: scope.clone(),
                    id,
                    item,
                    completed: !async_agent,
                    output,
                });
            }
        }
        if let Some(text) = tools::text(content).filter(|s| !s.is_empty()) {
            let item = Item::UserMessage {
                content: vec![Content {
                    text: Some(text),
                    path: None,
                }],
            };
            out.push(Event::Item {
                scope,
                id: required(row, "uuid")?,
                item,
                completed: true,
                output: None,
            });
        }
        Ok(())
    }
}
fn text_item(b: &Value) -> Option<Item> {
    match b["type"].as_str()? {
        "text" => Some(Item::AgentMessage {
            text: b["text"].as_str()?.to_owned(),
            is_update: false,
        }),
        "thinking" => Some(Item::Reasoning {
            summary: vec![b["thinking"].as_str()?.to_owned()],
        }),
        _ => None,
    }
}
