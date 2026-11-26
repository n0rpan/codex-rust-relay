//! The UI enqueues an owned call. Its wire object is built on the pipe writer.
use super::{ask::Answer, frame::RequestId, names::Method};
use serde_json::{Value, json};
use std::path::PathBuf;

pub enum Call {
    FindFile {
        project: std::path::PathBuf,
        query: String,
    },
    Features(Option<String>),
    Models {
        provider: String,
        cursor: Option<String>,
    },
    Config(PathBuf),
    Command {
        thread: String,
        command: ThreadCommand,
    },
    Access {
        thread: String,
        preset: super::access::Access,
    },
    Initialize,
    Account,
    Limits,
    Options {
        thread: String,
        options: crate::store::routes_thread::Options,
    },
    List {
        project: PathBuf,
        query: String,
        cursor: Option<String>,
    },
    WorkspaceList(Option<String>),
    Read(String),
    ReadFullHistory(String),
    Fork {
        preset: Option<super::access::Access>,
        thread: String,
        project: PathBuf,
        model: Option<String>,
        provider: String,
    },
    Archive(String),
    Unsubscribe(String),
    Descendants {
        root: String,
        cursor: Option<String>,
    },
    Resume {
        preset: Option<super::access::Access>,
        thread: String,
        project: Option<PathBuf>,
        full_history: bool,
    },
    History {
        thread: String,
        cursor: Option<String>,
    },
    New {
        preset: Option<super::access::Access>,
        project: PathBuf,
        model: Option<String>,
    },
    Start {
        images: Vec<crate::mentions::ImageInput>,
        options: Option<crate::store::routes_thread::Options>,
        thread: String,
        text: String,
        /// OX's sender statement, sent as an `additionalContext` application entry.
        context: Option<String>,
    },
    Steer {
        images: Vec<crate::mentions::ImageInput>,
        thread: String,
        turn: String,
        text: String,
        context: Option<String>,
    },
    Stop {
        thread: String,
        turn: String,
    },
    /// Raw Responses message items placed before the thread's first turn.
    Inject {
        thread: String,
        items: Vec<Value>,
    },
}
#[derive(Clone, Debug)]
pub enum ThreadCommand {
    Goal(crate::slash::Goal),
    Tier(Option<String>),
    Rename(String),
    Compact,
}
impl ThreadCommand {
    pub fn method(&self) -> Method {
        use crate::slash::Goal;
        match self {
            Self::Goal(Goal::Show) => Method::GetGoal,
            Self::Goal(Goal::Clear) => Method::ClearGoal,
            Self::Goal(_) => Method::SetGoal,
            Self::Tier(_) => Method::Settings,
            Self::Rename(_) => Method::Rename,
            Self::Compact => Method::Compact,
        }
    }
    fn params(self, thread: String) -> Value {
        use crate::slash::Goal;
        let mut value = json!({"threadId":thread});
        match self {
            Self::Goal(Goal::Show) | Self::Compact => (),
            Self::Goal(goal) => {
                value["origin"] = json!("user");
                match goal {
                    Goal::Set(text) => value["objective"] = text.into(),
                    Goal::Pause => value["status"] = "paused".into(),
                    Goal::Resume => value["status"] = "active".into(),
                    _ => (),
                }
            }
            Self::Tier(tier) => value["serviceTier"] = json!(tier),
            Self::Rename(name) => value["name"] = name.into(),
        }
        value
    }
}
fn apply_preset(params: &mut Value, preset: Option<super::access::Access>) {
    use super::access::Access;
    if let Some(preset) = preset {
        let (sandbox, policy, reviewer) = match preset {
            Access::ReadOnly => ("read-only", "on-request", "user"),
            Access::Workspace => ("workspace-write", "on-request", "user"),
            Access::AutoReview => ("workspace-write", "on-request", "auto_review"),
            Access::Yolo => ("danger-full-access", "never", "user"),
            _ => {
                unreachable!("only explicit Codex presets can override thread defaults")
            }
        };
        params["sandbox"] = sandbox.into();
        params["approvalPolicy"] = policy.into();
        params["approvalsReviewer"] = reviewer.into();
    }
}

