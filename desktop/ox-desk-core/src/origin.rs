//! Immutable lineage of a host-owned session. A worker, its forks and its own
//! children carry the stamp written when the host accepted the launch. The same
//! record can later describe other links, such as a cross-runtime handoff,
//! through `relation`, without a second lineage model.
use crate::{
    handoff::HandoffSource,
    host_protocol::Scope,
    modes::{Decision, ModeRequest, YoloAllowance},
    runtime::{Access, Route, Runtime},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The most a child may ever be granted, recorded once at launch. It is the
/// permission [`Level`] of the source, inherited by forks, continuations and
/// nested workers. Older stamps recorded only `read` and `write`.
pub use crate::modes::Level as Ceiling;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkerMode {
    Session,
    Goal,
    Workflow,
}

/// How this session relates to `parent`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// Started by the parent agent or by the operator on the parent's behalf.
    Worker,
    /// A native fork of a session that already carried this origin.
    Fork,
    /// A new native session that continues `parent` in another runtime, billing
    /// route or model, started by the operator. `handoff` names the source.
    Handoff,
}

/// The session that started a worker, as verified by the host.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ParentRef {
    pub project: PathBuf,
    /// The host scope of a hosted parent. Absent for a terminal session the host does not run.
    pub scope: Option<Scope>,
    /// The parent's session ID in that scope, or its native ID for a terminal session.
    pub session: Option<String>,
    /// Stock runtime of the parent, when known.
    pub runtime: Option<Runtime>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Origin {
    pub relation: Relation,
    pub parent: ParentRef,
    /// Non-secret identity of the launching OX helper or operator device.
    pub launch: String,
    pub by_operator: bool,
    pub runtime: Runtime,
    pub route: Route,
    pub mode: WorkerMode,
    /// The parent-facing job number. Forks keep the original worker's number.
    pub job_id: u64,
    /// Access requested at launch. Never above `ceiling`.
    pub requested: Ceiling,
    pub ceiling: Ceiling,
    /// For a fork, the session it was forked from.
    pub source: Option<String>,
    /// The native preset the child started in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<Access>,
    /// How that starting mode was decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided: Option<Decision>,
    /// For a YOLO or bypass child, the acceptance or grant that allowed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<YoloAllowance>,
    /// For a handoff, the source session and references to what it carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<Box<HandoffSource>>,
}

impl Origin {
    /// The stamp a fork of this session carries. It can only keep or narrow the ceiling.
    pub fn fork(&self, source: &str) -> Self {
        Self {
            relation: Relation::Fork,
            source: Some(source.to_owned()),
            ..self.clone()
        }
    }
    /// The most this session's children can receive because of its own stamp.
    /// Workers and their forks pass on their ceiling. A handoff target belongs
    /// to the operator, so only its live mode bounds its children.
    pub fn limits_children(&self) -> Option<Ceiling> {
        (self.relation != Relation::Handoff && self.handoff.is_none()).then_some(self.ceiling)
    }
    /// Whether the operator's mode changes on this session are bounded by the stamp.
    pub fn bounds_operator(&self) -> bool {
        self.limits_children().is_some()
    }
}

/// An operator's request to start a worker attached to a session in the subscribed scope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkerStart {
    pub parent: String,
    pub runtime: Runtime,
    pub model: Option<String>,
    pub mode: WorkerMode,
    pub task: String,
    /// Legacy access request: false is read, true is write. Absent means inherit.
    #[serde(default)]
    pub write: Option<bool>,
    /// The requested permission mode. Wins over `write`. Absent means inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<ModeRequest>,
    /// An operator start whose client showed the YOLO confirmation.
    #[serde(default)]
    pub yolo_confirmed: bool,
    /// Wall-clock limit in minutes. Absent means the host default (`OX_WORKER_MAX_MINUTES`, 4 hours).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_minutes: Option<u32>,
}

