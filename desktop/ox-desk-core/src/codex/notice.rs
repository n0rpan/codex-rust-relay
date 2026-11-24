//! Project and account notifications decoded on the reader.
use super::{access::Access, frame::RequestId, history::ThreadRow, names::Notice};
use crate::transcript::item::FileChange;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use std::io;

#[derive(Clone, Debug, Default, PartialEq, Deserialize, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Status {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, rename = "activeFlags")]
    pub flags: Vec<String>,
}
impl Status {
    pub fn label(&self) -> Option<&'static str> {
        match self.kind.as_str() {
            "active"
                if self
                    .flags
                    .iter()
                    .any(|f| matches!(f.as_str(), "waitingOnApproval" | "waitingOnUserInput")) =>
            {
                Some("Needs you")
            }
            "active" => Some("Running"),
            "systemError" => Some("Error"),
            _ => None,
        }
    }
}
#[derive(Debug, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
}
pub use crate::usage::{Limits, TokenUsage};
#[derive(Debug)]
pub struct EffectiveSettings {
    pub access: Access,
    pub model: String,
    pub provider: String,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
}
impl EffectiveSettings {
    pub(super) fn decode(p: &Value) -> Self {
        let string = |key: &str| p[key].as_str().map(str::to_owned);
        Self {
            access: Access::effective(p),
            model: string("model").unwrap_or_default(),
            provider: string("modelProvider").unwrap_or_default(),
            effort: string("effort"),
            service_tier: string("serviceTier"),
        }
    }
}
#[derive(Debug)]
pub enum Notification {
    Started(Box<ThreadRow>),
    Closed(String),
    Archived(String),
    Renamed {
        thread: String,
        name: Option<String>,
    },
    Status {
        thread: String,
        status: Status,
    },
    Settings {
        thread: String,
        settings: EffectiveSettings,
    },
    Resolved(RequestId),
    Diff {
        thread: String,
        turn: String,
        diff: String,
    },
    Plan {
        thread: String,
        turn: String,
        explanation: Option<String>,
        steps: Vec<PlanStep>,
    },
    FilePatch {
        thread: String,
        turn: String,
        item: String,
        changes: Vec<FileChange>,
    },
    Tokens {
        thread: String,
        turn: String,
        usage: TokenUsage,
    },
    RateLimits(Limits),
    Account {
        auth_mode: Option<String>,
    },
    Login {
        success: bool,
        error: Option<String>,
    },
    Error {
        thread: String,
        turn: String,
        message: String,
        will_retry: bool,
    },
}
fn take<T: DeserializeOwned>(p: &mut Value, key: &str) -> io::Result<T> {
    serde_json::from_value(p.get_mut(key).map(Value::take).unwrap_or_default())
        .map_err(io::Error::from)
}
impl Notification {
    pub(super) fn decode(method: Notice, mut p: Value) -> io::Result<Self> {
        Ok(match method {
            Notice::ThreadStarted => Self::Started(take(&mut p, "thread")?),
            Notice::ThreadClosed => Self::Closed(take(&mut p, "threadId")?),
            Notice::ThreadArchived => Self::Archived(take(&mut p, "threadId")?),
            Notice::ThreadName => Self::Renamed {
                thread: take(&mut p, "threadId")?,
                name: take(&mut p, "threadName")?,
            },
            Notice::ThreadStatus => Self::Status {
                thread: take(&mut p, "threadId")?,
                status: take(&mut p, "status")?,
            },
            Notice::ThreadSettings => Self::Settings {
                thread: take(&mut p, "threadId")?,
                settings: EffectiveSettings::decode(&p["threadSettings"]),
            },
            Notice::Resolved => Self::Resolved(take(&mut p, "requestId")?),
            Notice::Diff => Self::Diff {
                thread: take(&mut p, "threadId")?,
                turn: take(&mut p, "turnId")?,
                diff: take(&mut p, "diff")?,
            },
            Notice::Plan => Self::Plan {
                thread: take(&mut p, "threadId")?,
                turn: take(&mut p, "turnId")?,
                explanation: take(&mut p, "explanation")?,
                steps: take(&mut p, "plan")?,
            },
            Notice::FilePatch => Self::FilePatch {
                thread: take(&mut p, "threadId")?,
                turn: take(&mut p, "turnId")?,
                item: take(&mut p, "itemId")?,
                changes: take(&mut p, "changes")?,
            },
            Notice::Tokens => Self::Tokens {
                thread: take(&mut p, "threadId")?,
                turn: take(&mut p, "turnId")?,
                usage: take(&mut p, "tokenUsage")?,
            },
            Notice::RateLimits => Self::RateLimits(take(&mut p, "rateLimits")?),
            Notice::Account => Self::Account {
                auth_mode: take(&mut p, "authMode")?,
            },
            Notice::Login => Self::Login {
                success: take(&mut p, "success")?,
                error: take(&mut p, "error")?,
            },
            Notice::Error => Self::Error {
                thread: take(&mut p, "threadId")?,
                turn: take(&mut p, "turnId")?,
                message: take(&mut p["error"], "message")?,
                will_retry: take(&mut p, "willRetry")?,
            },
            _ => {
                return Err(io::Error::other(
                    "Expected a thread or account notification",
                ));
            }
        })
    }
}
