//! Host-owned OX workers. A worker is an ordinary native session in its own
//! project and billing route. This module keeps the immutable origin stamp of
//! each worker and fork, decides access ceilings, and answers the parent agent's
//! four MCP tools from host session state. It never resolves approvals: those
//! go to the human clients through the normal ask path.
use ox_desk_core::{
    host_protocol::{Scope, SessionView, Timeline},
    modes::{self, Level, ModeRequest},
    origin::{ExternalCaller, Origin, ParentCall, ParentRef, Relation, WorkerStart},
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
    /// A terminal parent's native session, observed after launch and bound once
    /// by the launch that started the worker. Never changed afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// Unix seconds when the worker session was stamped. 0 for workers saved before limits.
    #[serde(default)]
    pub started_at: i64,
    /// The wall-clock limit this worker runs under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_minutes: Option<u32>,
    /// The host stopped the worker because it ran past `max_minutes`.
    #[serde(default)]
    pub expired: bool,
    /// A caution recorded at launch, such as another write worker in the same project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The default wall-clock limit of a worker, in minutes.
pub(crate) const DEFAULT_MAX_MINUTES: u32 = 240;
/// The default number of workers one parent may run at once.
pub(crate) const DEFAULT_MAX_PER_PARENT: usize = 4;

/// A positive number from an environment variable, or `default`.
pub(crate) fn env_limit(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
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
                marks: Marks {
                    started_at: crate::durable::unix_now(),
                    ..Marks::default()
                },
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
    pub fn all(&self) -> impl Iterator<Item = (&Scope, &str, &Entry)> {
        self.sessions.iter().map(|((s, id), e)| (s, id.as_str(), e))
    }
    pub fn entries_mut(&mut self) -> impl Iterator<Item = (&Scope, &mut Entry)> {
        self.sessions.iter_mut().map(|((s, _), e)| (s, e))
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
                e.origin.relation != Relation::Handoff
                    && e.origin.parent.scope.as_ref() == Some(scope)
                    && e.origin.parent.session.as_deref() == Some(id)
            })
            .map(|((s, id), _)| (s, id.as_str()))
    }
}

/// The level a hosted source grants its children: its confirmed live mode,
/// further bounded by its own stamp when it is a worker or a fork of one. An
/// unverified mode grants nothing.
pub(crate) fn source_level(
    runtime: Runtime,
    access: Option<Access>,
    origin: Option<&Origin>,
) -> Option<Level> {
    let live = Level::of(runtime, access?)?;
    Some(match origin.and_then(Origin::limits_children) {
        Some(stamp) => live.min(stamp),
        None => live,
    })
}

/// A terminal launch's own mode cannot be verified. It may ask for read or
/// write, as before; inheriting gives read, and nothing above write is granted.
pub(crate) const TERMINAL_CEILING: Level = Level::Write;

/// Decide a worker's starting mode before any process starts. Agents and the
/// operator's `/worker` both inherit the source's level by default and may only
/// lower it. Returns the resolved level, preset and how it was decided.
pub(crate) fn grant(
    source: Option<Level>,
    external: bool,
    request: ModeRequest,
    runtime: Runtime,
) -> Result<modes::Resolved, String> {
    if external {
        let request = match request {
            ModeRequest::Inherit => ModeRequest::Level(Level::Read),
            other => other,
        };
        return modes::resolve(Some(TERMINAL_CEILING), request, runtime).map_err(|_| {
            "A terminal launch can start read or write workers. Its own mode cannot be verified, so nothing above write is granted.".into()
        });
    }
    modes::resolve(source, request, runtime)
}

/// The operator may change a worker's mode only within its recorded ceiling.
/// Handoff targets belong to the operator and have no such bound.
pub(crate) fn operator_access(origin: &Origin, access: Access) -> Result<(), String> {
    if !origin.bounds_operator() || origin.ceiling.allows(origin.runtime, access) {
        Ok(())
    } else {
        Err(format!(
            "This worker’s recorded ceiling is {}. Start a new worker for broader access.",
            origin.ceiling.label(origin.runtime)
        ))
    }
}

/// A parent-agent caller after identity resolution. Every parent operation,
/// listing included, filters jobs through [`owns`] with this one value, so no
/// operation is looser than another.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Resolved {
    /// Exactly one hosted session, verified as active on the helper's route.
    Hosted { scope: Scope, session: String },
    /// A terminal launch: its private launch digest and any session it observed.
    Terminal {
        project: std::path::PathBuf,
        launch: String,
        session: Option<String>,
    },
}

