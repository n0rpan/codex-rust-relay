//! Claude Code's stdio control protocol and model-independent transcript updates.
use super::{invalid, required};
use crate::{
    asks::{Ask, Choice, Decision, Question},
    runtime::RequestId,
    transcript::item::Item,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    io,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize)]
pub struct Scope {
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Init {
    pub session_id: String,
    pub cwd: String,
    pub model: String,
    #[serde(rename = "permissionMode")]
    pub permission_mode: String,
    pub tools: Vec<String>,
    pub slash_commands: Vec<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeltaTarget {
    Text,
    Thinking,
}

#[derive(Clone, Debug)]
pub enum Event {
    Init(Init),
    Title {
        session_id: String,
        name: String,
    },
    ControlReply(ControlReply),
    Ask(Permission),
    CancelAsk(String),
    UnsupportedRequest {
        request_id: String,
        subtype: String,
    },
    Item {
        scope: Scope,
        id: String,
        item: Item,
        completed: bool,
        output: Option<String>,
    },
    Delta {
        scope: Scope,
        id: String,
        target: DeltaTarget,
        text: String,
    },
    Result(TurnResult),
    /// The model that produced a top-level assistant message, as the stream reports it.
    /// Subagent messages are excluded: they may run another model by design.
    Model {
        session_id: String,
        model: String,
    },
}
#[derive(Clone, Debug, Deserialize)]
pub struct TurnResult {
    pub session_id: String,
    pub subtype: String,
    pub is_error: bool,
    #[serde(default)]
    pub errors: Vec<String>,
    pub stop_reason: Option<String>,
    /// Per-model usage, including the context window Claude Code used.
    #[serde(default, rename = "modelUsage")]
    pub model_usage: std::collections::BTreeMap<String, ModelUsage>,
}
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModelUsage {
    #[serde(default, rename = "contextWindow")]
    pub context_window: Option<u64>,
}
#[derive(Clone, Debug)]
pub struct ControlReply {
    pub request_id: String,
    pub payload: Result<Value, String>,
}
impl ControlReply {
    pub fn capabilities(&self) -> Option<Capabilities> {
        serde_json::from_value(self.payload.as_ref().ok()?.clone()).ok()
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct Capabilities {
    pub claude_code_version: Option<String>,
    pub current_permission_mode: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub commands: Vec<Command>,
    pub models: Vec<Model>,
    pub account: Account,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Command {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "argumentHint", default)]
    pub argument_hint: String,
    #[serde(default)]
    pub builtin: bool,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub value: String,
    pub display_name: String,
    pub resolved_model: Option<String>,
    #[serde(default)]
    pub supported_effort_levels: Vec<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub email: Option<String>,
    pub subscription_type: Option<String>,
    pub token_source: Option<String>,
    pub api_key_source: Option<String>,
    pub api_provider: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Permission {
    pub request_id: String,
    pub tool_use_id: String,
    pub agent_id: Option<String>,
    pub parent_tool_use_id: Option<String>,
    pub tool: String,
    pub description: Option<String>,
    pub input: Box<Value>,
    pub questions: Vec<Question>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireQuestion {
    question: String,
    header: String,
    multi_select: bool,
    options: Vec<Choice>,
}
impl Permission {
    pub(super) fn parse(row: &Value, request_id: String) -> io::Result<Self> {
        let r = &row["request"];
        let tool = required(r, "tool_name")?;
        let input = r
            .get("input")
            .filter(|v| v.is_object())
            .ok_or_else(|| invalid("Missing tool input"))?
            .clone();
        let mut questions = Vec::new();
        if tool == "AskUserQuestion" {
            let wire = Vec::<WireQuestion>::deserialize(&input["questions"])
                .map_err(|_| invalid("Invalid Claude question form"))?;
            let mut ids = HashSet::new();
            if wire.is_empty()
                || wire
                    .iter()
                    .any(|q| q.question.is_empty() || !ids.insert(&q.question))
            {
                return Err(invalid("Missing or duplicate Claude question"));
            }
            for q in wire {
                questions.push(Question {
                    id: q.question.clone(),
                    question: q.question,
                    header: q.header,
                    is_other: true,
                    is_secret: false,
                    multi_select: q.multi_select,
                    options: Some(q.options),
                });
            }
        }
        Ok(Self {
            request_id,
            tool_use_id: required(r, "tool_use_id")?,
            agent_id: r["agent_id"].as_str().map(str::to_owned),
            parent_tool_use_id: row["parent_tool_use_id"].as_str().map(str::to_owned),
            tool,
            description: r["description"]
                .as_str()
                .or(r["decision_reason"].as_str())
                .map(str::to_owned),
            input: Box::new(input),
            questions,
        })
    }
    pub fn allow(&self) -> Value {
        json!({"behavior":"allow","updatedInput":self.input})
    }
    pub fn deny(&self, reason: &str) -> Value {
        json!({"behavior":"deny","message":reason})
    }
    /// The SDK keys answers by the complete question. Multiple choices are joined by ", ".
    pub fn answer(&self, values: &BTreeMap<String, String>) -> io::Result<Value> {
        if self.questions.is_empty()
            || values.len() != self.questions.len()
            || self
                .questions
                .iter()
                .any(|q| values.get(&q.id).is_none_or(|s| s.trim().is_empty()))
        {
            return Err(invalid("Choose an answer for every Claude question"));
        }
        let mut reply = self.allow();
        reply["updatedInput"]["answers"] = json!(values);
        Ok(reply)
    }
    pub fn as_ask(&self, scoped_id: RequestId, display_thread: String) -> Ask {
        // Keep the action's actual arguments visible for informed consent.
        let mut detail = self.tool.clone();
        if let Some(text) = &self.description {
            detail.push('\n');
            detail.push_str(text);
        }
        if self.questions.is_empty() {
            detail.push('\n');
            detail.push_str(&serde_json::to_string_pretty(&self.input).unwrap_or_default());
        }
        Ask {
            command: (self.tool == "Bash")
                .then(|| self.input["command"].as_str())
                .flatten()
                .map(|command| crate::asks::CommandApproval {
                    command: command.to_owned(),
                    cwd: None,
                    decline: 1,
                }),
            id: scoped_id,
            thread: Some(display_thread),
            turn: None,
            item: Some(self.tool_use_id.clone()),
            title: if self.questions.is_empty() {
                "Allow Claude tool call"
            } else {
                "Claude has a question"
            }
            .into(),
            detail,
            questions: self.questions.clone(),
            decisions: if self.questions.is_empty() {
                vec![
                    Decision::new("Allow once", self.allow()),
                    Decision::new("Deny", self.deny("Denied by user")),
                ]
            } else {
                vec![Decision::new("Deny", self.deny("Denied by user"))]
            },
        }
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    #[test]
    fn a_turn_result_reports_the_context_window_per_model() {
        let r: TurnResult = serde_json::from_value(serde_json::json!({
            "type":"result","subtype":"success","is_error":false,"session_id":"s","stop_reason":"end_turn",
            "modelUsage":{"claude-sonnet":{"inputTokens":3,"contextWindow":200000},"haiku":{"inputTokens":1}}
        }))
        .unwrap();
        assert_eq!(r.model_usage["claude-sonnet"].context_window, Some(200_000));
        assert_eq!(r.model_usage["haiku"].context_window, None);
        let old: TurnResult = serde_json::from_value(serde_json::json!({
            "subtype":"success","is_error":false,"session_id":"s","stop_reason":null
        }))
        .unwrap();
        assert!(old.model_usage.is_empty());
    }
}
