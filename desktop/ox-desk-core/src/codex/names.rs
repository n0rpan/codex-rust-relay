//! Wire names shared by the decoder, request encoder and installed-schema contract.
use serde::{Deserialize, Serialize};

macro_rules! wire_names {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant,)+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }

            pub fn parse(name: &str) -> Option<Self> {
                match name { $($wire => Some(Self::$variant),)+ _ => None }
            }
        }
    }
}

wire_names!(Method {
    Initialize => "initialize", Account => "account/read", Login => "account/login/start",
    Limits => "account/rateLimits/read", Features => "experimentalFeature/list",
    Config => "config/read", Models => "model/list", FindFile => "fuzzyFileSearch",
    ThreadStart => "thread/start", Resume => "thread/resume", List => "thread/list",
    Unsubscribe => "thread/unsubscribe", Read => "thread/read", Turns => "thread/turns/list", Settings => "thread/settings/update",
    Rename => "thread/name/set", Archive => "thread/archive", Fork => "thread/fork",
    Compact => "thread/compact/start", GetGoal => "thread/goal/get", SetGoal => "thread/goal/set",
    ClearGoal => "thread/goal/clear", TurnStart => "turn/start", Steer => "turn/steer",
    Interrupt => "turn/interrupt", CallTool => "mcpServer/tool/call",
    InjectItems => "thread/inject_items",
});
wire_names!(Notice {
    ThreadStarted => "thread/started", ThreadStatus => "thread/status/changed",
    ThreadClosed => "thread/closed", ThreadArchived => "thread/archived",
    ThreadName => "thread/name/updated", ThreadSettings => "thread/settings/updated",
    TurnStarted => "turn/started", TurnCompleted => "turn/completed",
    Diff => "turn/diff/updated", Plan => "turn/plan/updated", Tokens => "thread/tokenUsage/updated",
    RateLimits => "account/rateLimits/updated", Account => "account/updated",
    Login => "account/login/completed", Started => "item/started", Completed => "item/completed",
    Text => "item/agentMessage/delta", CommandOutput => "item/commandExecution/outputDelta",
    FileOutput => "item/fileChange/outputDelta", FilePatch => "item/fileChange/patchUpdated",
    PlanText => "item/plan/delta", ReasoningSummary => "item/reasoning/summaryTextDelta",
    ReasoningPart => "item/reasoning/summaryPartAdded", Resolved => "serverRequest/resolved",
    Error => "error",
});
wire_names!(ServerMethod {
    Command => "item/commandExecution/requestApproval", Files => "item/fileChange/requestApproval",
    Permissions => "item/permissions/requestApproval", Question => "item/tool/requestUserInput",
    Elicit => "mcpServer/elicitation/request",
});