/// Whether this caller may see and control the job. A terminal launch owns what
/// it started. A terminal session owns jobs whose parent session it is, as
/// recorded at launch or bound later by that launch. Nothing else matches: when
/// a launch ends without a known session, only operators control its workers.
pub(crate) fn owns(caller: &Resolved, entry: &Entry) -> bool {
    let p = &entry.origin.parent;
    match caller {
        Resolved::Hosted { scope, session } => {
            p.scope.as_ref() == Some(scope) && p.session.as_deref() == Some(session)
        }
        Resolved::Terminal {
            project,
            launch,
            session,
        } => {
            p.scope.is_none()
                && &p.project == project
                && (&entry.origin.launch == launch
                    || session.as_ref().is_some_and(|s| {
                        p.session.as_ref() == Some(s)
                            || entry.marks.parent_session.as_ref() == Some(s)
                    }))
        }
    }
}

/// The non-secret digest a launch token is stored as. Catalog rows carry it, so
/// it must not let anyone who reads a row act as that launch.
pub(crate) fn launch_digest(kind: &str, token: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(token.as_bytes());
    let hex: String = hash[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("{kind}:{hex}")
}

/// Accept only a plausible native session identifier from a gateway.
pub(crate) fn valid_session(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
            session: c.session.clone().filter(|s| valid_session(s)),
            runtime: Some(c.runtime),
        },
    }
}

pub(crate) fn launch_identity(caller: &Caller) -> String {
    match caller {
        Caller::Hosted { launch, .. } => launch_digest("helper", launch),
        Caller::External(c) => launch_digest("terminal", &c.launch),
        Caller::Operator { device, .. } => format!("operator:{device}"),
    }
}

pub(crate) fn origin(
    caller: &Caller,
    parent: Option<String>,
    start: &WorkerStart,
    job_id: u64,
    resolved: modes::Resolved,
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
        requested: resolved.level,
        ceiling: resolved.level,
        source: None,
        started: Some(resolved.access),
        decided: Some(resolved.decision),
        yolo: None,
        handoff: None,
    }
}

/// Parse a gateway's request. Anything outside the four parent tools and the
/// terminal gateway's session binding, such as answering an approval, is refused.
pub(crate) fn parent_call(value: Value) -> Result<ParentCall, String> {
    serde_json::from_value(value)
        .map_err(|_| "A parent agent can list, inspect, steer or cancel its workers.".into())
}

/// The status a parent agent reads, built from the host's own session state.
/// Who could answer a worker request right now.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Operators {
    /// Desktop windows and phones currently subscribed to the host.
    pub connected: usize,
}

