//! Background agent acknowledgments precede the actual task completion.
use super::{Decoder, Event, Scope};
use crate::transcript::item::Item;
use serde_json::Value;

pub(super) struct Agent {
    pub scope: Scope,
    pub tool_id: String,
    label: String,
    run: Option<String>,
}
impl Decoder {
    pub(super) fn task(&mut self, row: &Value, out: &mut Vec<Event>) {
        let (Some(session), Some(task)) = (row["session_id"].as_str(), row["task_id"].as_str())
        else {
            return;
        };
        let key = (session.to_owned(), task.to_owned());
        let run = row["run_id"].as_str();
        match row["subtype"].as_str() {
            Some("task_started") if row["task_type"] == "local_agent" => {
                let Some(tool_id) = row["tool_use_id"].as_str() else {
                    return;
                };
                if self
                    .agents
                    .get(&key)
                    .and_then(|a| a.run.as_deref())
                    .zip(run)
                    .is_some_and(|(old, new)| new < old)
                {
                    return;
                }
                let parent = row["parent_task_id"]
                    .as_str()
                    .and_then(|id| self.agents.get(&(session.into(), id.into())))
                    .map(|agent| agent.tool_id.clone());
                let scope = Scope {
                    session_id: session.into(),
                    parent_tool_use_id: parent,
                };
                self.agents.insert(
                    key,
                    Agent {
                        scope,
                        tool_id: tool_id.into(),
                        label: row["description"].as_str().unwrap_or("Agent").to_owned(),
                        run: run.map(str::to_owned),
                    },
                );
            }
            Some("task_notification") => {
                let Some(agent) = self.agents.get(&key) else {
                    return;
                };
                if agent
                    .run
                    .as_deref()
                    .zip(run)
                    .is_some_and(|(existing, incoming)| existing != incoming)
                {
                    return;
                }
                let Some(status) = row["status"].as_str() else {
                    return;
                };
                if !matches!(status, "completed" | "failed" | "stopped") {
                    return;
                }
                let agent = self.agents.remove(&key).unwrap();
                let sender = agent.scope.parent_tool_use_id.as_deref().unwrap_or(session);
                let item = Item::AgentOperation {
                    tool: agent.label,
                    status: status.to_owned(),
                    sender_thread_id: sender.to_owned(),
                    receiver_thread_ids: vec![agent.tool_id.clone()],
                };
                out.push(Event::Item {
                    scope: agent.scope,
                    id: agent.tool_id,
                    item,
                    completed: true,
                    output: row["summary"].as_str().map(str::to_owned),
                });
            }
            _ => {}
        }
    }
}
