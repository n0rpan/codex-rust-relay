//! Shared device protocol. Only this versioned projection crosses the host boundary.
use crate::{
    actions::Action,
    asks::Question,
    runtime::{RequestId, Route},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Scope {
    pub project: PathBuf,
    pub route: Route,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Client {
    Hello {
        version: u32,
    },
    WatchCatalog,
    TerminalPreview {
        id: String,
        scope: Scope,
        thread: String,
    },
    Discover {
        id: String,
    },
    Subscribe {
        scope: Option<Scope>,
        after: Option<Cursor>,
    },
    Act {
        id: String,
        scope: Scope,
        action: Action,
        approval: Option<String>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Server {
    Opened {
        id: String,
        scope: Scope,
        thread: String,
    },
    Hello {
        version: u32,
        capabilities: Vec<String>,
        routes: Vec<Route>,
        projects: Vec<PathBuf>,
    },
    Discovered {
        id: String,
        scope: Scope,
        rows: Vec<SessionView>,
    },
    DiscoveredEnd {
        id: String,
        errors: Vec<String>,
    },
    Snapshot {
        cursor: Cursor,
        scopes: Vec<ScopeState>,
    },
    Delta {
        cursor: Cursor,
        scope: Scope,
        changes: Vec<Change>,
    },
    TerminalPreview {
        id: String,
        scope: Scope,
        thread: String,
        argv: Vec<String>,
    },
    Handoff {
        #[serde(default)]
        argv: Vec<String>,
        id: String,
        scope: Scope,
        thread: Option<String>,
        command: String,
    },
    Ack {
        id: String,
    },
    Error {
        id: Option<String>,
        message: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScopeState {
    pub scope: Scope,
    pub connection: String,
    pub capabilities: Vec<String>,
    pub sessions: Vec<SessionView>,
    pub asks: Vec<AskView>,
    pub transcripts: BTreeMap<String, Timeline>,
    pub detail: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionView {
    pub id: String,
    pub title: String,
    pub parent: Option<String>,
    pub status: String,
    pub updated_at: i64,
    pub capabilities: Vec<String>,
    pub detail: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AskView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<crate::asks::CommandApproval>,
    pub id: RequestId,
    pub token: String,
    pub thread: Option<String>,
    pub turn: Option<String>,
    pub item: Option<String>,
    pub choice_details: Vec<Option<String>>,
    pub title: String,
    pub detail: String,
    pub choices: Vec<String>,
    pub questions: Vec<Question>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Timeline {
    pub items: Vec<TimelineItem>,
    pub detail: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TimelineItem {
    pub turn: String,
    pub id: String,
    pub role: String,
    pub text: String,
    pub revision: u64,
    pub detail: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    Notification {
        thread: String,
        title: String,
        event: NotificationKind,
    },
    Session {
        session: SessionView,
    },
    Sessions {
        sessions: Vec<SessionView>,
    },
    Asks {
        asks: Vec<AskView>,
    },
    Transcript {
        thread: String,
        timeline: Timeline,
    },
    Items {
        thread: String,
        items: Vec<TimelineItem>,
    },
    Append {
        thread: String,
        turn: String,
        id: String,
        text: String,
        revision: u64,
    },
    TimelineState {
        thread: String,
        detail: Value,
    },
    State {
        connection: String,
        capabilities: Vec<String>,
        detail: Value,
    },
    Retired {
        thread: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Finished,
    Failed,
    Approval,
    Question,
}