pub(crate) fn status(
    entry: &Entry,
    id: &str,
    native: &str,
    view: Option<&SessionView>,
    timeline: Option<&Timeline>,
    live: bool,
    operators: Operators,
) -> Value {
    let o = &entry.origin;
    let row = view.map_or("interrupted", |v| v.status.as_str());
    let finished = timeline.is_some_and(|t| !t.detail["last_finished"].is_null());
    let status = if entry.marks.cancelled {
        "cancelled"
    } else {
        match row {
            // A host-owned worker's approvals and questions wait for a person,
            // never for the parent. Say so, so the agent can tell its user.
            "needs_input" => "waiting_for_operator",
            "running" => "running",
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
    let error = if entry.marks.expired {
        Some(format!(
            "Worker stopped after {} minutes (time limit).",
            entry.marks.max_minutes.unwrap_or(DEFAULT_MAX_MINUTES)
        ))
    } else {
        timeline
            .and_then(|t| t.detail["error"].as_str())
            .map(str::to_owned)
    };
    let elapsed = (entry.marks.started_at > 0)
        .then(|| (crate::durable::unix_now() - entry.marks.started_at).max(0));
    let operator = (entry.marks.steered > 0 || entry.marks.continued > 0 || entry.marks.access.is_some())
        .then(|| {
            json!({
                "steered": entry.marks.steered,
                "continued": entry.marks.continued,
                "access": entry.marks.access,
                "note": "The human operator sent this worker instructions. Its result may reflect them as well as your task.",
            })
        });
    let title = view.map_or("OX worker", |v| v.title.as_str());
    let waiting = (status == "waiting_for_operator").then(|| {
        let project = o.parent.project.display();
        let message = if operators.connected > 0 {
            format!(
                "The worker is waiting for a person to answer an approval or question. It is shown in OX Desktop and on paired phones as \"{title}\" under {project}. Tell the user to answer it there."
            )
        } else {
            format!(
                "The worker is waiting for a person to answer an approval or question, and no OX Desktop window or paired phone is connected. Tell the user to open the session \"{title}\" with `ox desktop {project}` or on a paired phone, or cancel the worker."
            )
        };
        json!({
            "answer_in": ["OX Desktop", "paired phone"],
            "desktop_command": format!("ox desktop {project}"),
            "session_title": title,
            "operators_connected": operators.connected,
            "message": message,
        })
    });
    json!({
        "waiting_for_operator": waiting,
        "job_id": o.job_id,
        "mode": o.mode,
        "runtime": o.runtime,
        "can_steer": status == "running"
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
        "started_mode": o.started,
        "mode_decided": o.decided,
        "progress": progress,
        "elapsed_seconds": elapsed,
        "max_minutes": entry.marks.max_minutes,
        "warning": entry.marks.warning,
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
    fn worker(ceiling: Level, runtime: Runtime) -> Origin {
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
            requested: Level::Read,
            ceiling,
            source: None,
            started: None,
            decided: None,
            yolo: None,
            handoff: None,
        }
    }

    fn level(r: Result<modes::Resolved, String>) -> Result<(Level, Access), String> {
        r.map(|r| (r.level, r.access))
    }

    #[test]
    fn workers_inherit_the_mapped_source_mode_and_unverified_modes_grant_nothing() {
        use ModeRequest::Inherit;
        let codex = |a| source_level(Runtime::Codex, Some(a), None);
        let claude = |a| source_level(Runtime::Claude, Some(a), None);
        // Workspace and Accept edits inherit across runtimes as each other.
        assert_eq!(
            level(grant(
                codex(Access::Workspace),
                false,
                Inherit,
                Runtime::Claude
            )),
            Ok((Level::Write, Access::AcceptEdits))
        );
        assert_eq!(
            level(grant(
                claude(Access::AcceptEdits),
                false,
                Inherit,
                Runtime::Codex
            )),
            Ok((Level::Write, Access::Workspace))
        );
        // Classifier modes inherit freely.
        assert_eq!(
            level(grant(claude(Access::Auto), false, Inherit, Runtime::Codex)),
            Ok((Level::Classifier, Access::AutoReview))
        );
        assert_eq!(
            level(grant(codex(Access::Yolo), false, Inherit, Runtime::Claude)),
            Ok((Level::Yolo, Access::BypassPermissions))
        );
        // Unverified or foreign presets grant nothing.
        assert_eq!(source_level(Runtime::Codex, None, None), None);
        assert_eq!(codex(Access::Configured), None);
        assert_eq!(codex(Access::AcceptEdits), None);
        assert!(grant(None, false, Inherit, Runtime::Codex).is_err());
        assert!(grant(None, false, ModeRequest::Level(Level::Read), Runtime::Codex).is_err());
    }

    #[test]
    fn requests_above_the_source_are_refused_and_lower_ones_start_there() {
        let c = Runtime::Codex;
        let src = source_level(c, Some(Access::Workspace), None);
        assert_eq!(
            level(grant(src, false, ModeRequest::Level(Level::Read), c)),
            Ok((Level::Read, Access::ReadOnly))
        );
        assert_eq!(
            grant(src, false, ModeRequest::Level(Level::Read), c)
                .unwrap()
                .decision,
            modes::Decision::Lowered
        );
        for above in [Level::Classifier, Level::Yolo] {
            assert!(grant(src, false, ModeRequest::Level(above), c).is_err());
        }
        // Legacy workspace_write: false is read, true is write, never above the source.
        let read = source_level(c, Some(Access::ReadOnly), None);
        assert!(grant(read, false, ModeRequest::legacy(Some(true)), c).is_err());
        assert_eq!(
            level(grant(read, false, ModeRequest::legacy(Some(false)), c)),
            Ok((Level::Read, Access::ReadOnly))
        );
        assert_eq!(
            level(grant(src, false, ModeRequest::legacy(Some(true)), c)),
            Ok((Level::Write, Access::Workspace))
        );
        // A terminal gateway: read by default, write on request, never more.
        assert_eq!(
            level(grant(None, true, ModeRequest::Inherit, c)),
            Ok((Level::Read, Access::ReadOnly))
        );
        assert_eq!(
            level(grant(None, true, ModeRequest::legacy(Some(true)), c)),
            Ok((Level::Write, Access::Workspace))
        );
        assert!(grant(None, true, ModeRequest::Level(Level::Yolo), c).is_err());
        assert!(grant(None, true, ModeRequest::Level(Level::Classifier), c).is_err());
    }

    #[test]
    fn ceilings_are_inherited_through_children_forks_and_continuation() {
        let read = worker(Level::Read, Runtime::Codex);
        // A read worker switched to YOLO by its operator still gives its children only read.
        let src = source_level(Runtime::Codex, Some(Access::Yolo), Some(&read));
        assert_eq!(src, Some(Level::Read));
        assert!(grant(src, false, ModeRequest::Level(Level::Write), Runtime::Codex).is_err());
        let write = worker(Level::Write, Runtime::Codex);
        let src = source_level(Runtime::Codex, Some(Access::Workspace), Some(&write));
        assert_eq!(
            level(grant(src, false, ModeRequest::Inherit, Runtime::Claude)),
            Ok((Level::Write, Access::AcceptEdits))
        );
        // A write worker lowered to read by its operator passes on read.
        let src = source_level(Runtime::Codex, Some(Access::ReadOnly), Some(&write));
        assert_eq!(src, Some(Level::Read));
        // Forks and forks of forks keep the original stamp's ceiling and job.
        let fork = read.fork("w1");
        let again = fork.fork("w2");
        for f in [&fork, &again] {
            assert_eq!(f.ceiling, Level::Read);
            assert_eq!(f.job_id, read.job_id);
            assert_eq!(f.parent, read.parent);
            assert_eq!(f.relation, Relation::Fork);
            assert_eq!(f.limits_children(), Some(Level::Read));
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
        assert_eq!(registry.get(&s, "w1").unwrap().origin.ceiling, Level::Read);
        assert!(registry.stamp(&s, "w2", fork));
        assert_eq!(registry.job(7).map(|(_, id, _)| id), Some("w1"));
    }

    #[test]
    fn the_operator_cannot_raise_access_above_the_recorded_ceiling() {
        let read = worker(Level::Read, Runtime::Codex);
        assert!(operator_access(&read, Access::ReadOnly).is_ok());
        assert!(operator_access(&read, Access::Workspace).is_err());
        assert!(operator_access(&read, Access::Yolo).is_err());
        let write = worker(Level::Write, Runtime::Codex);
        assert!(operator_access(&write, Access::Workspace).is_ok());
        assert!(operator_access(&write, Access::AutoReview).is_err());
        assert!(operator_access(&write, Access::Yolo).is_err());
        let claude = worker(Level::Write, Runtime::Claude);
        assert!(operator_access(&claude, Access::AcceptEdits).is_ok());
        assert!(operator_access(&claude, Access::BypassPermissions).is_err());
        assert!(operator_access(&claude, Access::Auto).is_err());
        let classifier = worker(Level::Classifier, Runtime::Claude);
        assert!(operator_access(&classifier, Access::Auto).is_ok());
        assert!(operator_access(&classifier, Access::BypassPermissions).is_err());
        // A handoff target is the operator's own session.
        let mut handoff = worker(Level::Read, Runtime::Claude);
        handoff.relation = Relation::Handoff;
        assert!(operator_access(&handoff, Access::BypassPermissions).is_ok());
        assert_eq!(handoff.limits_children(), None);
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
        let entry = |o: Origin| Entry {
            origin: o,
            marks: Marks::default(),
        };
        let hosted = entry(worker(Level::Read, Runtime::Codex));
        let me = |session: &str| Resolved::Hosted {
            scope: scope(Route::CODEX_SUBSCRIPTION),
            session: session.into(),
        };
        assert!(owns(&me("parent"), &hosted));
        // A sibling worker on the parent's own app-server is a different session.
        assert!(!owns(&me("w-sibling"), &hosted));
        let elsewhere = Resolved::Hosted {
            scope: scope(Route::codex_api("x".into())),
            session: "parent".into(),
        };
        assert!(!owns(&elsewhere, &hosted));
        let terminal = |session: Option<&str>, token: &str| Resolved::Terminal {
            project: "/p".into(),
            launch: launch_digest("terminal", token),
            session: session.map(Into::into),
        };
        assert!(
            !owns(&terminal(None, "a"), &hosted),
            "hosted jobs are not terminal jobs"
        );

        // Started by launch l1 without a known session.
        let mut o = worker(Level::Read, Runtime::Codex);
        o.parent.scope = None;
        o.parent.session = None;
        o.launch = launch_digest("terminal", "l1");
        let mut started = entry(o);
        assert!(
            owns(&terminal(None, "l1"), &started),
            "the launch controls its worker"
        );
        // Another terminal launch in the same project controls nothing, with or
        // without a session of its own. There is no project-wide fallback.
        assert!(!owns(&terminal(None, "l2"), &started));
        assert!(!owns(&terminal(Some("s9"), "l2"), &started));
        // The digest in catalog rows is not the launch token.
        let published = started.origin.launch.clone();
        assert!(!owns(&terminal(None, &published), &started));
        // Once l1 binds its observed session, a resume of that session sees it.
        started.marks.parent_session = Some("s1".into());
        assert!(owns(&terminal(Some("s1"), "l3"), &started));
        assert!(!owns(&terminal(Some("s2"), "l3"), &started));

        // Recorded at launch: the session's resume owns it; others do not.
        let mut t = worker(Level::Read, Runtime::Codex);
        t.parent.scope = None;
        t.parent.session = Some("s1".into());
        t.launch = launch_digest("terminal", "l1");
        let t = entry(t);
        assert!(owns(&terminal(Some("s1"), "l2"), &t));
        assert!(!owns(&terminal(Some("s2"), "l2"), &t));
        assert!(!owns(&terminal(None, "l2"), &t));
    }

    #[test]
    fn status_reports_operator_continuation_and_restart() {
        let mut entry = Entry {
            origin: worker(Level::Read, Runtime::Codex),
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
        // No desktop or phone connected: the parent learns it must send the user there.
        let s = status(
            &entry,
            "w",
            "w",
            Some(&view("needs_input")),
            None,
            true,
            Operators::default(),
        );
        assert_eq!(s["status"], "waiting_for_operator");
        assert_eq!(s["can_steer"], false);
        let waiting = &s["waiting_for_operator"];
        assert_eq!(waiting["operators_connected"], 0);
        assert_eq!(waiting["answer_in"], json!(["OX Desktop", "paired phone"]));
        assert_eq!(waiting["desktop_command"], "ox desktop /p");
        assert!(
            waiting["message"]
                .as_str()
                .unwrap()
                .contains("no OX Desktop window or paired phone is connected")
        );
        let s = status(
            &entry,
            "w",
            "w",
            Some(&view("needs_input")),
            None,
            true,
            Operators { connected: 2 },
        );
        assert!(
            s["waiting_for_operator"]["message"]
                .as_str()
                .unwrap()
                .contains("Tell the user to answer it there")
        );
        let s = status(
            &entry,
            "w",
            "w",
            Some(&view("running")),
            None,
            true,
            Operators::default(),
        );
        assert!(s["waiting_for_operator"].is_null());
        let s = status(
            &entry,
            "w",
            "w",
            Some(&view("idle")),
            Some(&done),
            true,
            Operators::default(),
        );
        assert_eq!(
            (s["status"].as_str(), s["result"].as_str()),
            (Some("completed"), Some("final"))
        );
        assert!(s["operator"].is_null());
        entry.marks.continued = 1;
        let s = status(
            &entry,
            "w",
            "w",
            Some(&view("running")),
            Some(&done),
            true,
            Operators::default(),
        );
        assert_eq!(s["status"], "running");
        assert_eq!(s["operator"]["continued"], 1);
        let s = status(
            &entry,
            "w",
            "w",
            Some(&view("interrupted")),
            None,
            false,
            Operators::default(),
        );
        assert_eq!(s["status"], "interrupted");
        entry.marks.cancelled = true;
        assert_eq!(
            status(&entry, "w", "w", None, None, false, Operators::default())["status"],
            "cancelled"
        );
    }
}
