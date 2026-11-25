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
    /// A same-user OX gateway that the host does not run asks it to own a worker.
    WorkerLaunch {
        id: String,
        caller: crate::origin::ExternalCaller,
        start: crate::origin::WorkerStart,
    },
    /// Ask for the live processes the host runs. Answered by `Server::Processes`.
    /// Only clients that send this receive that frame, so older clients are unaffected.
    ListProcesses {
        id: String,
    },
    /// The parent-agent tools of such a gateway.
    WorkerCall {
        id: String,
        caller: crate::origin::ExternalCaller,
        call: crate::origin::ParentCall,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostInfo {
    pub version: String,
    pub epoch: String,
}

/// Which side to update when a client and host speak different protocol versions.
pub fn version_mismatch(host: u32, client: u32) -> String {
    if host < client {
        format!(
            "The running OX session host speaks protocol {host}, older than this client's {client}. Run `ox host restart` to start the installed host, or rerun the installer with --desktop."
        )
    } else {
        format!(
            "This client speaks OX protocol {client}, older than the session host's {host}. Update OX Desktop by rerunning the installer with --desktop; on a phone, reload the OX page."
        )
    }
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
        /// The host's build version and boot epoch. Older hosts omit it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<HostInfo>,
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
    WorkerReply {
        id: String,
        result: Option<Value>,
        error: Option<String>,
    },
    /// The live processes of this host, newest first by start. Answers `Client::ListProcesses`.
    Processes {
        id: String,
        processes: Vec<ProcessView>,
    },
    Error {
        id: Option<String>,
        message: String,
    },
}
/// One live process the host runs for a project and billing route.
///
/// A Claude process serves one thread. A Codex app-server serves a scope and lists
/// every thread subscribed on it. To end a process, send `Action::EndSession(thread)`
/// for each id in `threads` (scoped to `scope`). The host refuses `EndSession` on a busy
/// thread; `Action::ForceEndSession(thread)` ends it anyway, after the user confirmed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProcessView {
    /// `claude`, `codex` or `worker` (every thread of the process is a host-owned worker).
    pub kind: String,
    pub scope: Scope,
    pub project: PathBuf,
    pub threads: Vec<String>,
    /// The scope's billing route label, such as the model alias.
    pub route: String,
    pub model: Option<String>,
    /// Unix seconds when the host started or first saw the process.
    pub started_at: i64,
    /// Unix seconds of the last event or action on any of its threads.
    pub last_activity: i64,
    /// A turn is in progress or an approval is pending, so ending would lose work.
    pub running: bool,
    /// The blank process kept warm for new sessions (the sign-in and version probe), not a
    /// conversation. `kind` stays `claude`. End all idle skips it, since it respawns.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft: bool,
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
