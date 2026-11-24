//! The reader resolves the wire ID before delivering a decoded reply to the UI.
use super::{
    history::{ReadThread, ResumedThread, ThreadPage, TurnPage},
    names::Method,
};
use serde::Deserialize;
use serde_json::{Value, from_value};

#[derive(Debug, Deserialize)]
pub struct AccountReply {
    account: Option<Account>,
}
#[derive(Debug, Deserialize)]
struct Account {
    #[serde(rename = "type")]
    kind: String,
}
impl AccountReply {
    pub fn chatgpt(&self) -> bool {
        self.account
            .as_ref()
            .is_some_and(|account| account.kind == "chatgpt")
    }
}

#[derive(Debug, Deserialize)]
pub struct TurnStatus {
    pub id: String,
    pub status: String,
}
#[derive(Deserialize)]
struct Started {
    turn: TurnStatus,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Steered {
    turn_id: String,
}
#[derive(Debug)]
pub enum Reply {
    Files(crate::mentions::Matches),
    Features(super::commands::Page<super::commands::Feature>),
    Models(super::commands::Page<super::commands::Model>),
    Config(super::commands::ProjectConfig),
    Goal(super::commands::ThreadGoal),
    Cleared(super::commands::Cleared),
    Job(Result<super::jobs::Response, String>),
    Opened(Box<ResumedThread>),
    Turn(TurnStatus),
    Steered(String),
    Acknowledged,
    UnsupportedVersion(String),
    Unsubscribed(super::commands::Unsubscribed),
    Account(AccountReply),
    Limits(Box<crate::usage::AccountLimits>),
    Read(ReadThread),
    Resume(Box<ResumedThread>),
    Threads(ThreadPage),
    Turns(TurnPage),
    Unknown(Value),
}
impl Reply {
    pub(super) fn decode(method: Option<Method>, value: Value) -> serde_json::Result<Self> {
        Ok(match method {
            Some(Method::Initialize) => value
                .get("userAgent")
                .and_then(Value::as_str)
                .and_then(super::version::unsupported)
                .map(Self::UnsupportedVersion)
                .unwrap_or(Self::Acknowledged),
            Some(
                Method::Interrupt
                | Method::Settings
                | Method::Archive
                | Method::Rename
                | Method::Compact,
            ) => Self::Acknowledged,
            Some(Method::FindFile) => Self::Files(from_value(value)?),
            Some(Method::CallTool) => Self::Job(super::jobs::result(&value)),
            Some(Method::Features) => Self::Features(from_value(value)?),
            Some(Method::Models) => Self::Models(from_value(value)?),
            Some(Method::Config) => Self::Config(from_value(value)?),
            Some(Method::GetGoal | Method::SetGoal) => Self::Goal(from_value(value)?),
            Some(Method::ClearGoal) => Self::Cleared(from_value(value)?),
            Some(Method::Limits) => Self::Limits(from_value(value)?),
            Some(Method::Account) => Self::Account(from_value(value)?),
            Some(Method::Unsubscribe) => Self::Unsubscribed(from_value(value)?),
            Some(Method::List) => Self::Threads(from_value(value)?),
            Some(Method::Read) => Self::Read(from_value(value)?),
            Some(Method::ThreadStart | Method::Fork) => Self::Opened(from_value(value)?),
            Some(Method::TurnStart) => Self::Turn(from_value::<Started>(value)?.turn),
            Some(Method::Steer) => Self::Steered(from_value::<Steered>(value)?.turn_id),
            Some(Method::Resume) => Self::Resume(from_value(value)?),
            Some(Method::Turns) => Self::Turns(from_value(value)?),
            _ => Self::Unknown(value),
        })
    }
}
