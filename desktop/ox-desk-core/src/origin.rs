//! Immutable lineage of a host-owned session. A worker, its forks and its own
//! children carry the stamp written when the host accepted the launch. The same
//! record can later describe other links, such as a cross-runtime handoff,
//! through `relation`, without a second lineage model.
use crate::{
    host_protocol::Scope,
    runtime::{Access, Route, Runtime},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The most a worker may ever be granted. Recorded once at launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Ceiling {
    /// The worker reads the project. Anything else needs a per-action human approval.
    Read,
    /// The worker may edit the workspace without asking about each edit.
    Write,
}

impl Ceiling {
    /// Whether a native permission preset stays within this ceiling. Presets that
    /// let a classifier or no one approve actions are never worker presets.
    pub fn allows(self, runtime: Runtime, access: Access) -> bool {
        match (runtime, access) {
            (Runtime::Codex, Access::ReadOnly)
            | (Runtime::Claude, Access::Default | Access::Plan) => true,
            (Runtime::Codex, Access::Workspace) | (Runtime::Claude, Access::AcceptEdits) => {
                self == Self::Write
            }
            _ => false,
        }
    }
    /// The preset a newly launched worker starts with for this requested access.
    pub fn preset(self, runtime: Runtime) -> Access {
        match (runtime, self) {
            (Runtime::Codex, Self::Read) => Access::ReadOnly,
            (Runtime::Codex, Self::Write) => Access::Workspace,
            (Runtime::Claude, Self::Read) => Access::Default,
            (Runtime::Claude, Self::Write) => Access::AcceptEdits,
        }
    }
    pub fn of(write: bool) -> Self {
        if write { Self::Write } else { Self::Read }
    }
}

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
    /// The ceiling a child of this worker can receive.
    pub fn child_ceiling(&self, runtime: Runtime, wanted: Ceiling) -> Ceiling {
        if self.ceiling == Ceiling::Write && self.runtime == runtime {
            wanted
        } else {
            Ceiling::Read
        }
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
    pub write: bool,
}

impl WorkerStart {
    /// The composer form: `/worker <codex|claude>[:<api model>] [--write]
    /// [--goal|--workflow] <task>`. A Claude worker without a model uses the
    /// Claude subscription. Returns None when the text is not a worker command.
    pub fn parse_command(parent: &str, text: &str) -> Option<Result<Self, String>> {
        let rest = text.trim_start().strip_prefix("/worker")?;
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let usage = "Use /worker codex:<model> [--write] [--goal] <task> or /worker claude[:<model>] [--write] [--workflow] <task>.";
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
        let (mut write, mut mode) = (false, WorkerMode::Session);
        loop {
            let (flag, after) = task.split_once(char::is_whitespace).unwrap_or((task, ""));
            match flag {
                "--write" => write = true,
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
        };
        Some(start.validate().map(|()| start))
    }
    /// The checks the standalone OX gateway applies, shared by every launcher.
    pub fn validate(&self) -> Result<(), String> {
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
    Status { job_id: u64 },
    Steer { job_id: u64, text: String },
    Cancel { job_id: u64 },
}

/// A worker gateway the host does not run, such as `ox claude` in a terminal.
/// The host can verify only the OS user. The session is a hint from the
/// gateway's own launch arguments.
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
        assert!(start.write);
        assert_eq!(start.task, "fix the tests");
        assert_eq!(start.parent, "p");
        let claude = WorkerStart::parse_command("p", "/worker claude --workflow review")
            .unwrap()
            .unwrap();
        assert_eq!(claude.route(), Runtime::Claude.route(None));
        assert_eq!(claude.mode, WorkerMode::Workflow);
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
    fn read_ceilings_never_admit_edit_or_unattended_presets() {
        for runtime in [Runtime::Codex, Runtime::Claude] {
            for access in Access::choices(runtime) {
                let read = Ceiling::Read.allows(runtime, *access);
                let write = Ceiling::Write.allows(runtime, *access);
                assert!(!read || write, "{access:?}");
                assert!(!access.bypasses_permissions() || !write);
            }
            assert!(Ceiling::Read.allows(runtime, Ceiling::Read.preset(runtime)));
            assert!(!Ceiling::Read.allows(runtime, Ceiling::Write.preset(runtime)));
            assert!(Ceiling::Write.allows(runtime, Ceiling::Write.preset(runtime)));
        }
        assert!(!Ceiling::Write.allows(Runtime::Codex, Access::AutoReview));
        assert!(!Ceiling::Write.allows(Runtime::Claude, Access::Auto));
    }
}
