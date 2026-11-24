//! Host-owned OX workers. A worker is an ordinary native session in its own
//! project and billing route. This module keeps the immutable origin stamp of
//! each worker and fork, decides access ceilings, and answers the parent agent's
//! four MCP tools from host session state. It never resolves approvals: those
//! go to the human clients through the normal ask path.
use ox_desk_core::{
    host_protocol::{Scope, SessionView, Timeline},
    origin::{Ceiling, ExternalCaller, Origin, ParentCall, ParentRef, Relation, WorkerStart},
    runtime::{Access, Runtime},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Mutable facts about a worker that the parent agent must see. Not part of the stamp.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Marks {
    #[serde(default)]
    pub cancelled: bool,
    /// Operator messages delivered while the worker was running.
    #[serde(default)]
    pub steered: u32,
    /// Operator messages that started a new turn after the worker finished.
    #[serde(default)]
    pub continued: u32,
    #[serde(default)]
    pub access: Option<Access>,
}

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub origin: Origin,
    pub marks: Marks,
}

/// Who is asking. A hosted gateway is a runtime the host started for `scope`,
/// identified through its private lifetime endpoint.
#[derive(Clone, Debug)]
pub(crate) enum Caller {
    Hosted {
        scope: Scope,
        hint: Option<String>,
        launch: String,
    },
    External(ExternalCaller),
    Operator {
        scope: Scope,
        parent: String,
        device: String,
    },
}

#[derive(Default)]
pub(crate) struct Registry {
    sessions: HashMap<(Scope, String), Entry>,
}

impl Registry {
    pub fn get(&self, scope: &Scope, id: &str) -> Option<&Entry> {
        self.sessions.get(&(scope.clone(), id.to_owned()))
    }
    pub fn get_mut(&mut self, scope: &Scope, id: &str) -> Option<&mut Entry> {
        self.sessions.get_mut(&(scope.clone(), id.to_owned()))
    }
    /// A stamp is written once. A second stamp for the same session is refused.
    pub fn stamp(&mut self, scope: &Scope, id: &str, origin: Origin) -> bool {
        let key = (scope.clone(), id.to_owned());
        if self.sessions.contains_key(&key) {
            return false;
        }
        self.sessions.insert(
            key,
            Entry {
                origin,
                marks: Marks::default(),
            },
        );
        true
    }
    pub fn restore(&mut self, scope: &Scope, id: &str, origin: Origin, marks: Marks) {
        self.sessions
            .entry((scope.clone(), id.to_owned()))
            .or_insert(Entry { origin, marks });
    }
    pub fn in_scope<'a>(
        &'a self,
        scope: &'a Scope,
    ) -> impl Iterator<Item = (&'a str, &'a Entry)> + 'a {
        self.sessions
            .iter()
            .filter(move |((s, _), _)| s == scope)
            .map(|((_, id), e)| (id.as_str(), e))
    }
    pub fn job_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.sessions.values().map(|e| e.origin.job_id)
    }
    /// The worker session for a parent-facing job number. Forks share the
    /// number but are never the job itself.
    pub fn job(&self, job_id: u64) -> Option<(&Scope, &str, &Entry)> {
        self.sessions
            .iter()
            .find(|(_, e)| e.origin.job_id == job_id && e.origin.relation == Relation::Worker)
            .map(|((s, id), e)| (s, id.as_str(), e))
    }
    pub fn jobs(&self) -> impl Iterator<Item = (&Scope, &str, &Entry)> {
        self.sessions
            .iter()
            .filter(|(_, e)| e.origin.relation == Relation::Worker)
            .map(|((s, id), e)| (s, id.as_str(), e))
    }
    /// Sessions whose recorded parent is this hosted session.
    pub fn children_of<'a>(
        &'a self,
        scope: &'a Scope,
        id: &'a str,
    ) -> impl Iterator<Item = (&'a Scope, &'a str)> + 'a {
        self.sessions
            .iter()
            .filter(move |(_, e)| {
                e.origin.parent.scope.as_ref() == Some(scope)
                    && e.origin.parent.session.as_deref() == Some(id)
            })
            .map(|((s, id), _)| (s, id.as_str()))
    }
}