impl WorkerStart {
    /// The mode the launcher asked for.
    pub fn mode_request(&self) -> ModeRequest {
        self.permission.unwrap_or(ModeRequest::legacy(self.write))
    }
    /// The composer form: `/worker <codex|claude>[:<api model>] [--write]
    /// [--mode <mode>] [--goal|--workflow] <task>`. A Claude worker without a model uses the
    /// Claude subscription. Returns None when the text is not a worker command.
    pub fn parse_command(parent: &str, text: &str) -> Option<Result<Self, String>> {
        let rest = text.trim_start().strip_prefix("/worker")?;
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let usage = "Use /worker codex:<model> [--mode <mode>] [--goal] <task> or /worker claude[:<model>] [--mode <mode>] [--workflow] <task>.";
        let mut words = rest.trim_start().splitn(2, char::is_whitespace);
        let target = words.next().filter(|w| !w.is_empty());
        let Some(target) = target else {
            return Some(Err(usage.into()));
        };
        let (runtime, model) = match target.split_once(':') {
            Some((r, m)) => (r, Some(m.to_owned()).filter(|m| !m.is_empty())),
            None => (target, None),
        };
        let runtime = match runtime {
            "codex" => Runtime::Codex,
            "claude" => Runtime::Claude,
            _ => return Some(Err(usage.into())),
        };
        let mut task = words.next().unwrap_or("").trim_start();
        let (mut write, mut mode, mut permission) = (None, WorkerMode::Session, None);
        loop {
            let (flag, after) = task.split_once(char::is_whitespace).unwrap_or((task, ""));
            match flag {
                "--write" => write = Some(true),
                "--mode" => {
                    let after = after.trim_start();
                    let (name, rest) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
                    match ModeRequest::parse(name).filter(|_| !name.is_empty()) {
                        Some(m) => permission = Some(m),
                        None => return Some(Err(format!("Unknown mode {name:?}. {usage}"))),
                    }
                    task = rest.trim_start();
                    continue;
                }
                "--goal" => mode = WorkerMode::Goal,
                "--workflow" => mode = WorkerMode::Workflow,
                _ => break,
            }
            task = after.trim_start();
        }
        let start = Self {
            parent: parent.to_owned(),
            runtime,
            model,
            mode,
            task: task.to_owned(),
            write,
            permission,
            yolo_confirmed: false,
            max_minutes: None,
        };
        Some(start.validate().map(|()| start))
    }
    /// The checks the standalone OX gateway applies, shared by every launcher.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_minutes.is_some_and(|m| m == 0 || m > 7 * 24 * 60) {
            return Err("max_minutes must be between 1 and 10080.".into());
        }
        if self.task.trim().is_empty() || self.task.len() > 32 * 1024 {
            return Err("Provide a task of up to 32 KiB.".into());
        }
        if self.mode == WorkerMode::Goal
            && (self.runtime != Runtime::Codex || self.task.chars().count() > 4000)
        {
            return Err(
                "Goal mode requires Codex and an objective of at most 4,000 characters.".into(),
            );
        }
        if self.mode == WorkerMode::Workflow && self.runtime != Runtime::Claude {
            return Err("Workflow mode requires Claude Code.".into());
        }
        if self.runtime == Runtime::Codex && self.model.is_none() {
            return Err("Choose an ox API model for a Codex worker.".into());
        }
        if self.model.as_ref().is_some_and(|model| {
            model.trim().is_empty() || model.len() > 256 || model.starts_with('-')
        }) {
            return Err("Provide an ox model alias of up to 256 bytes.".into());
        }
        Ok(())
    }
    pub fn route(&self) -> Route {
        self.runtime.route(self.model.clone())
    }
    /// The first message sent to the worker session.
    pub fn prompt(&self) -> String {
        let task = self.task.trim();
        match self.mode {
            WorkerMode::Workflow => format!(
                "Run this task as a Claude Code workflow using your Workflow tool. If the tool is unavailable, explain that and stop. Objective:\n{task}"
            ),
            WorkerMode::Goal => format!("/goal {task}"),
            WorkerMode::Session => task.to_owned(),
        }
    }
}

