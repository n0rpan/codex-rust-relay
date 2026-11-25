//! Translate stock Codex item names and fields into the shared transcript model.
use crate::transcript::item::Item;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

fn field<T: DeserializeOwned>(input: &Value, key: &str) -> Option<T> {
    T::deserialize(&input[key]).ok()
}

fn decode_item(input: &Value) -> Option<Item> {
    Some(match input.get("type")?.as_str()? {
        "userMessage" => Item::UserMessage {
            content: field(input, "content")?,
        },
        "agentMessage" => Item::AgentMessage {
            text: field(input, "text")?,
            is_update: input["phase"] == "commentary",
        },
        "reasoning" => Item::Reasoning {
            summary: match input.get("summary") {
                Some(v) => Vec::deserialize(v).ok()?,
                None => vec![],
            },
        },
        "commandExecution" => Item::CommandExecution {
            cwd: input["cwd"].as_str().map(Into::into),
            command: field(input, "command")?,
            status: field(input, "status")?,
            aggregated_output: field(input, "aggregatedOutput")?,
            exit_code: field(input, "exitCode")?,
        },
        "fileChange" => Item::FileChange {
            status: field(input, "status")?,
            changes: field(input, "changes")?,
        },
        "collabAgentToolCall" => Item::AgentOperation {
            tool: field(input, "tool")?,
            status: field(input, "status")?,
            sender_thread_id: field(input, "senderThreadId")?,
            receiver_thread_ids: match input.get("receiverThreadIds") {
                Some(v) => Vec::deserialize(v).ok()?,
                None => vec![],
            },
        },
        "mcpToolCall" => Item::McpToolCall {
            status: field(input, "status")?,
            server: field(input, "server")?,
            tool: field(input, "tool")?,
        },
        "dynamicToolCall" => Item::ToolCall {
            status: field(input, "status")?,
            tool: field(input, "tool")?,
        },
        "webSearch" => Item::WebSearch {
            query: field(input, "query")?,
        },
        "plan" => Item::Plan {
            text: field(input, "text")?,
        },
        _ => return None,
    })
}

#[derive(Clone, Debug)]
pub struct ThreadItem {
    pub id: String,
    pub item: Item,
}

impl ThreadItem {
    pub fn decode(input: &Value) -> Option<Self> {
        let id = input.get("id")?.as_str()?.to_owned();
        // Future item types and unfamiliar shapes stay visible as a generic row.
        let item = decode_item(input).unwrap_or_else(|| Item::Unknown {
            kind: input
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("Unknown item")
                .into(),
            status: input
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
        });
        Some(Self { id, item })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn assistant_updates_and_reasoning_summaries_cross_the_view_boundary() {
        let update = ThreadItem::decode(
            &json!({"id":"a","type":"agentMessage","text":"Reading","phase":"commentary"}),
        )
        .unwrap();
        assert!(matches!(
            update.item,
            Item::AgentMessage {
                is_update: true,
                ..
            }
        ));
        let reasoning = ThreadItem::decode(
            &json!({"id":"b","type":"reasoning","summary":["A summary"],"content":["internal"]}),
        )
        .unwrap();
        assert!(matches!(reasoning.item,Item::Reasoning {summary} if summary==["A summary"]));
    }

    #[test]
    fn command_fields_survive_and_future_types_are_visible() {
        let known = ThreadItem::decode(&json!({"id":"a","type":"commandExecution","command":"pwd","cwd":"/demo","status":"completed","exitCode":0,"aggregatedOutput":"/demo"})).unwrap();
        assert!(
            matches!(known.item, Item::CommandExecution { exit_code: Some(0), aggregated_output: Some(ref output), cwd:Some(ref dir), .. } if output == "/demo" && dir == std::path::Path::new("/demo"))
        );
        let future =
            ThreadItem::decode(&json!({"id":"a","type":"newTool","status":"running"})).unwrap();
        assert!(matches!(future.item, Item::Unknown { ref kind, .. } if kind == "newTool"));
    }
}