/// The parent permission check that existed for gateway-run workers, unchanged.
pub(crate) fn permits(access: Option<Access>, write: bool, claude: bool) -> bool {
    match access {
        Some(Access::Yolo | Access::BypassPermissions) => true,
        Some(Access::Workspace | Access::AutoReview) => !write || !claude,
        Some(Access::AcceptEdits) => !write || claude,
        Some(Access::ReadOnly | Access::Plan | Access::Default | Access::Auto) => !write,
        _ => false,
    }
}

/// Decide the recorded ceiling before any process starts.
/// A hosted parent's confirmed mode bounds it. A worker parent's own ceiling
/// bounds its children. A terminal parent is bounded by what it asked for.
pub(crate) fn grant(
    parent_access: Option<Access>,
    parent_origin: Option<&Origin>,
    external: bool,
    runtime: Runtime,
    requested: Ceiling,
) -> Result<Ceiling, String> {
    let claude = runtime == Runtime::Claude;
    let mut ceiling = if external {
        requested
    } else if permits(parent_access, true, claude) {
        Ceiling::Write
    } else if permits(parent_access, false, claude) {
        Ceiling::Read
    } else {
        return Err("The parent session’s permission mode is not verified.".into());
    };
    if let Some(origin) = parent_origin {
        ceiling = origin.child_ceiling(runtime, ceiling);
    }
    if requested > ceiling {
        return Err("Worker access exceeds its active parent’s permission ceiling.".into());
    }
    Ok(ceiling)
}

/// The operator may change a worker's mode only within its recorded ceiling.
pub(crate) fn operator_access(origin: &Origin, access: Access) -> Result<(), String> {
    if origin.ceiling.allows(origin.runtime, access) {
        Ok(())
    } else {
        Err(format!(
            "This worker’s recorded ceiling is {}. Start a new worker for broader access.",
            match origin.ceiling {
                Ceiling::Read => "read-only",
                Ceiling::Write => "workspace write",
            }
        ))
    }
}

/// Parent-agent callers see only their own jobs.
/// `parents` are the hosted sessions that could be calling.
pub(crate) fn owns(caller: &Caller, parents: &[String], origin: &Origin) -> bool {
    let p = &origin.parent;
    match caller {
        Caller::Hosted { scope, .. } => {
            p.scope.as_ref() == Some(scope)
                && p.session.as_ref().is_some_and(|s| parents.contains(s))
        }
        Caller::External(c) => {
            p.scope.is_none()
                && p.project == c.project
                && (origin.launch == c.launch
                    || match (&p.session, &c.session) {
                        (Some(a), Some(b)) => a == b,
                        (None, _) => true,
                        (Some(_), None) => false,
                    })
        }
        Caller::Operator { .. } => false,
    }
}

/// List only what the calling launch or session started.
pub(crate) fn lists(caller: &Caller, parents: &[String], origin: &Origin) -> bool {
    match caller {
        Caller::External(c) => {
            origin.parent.scope.is_none()
                && origin.parent.project == c.project
                && (origin.launch == c.launch
                    || origin.parent.session.is_some() && origin.parent.session == c.session)
        }
        _ => owns(caller, parents, origin),
    }
}

pub(crate) fn parent_ref(caller: &Caller, session: Option<String>) -> ParentRef {
    match caller {
        Caller::Hosted { scope, .. } | Caller::Operator { scope, .. } => ParentRef {
            project: scope.project.clone(),
            scope: Some(scope.clone()),
            session,
            runtime: Some(scope.route.runtime),
        },
        Caller::External(c) => ParentRef {
            project: c.project.clone(),
            scope: None,
            session: c.session.clone(),
            runtime: Some(c.runtime),
        },
    }
}

pub(crate) fn launch_identity(caller: &Caller) -> String {
    match caller {
        Caller::Hosted { launch, .. } => format!("helper:{launch}"),
        Caller::External(c) => format!("terminal:{}", c.launch),
        Caller::Operator { device, .. } => format!("operator:{device}"),
    }
}