/// The only operations a parent agent has on its workers. Approvals, questions and
/// permission changes belong to the human operator and have no variant here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParentCall {
    List,
    Status {
        job_id: u64,
    },
    Steer {
        job_id: u64,
        text: String,
    },
    Cancel {
        job_id: u64,
    },
    /// Read this session's own handoff sources, read-only. `session` names a
    /// source in the caller's chain; absent means the nearest one. Pages by item
    /// ID: `after` starts after that item, `item` returns one item in full.
    ReadThread {
        #[serde(default)]
        session: Option<String>,
        #[serde(default)]
        item: Option<String>,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Sent by a terminal gateway, never by the agent: the gateway observed its
    /// own native session. The host records it once on the launch's workers so
    /// a later resume of that session can reach them.
    Bind {
        session: String,
    },
}

/// A worker gateway the host does not run, such as `ox claude` in a terminal.
/// The host can verify only the OS user. `launch` is a random token private to
/// that gateway; the host keeps only its digest. `session` is the native session
/// the gateway has observed, from its resume arguments or the stock client's own
/// MCP request metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ExternalCaller {
    pub project: PathBuf,
    pub runtime: Runtime,
    pub session: Option<String>,
    pub launch: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_composer_worker_command_is_validated_before_it_reaches_the_host() {
        let start =
            WorkerStart::parse_command("p", "/worker codex:fixture/demo --write fix the tests")
                .unwrap()
                .unwrap();
        assert_eq!(
            start.route(),
            Runtime::Codex.route(Some("fixture/demo".into()))
        );
        assert_eq!(start.write, Some(true));
        assert_eq!(start.mode_request(), ModeRequest::Level(Ceiling::Write));
        assert_eq!(start.task, "fix the tests");
        let lowered = WorkerStart::parse_command("p", "/worker codex:a --mode read-only --goal go")
            .unwrap()
            .unwrap();
        assert_eq!(lowered.mode_request(), ModeRequest::Level(Ceiling::Read));
        assert_eq!(lowered.mode, WorkerMode::Goal);
        assert!(
            WorkerStart::parse_command("p", "/worker codex:a --mode root go")
                .unwrap()
                .is_err()
        );
        assert_eq!(start.parent, "p");
        let claude = WorkerStart::parse_command("p", "/worker claude --workflow review")
            .unwrap()
            .unwrap();
        assert_eq!(claude.route(), Runtime::Claude.route(None));
        assert_eq!(claude.mode, WorkerMode::Workflow);
        assert_eq!(claude.mode_request(), ModeRequest::Inherit);
        assert!(
            WorkerStart::parse_command("p", "/worker codex task")
                .unwrap()
                .is_err()
        );
        assert!(
            WorkerStart::parse_command("p", "/worker claude --goal x")
                .unwrap()
                .is_err()
        );
        assert!(WorkerStart::parse_command("p", "/workers x").is_none());
        assert!(WorkerStart::parse_command("p", "hello").is_none());
    }

    #[test]
    fn old_worker_starts_and_stamps_still_decode() {
        let old: WorkerStart = serde_json::from_value(serde_json::json!({
            "parent":"p","runtime":"codex","model":"m","mode":"session","task":"t","write":false
        }))
        .unwrap();
        assert_eq!(old.mode_request(), ModeRequest::Level(Ceiling::Read));
        let new: WorkerStart = serde_json::from_value(serde_json::json!({
            "parent":"p","runtime":"codex","model":"m","mode":"session","task":"t","permission":"write"
        }))
        .unwrap();
        assert_eq!(new.mode_request(), ModeRequest::Level(Ceiling::Write));
        let omitted: WorkerStart = serde_json::from_value(serde_json::json!({
            "parent":"p","runtime":"codex","model":"m","mode":"session","task":"t"
        }))
        .unwrap();
        assert_eq!(omitted.mode_request(), ModeRequest::Inherit);
        let stamp: Origin = serde_json::from_value(serde_json::json!({
            "relation":"worker","parent":{"project":"/p","scope":null,"session":null,"runtime":"codex"},
            "launch":"l","by_operator":false,"runtime":"codex","route":{"runtime":"codex","billing":{"api":"m"}},
            "mode":"session","job_id":3,"requested":"read","ceiling":"write","source":null
        }))
        .unwrap();
        assert_eq!(stamp.ceiling, Ceiling::Write);
        assert_eq!(stamp.limits_children(), Some(Ceiling::Write));
        assert!(stamp.started.is_none() && stamp.handoff.is_none());
    }
}