impl Call {
    pub fn method(&self) -> Method {
        match self {
            Self::Features(_) => Method::Features,
            Self::Models { .. } => Method::Models,
            Self::Config(_) => Method::Config,
            Self::Command { command, .. } => command.method(),
            Self::Access { .. } | Self::Options { .. } => Method::Settings,
            Self::Initialize => Method::Initialize,
            Self::Account => Method::Account,
            Self::Limits => Method::Limits,
            Self::List { .. } | Self::WorkspaceList(_) | Self::Descendants { .. } => Method::List,
            Self::Unsubscribe(_) => Method::Unsubscribe,
            Self::Read(_) | Self::ReadFullHistory(_) => Method::Read,
            Self::Fork { .. } => Method::Fork,
            Self::Archive(_) => Method::Archive,
            Self::Resume { .. } => Method::Resume,
            Self::History { .. } => Method::Turns,
            Self::New { .. } => Method::ThreadStart,
            Self::FindFile { .. } => Method::FindFile,
            Self::Start { .. } => Method::TurnStart,
            Self::Steer { .. } => Method::Steer,
            Self::Stop { .. } => Method::Interrupt,
            Self::Inject { .. } => Method::InjectItems,
        }
    }
    fn params(self) -> Value {
        const TURNS: usize = 12;
        match self {
            Self::Features(cursor) => json!({"limit":100,"cursor":cursor}),
            Self::Models { provider, cursor } => {
                json!({"modelProvider":provider,"limit":100,"cursor":cursor})
            }
            Self::Config(project) => json!({"cwd":project,"includeLayers":false}),
            Self::Command { thread, command } => command.params(thread),
            Self::Access { thread, preset } => {
                preset.params(&thread).expect("editable permission preset")
            }
            Self::Initialize => {
                json!({"clientInfo":{"name":"ox_desktop","title":"OX Desktop","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})
            }
            Self::Account => json!({"refreshToken":false}),
            Self::Limits => json!({}),
            Self::Options { thread, options } => {
                json!({"threadId":thread,"model":options.model,"effort":options.effort,"serviceTier":options.tier})
            }
            Self::List {
                project,
                query,
                cursor,
            } => {
                let mut params = json!({"cwd":project,"modelProviders":[],"sortKey":"updated_at","sourceKinds":["cli","vscode","exec","appServer","subAgent","subAgentThreadSpawn","subAgentReview","subAgentCompact","subAgentOther"],"limit":50,"cursor":cursor});
                if !query.is_empty() {
                    params["searchTerm"] = query.into();
                }
                params
            }
            Self::WorkspaceList(cursor) => {
                json!({"modelProviders":[],"sortKey":"updated_at","sourceKinds":["cli","vscode","exec","appServer","subAgent","subAgentThreadSpawn","subAgentReview","subAgentCompact","subAgentOther"],"limit":200,"cursor":cursor})
            }
            Self::Unsubscribe(thread) => json!({"threadId":thread}),
            Self::Descendants { root, cursor } => {
                json!({"ancestorThreadId":root,"sourceKinds":["subAgent","subAgentThreadSpawn","subAgentReview","subAgentCompact","subAgentOther"],"modelProviders":[],"limit":100,"cursor":cursor})
            }
            Self::Fork {
                preset,
                thread,
                project,
                model,
                provider,
            } => {
                let mut params = json!({"threadId":thread,"cwd":project,"model":model,"modelProvider":provider,"excludeTurns":true,"deferGoalContinuation":true});
                apply_preset(&mut params, preset);
                params
            }
            Self::Archive(thread) => json!({"threadId":thread}),
            Self::Read(thread) => json!({"threadId":thread,"includeTurns":false}),
            Self::ReadFullHistory(thread) => json!({"threadId":thread,"includeTurns":true}),
            Self::Resume {
                preset,
                thread,
                project,
                full_history,
            } => {
                let mut params = json!({"threadId":thread,"excludeTurns":!full_history});
                apply_preset(&mut params, preset);
                if !full_history {
                    params["initialTurnsPage"] =
                        json!({"limit":TURNS,"sortDirection":"desc","itemsView":"full"});
                }
                if let Some(project) = project {
                    params["cwd"] = json!(project);
                }
                params
            }
            Self::History { thread, cursor } => {
                json!({"threadId":thread,"limit":TURNS,"sortDirection":"desc","itemsView":"full","cursor":cursor})
            }
            Self::New {
                project,
                model,
                preset,
            } => {
                let mut params =
                    json!({"cwd":project,"approvalPolicy":"on-request","approvalsReviewer":"user"});
                apply_preset(&mut params, preset);
                if let Some(m) = model {
                    params["model"] = m.into();
                }
                params
            }
            Self::FindFile { project, query } => {
                json!({"roots":[project],"query":query,"cancellationToken":"ox_desktop_mentions"})
            }
            Self::Start {
                thread,
                text,
                images,
                options,
                context,
            } => {
                let mut params =
                    json!({"threadId":thread,"input":crate::mentions::turn_input(text,images)});
                if let Some(settings) = options {
                    params["model"] = settings.model.into();
                    params["effort"] = json!(settings.effort);
                    params["serviceTier"] = json!(settings.tier);
                }
                if let Some(context) = context {
                    params["additionalContext"] = crate::surface::codex_context(&context);
                }
                params
            }
            Self::Steer {
                thread,
                turn,
                text,
                images,
                context,
            } => {
                let mut params = json!({"threadId":thread,"expectedTurnId":turn,"input":crate::mentions::turn_input(text,images)});
                if let Some(context) = context {
                    params["additionalContext"] = crate::surface::codex_context(&context);
                }
                params
            }
            Self::Stop { thread, turn } => json!({"threadId":thread,"turnId":turn}),
            Self::Inject { thread, items } => json!({"threadId":thread,"items":items}),
        }
    }
}

pub(crate) enum Outgoing {
    Request(i64, Call),
    Initialized,
    Answer(RequestId, Answer),
    Reject(RequestId, i64, String),
}
impl Outgoing {
    pub fn encode(self) -> Value {
        match self {
            Self::Request(id, call) => {
                json!({"id":id,"method":call.method().as_str(),"params":call.params()})
            }
            Self::Initialized => json!({"method":"initialized"}),
            Self::Answer(id, answer) => json!({"id":id,"result":answer}),
            Self::Reject(id, code, message) => {
                json!({"id":id,"error":{"code":code,"message":message}})
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_preset_is_applied_to_new_resumed_and_forked_threads() {
        use super::super::access::Access;
        for (preset, sandbox, approvals, reviewer) in [
            (Access::ReadOnly, "read-only", "on-request", "user"),
            (Access::Workspace, "workspace-write", "on-request", "user"),
            (
                Access::AutoReview,
                "workspace-write",
                "on-request",
                "auto_review",
            ),
            (Access::Yolo, "danger-full-access", "never", "user"),
        ] {
            let preset = Some(preset);
            for call in [
                Call::New {
                    preset,
                    project: "/work".into(),
                    model: None,
                },
                Call::Resume {
                    preset,
                    thread: "session".into(),
                    project: None,
                    full_history: true,
                },
                Call::Fork {
                    preset,
                    thread: "session".into(),
                    project: "/work".into(),
                    model: None,
                    provider: "openai".into(),
                },
            ] {
                let params = call.params();
                assert_eq!(params["sandbox"], sandbox);
                assert_eq!(params["approvalPolicy"], approvals);
                assert_eq!(params["approvalsReviewer"], reviewer);
            }
        }
        let follow = Call::Resume {
            preset: None,
            thread: "old".into(),
            project: None,
            full_history: false,
        }
        .params();
        assert!(follow.get("sandbox").is_none() && follow.get("approvalPolicy").is_none());
    }

    #[test]
    fn start_and_steer_send_the_sender_as_one_application_entry() {
        let start = Call::Start {
            images: vec![],
            options: None,
            thread: "t".into(),
            text: "exact text".into(),
            context: Some("from OX Desktop".into()),
        }
        .params();
        assert_eq!(
            start["additionalContext"],
            json!({"ox.sender":{"kind":"application","value":"from OX Desktop"}})
        );
        assert_eq!(start["input"][0]["text"], "exact text");
        let steer = Call::Steer {
            images: vec![],
            thread: "t".into(),
            turn: "u".into(),
            text: "more".into(),
            context: Some("from the OX phone app".into()),
        }
        .params();
        assert_eq!(
            steer["additionalContext"]["ox.sender"]["value"],
            "from the OX phone app"
        );
        assert_eq!(steer["expectedTurnId"], "u");
        let plain = Call::Steer {
            images: vec![],
            thread: "t".into(),
            turn: "u".into(),
            text: "more".into(),
            context: None,
        }
        .params();
        assert!(plain.get("additionalContext").is_none());
    }
}