pub(crate) fn origin(
    caller: &Caller,
    parent: Option<String>,
    start: &WorkerStart,
    job_id: u64,
    ceiling: Ceiling,
) -> Origin {
    Origin {
        relation: Relation::Worker,
        parent: parent_ref(caller, parent),
        launch: launch_identity(caller),
        by_operator: matches!(caller, Caller::Operator { .. }),
        runtime: start.runtime,
        route: start.route(),
        mode: start.mode,
        job_id,
        requested: Ceiling::of(start.write),
        ceiling,
        source: None,
    }
}

/// Parse a gateway's parent-agent request. Anything outside the four tools,
/// such as answering an approval, is refused here.
pub(crate) fn parent_call(value: Value) -> Result<ParentCall, String> {
    serde_json::from_value(value)
        .map_err(|_| "A parent agent can list, inspect, steer or cancel its workers.".into())
}

/// The status a parent agent reads, built from the host's own session state.
pub(crate) fn status(
    entry: &Entry,
    id: &str,
    native: &str,
    view: Option<&SessionView>,
    timeline: Option<&Timeline>,
    live: bool,
) -> Value {
    let o = &entry.origin;
    let row = view.map_or("interrupted", |v| v.status.as_str());
    let finished = timeline.is_some_and(|t| !t.detail["last_finished"].is_null());
    let status = if entry.marks.cancelled {
        "cancelled"
    } else {
        match row {
            "running" | "needs_input" => "running",
            "failed" => "failed",
            "interrupted" | "Last seen running" => "interrupted",
            _ if !live => "interrupted",
            _ if finished || timeline.is_none() => "completed",
            _ => "running",
        }
    };
    let progress = match row {
        "needs_input" => "Waiting for the operator to answer a request",
        "running" => "Working",
        _ if status == "interrupted" => {
            "The session host restarted or the runtime closed. Open the worker to continue it."
        }
        _ => "",
    };
    let result = timeline.and_then(|t| {
        t.items
            .iter()
            .rev()
            .find(|i| i.role == "assistant")
            .map(|i| i.text[..i.text.floor_char_boundary(24 * 1024)].to_owned())
    });
    let error = timeline
        .and_then(|t| t.detail["error"].as_str())
        .map(str::to_owned);
    let operator = (entry.marks.steered > 0 || entry.marks.continued > 0 || entry.marks.access.is_some())
        .then(|| {
            json!({
                "steered": entry.marks.steered,
                "continued": entry.marks.continued,
                "access": entry.marks.access,
                "note": "The human operator sent this worker instructions. Its result may reflect them as well as your task.",
            })
        });
    json!({
        "job_id": o.job_id,
        "mode": o.mode,
        "runtime": o.runtime,
        "can_steer": status == "running" && row == "running"
            && view.is_some_and(|v| v.capabilities.iter().any(|c| c == "steer")),
        "status": status,
        "model": match &o.route.billing {
            ox_desk_core::runtime::Billing::Api(alias) => alias.clone(),
            ox_desk_core::runtime::Billing::Subscription => format!("{} subscription", o.runtime.account()),
        },
        "native_session": native,
        "session": id,
        "owner": "host",
        "access_ceiling": o.ceiling,
        "progress": progress,
        "result": result,
        "error": error,
        "operator": operator,
        "messages": [],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::{origin::WorkerMode, runtime::Route};

    fn scope(route: Route) -> Scope {
        Scope {
            project: "/p".into(),
            route,
        }
    }
    fn worker(ceiling: Ceiling, runtime: Runtime) -> Origin {
        Origin {
            relation: Relation::Worker,
            parent: ParentRef {
                project: "/p".into(),
                scope: Some(scope(Route::CODEX_SUBSCRIPTION)),
                session: Some("parent".into()),
                runtime: Some(Runtime::Codex),
            },
            launch: "helper:a".into(),
            by_operator: false,
            runtime,
            route: runtime.route(Some("fixture".into())),
            mode: WorkerMode::Session,
            job_id: 7,
            requested: Ceiling::Read,
            ceiling,
            source: None,
        }
    }

    #[test]
    fn worker_cannot_inherit_unverified_or_broader_permissions() {
        for mode in [
            Access::ReadOnly,
            Access::Plan,
            Access::Default,
            Access::Auto,
        ] {
            assert!(!permits(Some(mode), true, false));
            assert!(!permits(Some(mode), true, true));
        }
        assert!(!permits(None, false, false));
        assert!(!permits(Some(Access::Configured), true, false));
        assert!(permits(Some(Access::Workspace), true, false));
        assert!(!permits(Some(Access::Workspace), true, true));
        assert!(!permits(Some(Access::AcceptEdits), true, false));
    }

    #[test]
    fn the_ceiling_is_the_parent_grant_and_requests_cannot_exceed_it() {
        let c = Runtime::Codex;
        assert_eq!(
            grant(Some(Access::Workspace), None, false, c, Ceiling::Read),
            Ok(Ceiling::Write)
        );
        assert_eq!(
            grant(Some(Access::ReadOnly), None, false, c, Ceiling::Read),
            Ok(Ceiling::Read)
        );
        assert!(grant(Some(Access::ReadOnly), None, false, c, Ceiling::Write).is_err());
        assert!(grant(None, None, false, c, Ceiling::Read).is_err());
        assert!(grant(Some(Access::Configured), None, false, c, Ceiling::Read).is_err());
        assert!(
            grant(
                Some(Access::Workspace),
                None,
                false,
                Runtime::Claude,
                Ceiling::Write
            )
            .is_err()
        );
        // A terminal gateway is bounded by its own request.
        assert_eq!(grant(None, None, true, c, Ceiling::Read), Ok(Ceiling::Read));
        assert_eq!(
            grant(None, None, true, c, Ceiling::Write),
            Ok(Ceiling::Write)
        );
    }

    #[test]
    fn ceilings_are_inherited_through_children_forks_and_continuation() {
        let read = worker(Ceiling::Read, Runtime::Codex);
        // A read worker in YOLO would still give its children only read.
        assert_eq!(
            grant(
                Some(Access::Yolo),
                Some(&read),
                false,
                Runtime::Codex,
                Ceiling::Read
            ),
            Ok(Ceiling::Read)
        );
        assert!(
            grant(
                Some(Access::Yolo),
                Some(&read),
                false,
                Runtime::Codex,
                Ceiling::Write
            )
            .is_err()
        );
        let write = worker(Ceiling::Write, Runtime::Codex);
        assert_eq!(
            grant(
                Some(Access::Workspace),
                Some(&write),
                false,
                Runtime::Codex,
                Ceiling::Write
            ),
            Ok(Ceiling::Write)
        );
        // A nested write needs the same runtime's sandbox.
        assert!(
            grant(
                Some(Access::Yolo),
                Some(&write),
                false,
                Runtime::Claude,
                Ceiling::Write
            )
            .is_err()
        );
        // Forks and forks of forks keep the original stamp's ceiling and job.
        let fork = read.fork("w1");
        let again = fork.fork("w2");
        for f in [&fork, &again] {
            assert_eq!(f.ceiling, Ceiling::Read);
            assert_eq!(f.job_id, read.job_id);
            assert_eq!(f.parent, read.parent);
            assert_eq!(f.relation, Relation::Fork);
        }
        assert_eq!(again.source.as_deref(), Some("w2"));
        // Continuation keeps the session and its stamp, so the operator check is unchanged.
        assert!(operator_access(&again, Access::Workspace).is_err());
        let mut registry = Registry::default();
        let s = scope(Route::codex_api("fixture".into()));
        assert!(registry.stamp(&s, "w1", read.clone()));
        assert!(
            !registry.stamp(&s, "w1", write.clone()),
            "a stamp is immutable"
        );
        assert_eq!(
            registry.get(&s, "w1").unwrap().origin.ceiling,
            Ceiling::Read
        );
        assert!(registry.stamp(&s, "w2", fork));
        assert_eq!(registry.job(7).map(|(_, id, _)| id), Some("w1"));
    }

    #[test]
    fn the_operator_cannot_raise_access_above_the_recorded_ceiling() {
        let read = worker(Ceiling::Read, Runtime::Codex);
        assert!(operator_access(&read, Access::ReadOnly).is_ok());
        assert!(operator_access(&read, Access::Workspace).is_err());
        assert!(operator_access(&read, Access::Yolo).is_err());
        let write = worker(Ceiling::Write, Runtime::Codex);
        assert!(operator_access(&write, Access::Workspace).is_ok());
        assert!(operator_access(&write, Access::AutoReview).is_err());
        assert!(operator_access(&write, Access::Yolo).is_err());
        let claude = worker(Ceiling::Write, Runtime::Claude);
        assert!(operator_access(&claude, Access::AcceptEdits).is_ok());
        assert!(operator_access(&claude, Access::BypassPermissions).is_err());
        assert!(operator_access(&claude, Access::Auto).is_err());
    }

    #[test]
    fn a_parent_agent_has_no_approval_or_permission_operation() {
        for refused in [
            json!({"op":"answer","job_id":7,"choice":0}),
            json!({"op":"answer_questions","job_id":7,"answers":["yes"]}),
            json!({"op":"set_access","job_id":7,"mode":"yolo"}),
            json!({"op":"status","job_id":7,"approval":"token"}),
        ] {
            assert!(parent_call(refused).is_err());
        }
        assert_eq!(
            parent_call(json!({"op":"status","job_id":7})),
            Ok(ParentCall::Status { job_id: 7 })
        );
    }

    #[test]
    fn parent_agents_see_only_their_own_jobs() {
        let o = worker(Ceiling::Read, Runtime::Codex);
        let hosted = Caller::Hosted {
            scope: scope(Route::CODEX_SUBSCRIPTION),
            hint: None,
            launch: "b".into(),
        };
        assert!(owns(&hosted, &["parent".into()], &o));
        assert!(!owns(&hosted, &["other".into()], &o));
        let elsewhere = Caller::Hosted {
            scope: scope(Route::codex_api("x".into())),
            hint: None,
            launch: "b".into(),
        };
        assert!(!owns(&elsewhere, &["parent".into()], &o));
        let terminal = |session: Option<&str>, launch: &str| {
            Caller::External(ExternalCaller {
                project: "/p".into(),
                runtime: Runtime::Claude,
                session: session.map(Into::into),
                launch: launch.into(),
            })
        };
        assert!(
            !owns(&terminal(None, "a"), &[], &o),
            "hosted jobs are not terminal jobs"
        );
        let mut t = o.clone();
        t.parent.scope = None;
        t.parent.session = Some("s1".into());
        t.launch = "terminal:l1".into();
        // A resumed terminal session sees its job. Another session does not.
        assert!(owns(&terminal(Some("s1"), "l2"), &[], &t));
        assert!(!owns(&terminal(Some("s2"), "l2"), &[], &t));
        assert!(!owns(&terminal(None, "l2"), &[], &t));
        assert!(lists(&terminal(Some("s1"), "l2"), &[], &t));
        assert!(!lists(&terminal(None, "l2"), &[], &t));
    }

    #[test]
    fn status_reports_operator_continuation_and_restart() {
        let mut entry = Entry {
            origin: worker(Ceiling::Read, Runtime::Codex),
            marks: Marks::default(),
        };
        let view = |status: &str| SessionView {
            id: "w".into(),
            title: String::new(),
            parent: None,
            status: status.into(),
            updated_at: 0,
            capabilities: vec!["steer".into()],
            detail: Value::Null,
        };
        let done = Timeline {
            items: vec![ox_desk_core::host_protocol::TimelineItem {
                turn: "t".into(),
                id: "i".into(),
                role: "assistant".into(),
                text: "final".into(),
                revision: 1,
                detail: Value::Null,
            }],
            detail: json!({"last_finished":"t"}),
        };
        let s = status(&entry, "w", "w", Some(&view("needs_input")), None, true);
        assert_eq!(s["status"], "running");
        assert_eq!(s["can_steer"], false);
        let s = status(&entry, "w", "w", Some(&view("idle")), Some(&done), true);
        assert_eq!(
            (s["status"].as_str(), s["result"].as_str()),
            (Some("completed"), Some("final"))
        );
        assert!(s["operator"].is_null());
        entry.marks.continued = 1;
        let s = status(&entry, "w", "w", Some(&view("running")), Some(&done), true);
        assert_eq!(s["status"], "running");
        assert_eq!(s["operator"]["continued"], 1);
        let s = status(&entry, "w", "w", Some(&view("interrupted")), None, false);
        assert_eq!(s["status"], "interrupted");
        entry.marks.cancelled = true;
        assert_eq!(
            status(&entry, "w", "w", None, None, false)["status"],
            "cancelled"
        );
    }
}
