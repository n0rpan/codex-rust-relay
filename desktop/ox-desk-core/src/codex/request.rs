//! The UI enqueues an owned call. Its wire object is built on the pipe writer.
use super::{ask::Answer, frame::RequestId, names::Method};
use serde_json::{Value, json};
use std::path::PathBuf;

pub enum Call {
    FindFile {
        project: std::path::PathBuf,
        query: String,
    },
    Job {
        parent: String,
        number: u64,
        operation: crate::worker::WorkerAction,
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
    Fork {
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
        thread: String,
        project: Option<PathBuf>,
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
    },
    Steer {
        images: Vec<crate::mentions::ImageInput>,
        thread: String,
        turn: String,
        text: String,
    },
    Stop {
        thread: String,
        turn: String,
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
impl Call {
    pub fn method(&self) -> Method {
        match self {
            Self::Job { .. } => Method::CallTool,
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
            Self::Read(_) => Method::Read,
            Self::Fork { .. } => Method::Fork,
            Self::Archive(_) => Method::Archive,
            Self::Resume { .. } => Method::Resume,
            Self::History { .. } => Method::Turns,
            Self::New { .. } => Method::ThreadStart,
            Self::FindFile { .. } => Method::FindFile,
            Self::Start { .. } => Method::TurnStart,
            Self::Steer { .. } => Method::Steer,
            Self::Stop { .. } => Method::Interrupt,
        }
    }
    fn params(self) -> Value {
        const TURNS: usize = 12;
        match self {
            Self::Job {
                parent,
                number,
                operation,
            } => {
                let mut args = json!({"job_id":number});
                let tool = match &operation {
                    crate::worker::WorkerAction::Refresh => "ox_worker_status",
                    crate::worker::WorkerAction::Steer(_) => "steer_ox_worker",
                    crate::worker::WorkerAction::Stop => "cancel_ox_worker",
                };
                if let crate::worker::WorkerAction::Steer(text) = operation {
                    args["text"] = text.into();
                }
                json!({"threadId":parent,"server":"ox_workers","tool":tool,"arguments":args})
            }
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
                thread,
                project,
                model,
                provider,
            } => {
                json!({"threadId":thread,"cwd":project,"model":model,"modelProvider":provider,"excludeTurns":true,"deferGoalContinuation":true})
            }
            Self::Archive(thread) => json!({"threadId":thread}),
            Self::Read(thread) => json!({"threadId":thread,"includeTurns":false}),
            Self::Resume { thread, project } => {
                let mut params = json!({"threadId":thread,"excludeTurns":true,"initialTurnsPage":{"limit":TURNS,"sortDirection":"desc","itemsView":"full"}});
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
            } => {
                let mut params =
                    json!({"threadId":thread,"input":crate::mentions::turn_input(text,images)});
                if let Some(settings) = options {
                    params["model"] = settings.model.into();
                    params["effort"] = json!(settings.effort);
                    params["serviceTier"] = json!(settings.tier);
                }
                params
            }
            Self::Steer {
                thread,
                turn,
                text,
                images,
            } => {
                json!({"threadId":thread,"expectedTurnId":turn,"input":crate::mentions::turn_input(text,images)})
            }
            Self::Stop { thread, turn } => json!({"threadId":thread,"turnId":turn}),
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
