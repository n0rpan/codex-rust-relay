//! YOLO and bypass children need a person. A child that would start in YOLO
//! waits for the operator, who sees an ask on the desktop and on paired phones
//! (with the usual badge and Web Push) on the parent session: Accept YOLO, Start
//! in the highest non-YOLO mode within the ceiling, or Decline. The parent agent
//! has no operation that answers it; it sees `waiting_for_operator`.
use crate::engine::Asking;
use ox_desk_core::{
    host_protocol::{AskView, Change, NotificationKind, Scope},
    modes::Level,
    runtime::{Billing, RequestId},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) const PREFIX: &str = ox_desk_core::asks::YOLO_REQUEST_PREFIX;

pub(crate) fn request_id(job: u64) -> RequestId {
    RequestId::Text(format!("{PREFIX}{job}"))
}
pub(crate) fn job_of(id: &RequestId) -> Option<u64> {
    match id {
        RequestId::Text(t) => t.strip_prefix(PREFIX)?.parse().ok(),
        RequestId::Number(_) => None,
    }
}
pub(crate) fn is_request(ask: &AskView) -> bool {
    job_of(&ask.id).is_some()
}

/// The operator's three answers, in order.
pub(crate) enum Answer {
    Accept,
    Lower,
    Decline,
}
impl Answer {
    pub(crate) fn of(choice: usize) -> Option<Self> {
        [Self::Accept, Self::Lower, Self::Decline]
            .into_iter()
            .nth(choice)
    }
}

pub(crate) fn model(a: &Asking) -> String {
    match &a.origin.route.billing {
        Billing::Api(alias) => alias.clone(),
        Billing::Subscription => format!("{} subscription default", a.origin.runtime.account()),
    }
}

pub(crate) fn view(a: &Asking) -> AskView {
    let o = &a.origin;
    let lower = Level::Yolo.below_yolo().label(o.runtime);
    let task = a.start.task.trim();
    let task = &task[..task.floor_char_boundary(600)];
    AskView {
        command: None,
        id: request_id(o.job_id),
        token: String::new(),
        thread: o.parent.session.clone(),
        turn: None,
        item: None,
        choice_details: vec![
            Some("Run without permission prompts.".into()),
            Some(format!("Start with {lower} instead.")),
            Some("Do not start this worker.".into()),
        ],
        title: format!("Start a {} worker in YOLO?", o.runtime.name()),
        detail: format!(
            "Parent: {}\nRuntime: {}\nRoute: {}\nModel: {}\nTask: {task}",
            a.parent_title,
            o.runtime.name(),
            o.route.label(),
            model(a),
        ),
        choices: vec![
            "Accept YOLO".into(),
            format!("Start in {lower}"),
            "Decline".into(),
        ],
        questions: vec![],
    }
}

/// Add the YOLO requests whose parent lives in `scope` to its asks, keeping the
/// tokens already issued, and announce new ones with an approval notification.
pub(crate) fn merge(
    asking: &BTreeMap<u64, Asking>,
    activations: impl Iterator<Item = AskView>,
    scope: &Scope,
    previous: Vec<AskView>,
    asks: &mut Vec<AskView>,
    changes: &mut Vec<Change>,
) {
    let mut wanted: Vec<AskView> = asking
        .values()
        .filter(|a| a.origin.parent.scope.as_ref() == Some(scope))
        .map(view)
        .chain(activations)
        .map(|mut v| {
            if let Some(old) = previous.iter().find(|p| p.id == v.id) {
                v.token.clone_from(&old.token);
            }
            v
        })
        .collect();
    let same =
        wanted.len() == previous.len() && wanted.iter().zip(&previous).all(|(a, b)| a.id == b.id);
    for new in wanted
        .iter()
        .filter(|w| !previous.iter().any(|p| p.id == w.id))
    {
        if let Some(thread) = &new.thread {
            let title = asking
                .get(&job_of(&new.id).unwrap_or_default())
                .map_or("OX session", |a| a.parent_title.as_str());
            changes.push(Change::Notification {
                thread: thread.clone(),
                title: title.into(),
                event: NotificationKind::Approval,
                goal_reached: false,
            });
        }
    }
    asks.append(&mut wanted);
    if !same && !changes.iter().any(|c| matches!(c, Change::Asks { .. })) {
        changes.push(Change::Asks { asks: asks.clone() });
    }
}

/// What the parent agent reads for a job that is waiting for the operator.
pub(crate) fn status(a: &Asking, operators: usize) -> Value {
    let o = &a.origin;
    let message = if operators > 0 {
        format!(
            "This {} worker would run in YOLO, so it waits for a person to accept it in OX Desktop or on a paired phone, on the session \"{}\". Tell the user to answer there.",
            o.runtime.name(),
            a.parent_title
        )
    } else {
        format!(
            "This {} worker would run in YOLO, so it waits for a person to accept it, and no OX Desktop window or paired phone is connected. Tell the user to open \"{}\" with `ox desktop {}` or on a paired phone.",
            o.runtime.name(),
            a.parent_title,
            o.parent.project.display()
        )
    };
    json!({
        "job_id": o.job_id,
        "status": "waiting_for_operator",
        "runtime": o.runtime,
        "mode": o.mode,
        "owner": "host",
        "access_ceiling": o.ceiling,
        "can_steer": false,
        "progress": "Waiting for the operator to accept YOLO",
        "waiting_for_operator": {
            "reason": "yolo_acceptance",
            "answer_in": ["OX Desktop", "paired phone"],
            "desktop_command": format!("ox desktop {}", o.parent.project.display()),
            "session_title": a.parent_title,
            "operators_connected": operators,
            "message": message,
        },
        "messages": [],
    })
}

/// What the parent agent reads for a job that never became a session.
pub(crate) fn outcome(o: &crate::durable::Outcome) -> Value {
    json!({
        "job_id": o.origin.job_id,
        "status": o.status,
        "runtime": o.origin.runtime,
        "mode": o.origin.mode,
        "owner": "host",
        "can_steer": false,
        "error": o.message,
        "messages": [],
    })
}
