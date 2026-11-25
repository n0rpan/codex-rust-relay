//! Notifications for a specific thread. Deltas carry only newly received text.
use super::{items::ThreadItem, names::Notice};
use serde::Deserialize;
use serde_json::{Value, from_value};
use std::io;

#[derive(Debug)]
pub struct Activity {
    pub thread: String,
    pub turn: String,
    pub change: Change,
}
#[derive(Debug)]
pub enum Change {
    TurnStarted,
    TurnCompleted {
        error: Option<String>,
        /// The turn stopped because the user interrupted it, not because it finished.
        interrupted: bool,
    },
    Item {
        row: ThreadItem,
        completed: bool,
    },
    Delta {
        item: String,
        target: Target,
        text: String,
    },
}
pub use crate::transcript::DeltaTarget as Target;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lifecycle {
    thread_id: String,
    turn: Turn,
}
#[derive(Deserialize)]
struct Turn {
    id: String,
    status: String,
    error: Option<TurnError>,
}
#[derive(Deserialize)]
struct TurnError {
    message: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemEvent {
    thread_id: String,
    turn_id: String,
    item: Value,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delta {
    thread_id: String,
    turn_id: String,
    item_id: String,
    #[serde(default)]
    delta: String,
    #[serde(default)]
    summary_index: usize,
}

impl Activity {
    pub fn handles(name: Notice) -> bool {
        matches!(
            name,
            Notice::TurnStarted
                | Notice::TurnCompleted
                | Notice::Started
                | Notice::Completed
                | Notice::Text
                | Notice::PlanText
                | Notice::CommandOutput
                | Notice::FileOutput
                | Notice::ReasoningSummary
                | Notice::ReasoningPart
        )
    }
    pub fn decode(name: Notice, params: Value) -> io::Result<Self> {
        match name {
            Notice::TurnStarted | Notice::TurnCompleted => {
                let Lifecycle { thread_id, turn } = from_value(params)?;
                let change = if name == Notice::TurnStarted {
                    Change::TurnStarted
                } else {
                    Change::TurnCompleted {
                        interrupted: turn.status == "interrupted",
                        error: if turn.status == "failed" {
                            Some(
                                turn.error
                                    .map(|e| e.message)
                                    .unwrap_or_else(|| "Codex turn failed".into()),
                            )
                        } else {
                            None
                        },
                    }
                };
                Ok(Self {
                    thread: thread_id,
                    turn: turn.id,
                    change,
                })
            }
            Notice::Started | Notice::Completed => {
                let ItemEvent {
                    thread_id,
                    turn_id,
                    item,
                } = from_value(params)?;
                let row = ThreadItem::decode(&item).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Codex item omitted its ID")
                })?;
                Ok(Self {
                    thread: thread_id,
                    turn: turn_id,
                    change: Change::Item {
                        row,
                        completed: name == Notice::Completed,
                    },
                })
            }
            _ => {
                let d: Delta = from_value(params)?;
                let target = match name {
                    Notice::Text => Target::Agent,
                    Notice::PlanText => Target::Plan,
                    Notice::CommandOutput => Target::Command,
                    Notice::FileOutput => Target::File,
                    Notice::ReasoningSummary | Notice::ReasoningPart => {
                        Target::Summary(d.summary_index)
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Not a transcript notification",
                        ));
                    }
                };
                Ok(Self {
                    thread: d.thread_id,
                    turn: d.turn_id,
                    change: Change::Delta {
                        item: d.item_id,
                        target,
                        text: d.delta,
                    },
                })
            }
        }
    }
}
