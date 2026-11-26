//! Worker transitions addressed to the hosted session that launched the job.
//! The outbox is part of the ownership ledger. A short stable message lets a
//! restarted host reconcile a send against the parent's native history.
use ox_desk_core::{
    host_protocol::Scope,
    origin::{Origin, Relation},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Notices {
    #[serde(default)]
    pub initialized: bool,
    #[serde(default)]
    pub observed: BTreeMap<u64, String>,
    #[serde(default)]
    pub pending: Vec<Notice>,
    /// The most recent event per parent for Desktop and phone catalog snapshots.
    #[serde(default)]
    pub latest: Vec<Summary>,
    #[serde(default)]
    next: u64,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Notice {
    pub id: u64,
    pub scope: Scope,
    pub session: String,
    pub job: u64,
    pub status: String,
    pub text: String,
    pub submitted: bool,
    pub announced: bool,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Summary {
    pub scope: Scope,
    pub session: String,
    pub job_id: u64,
    pub status: String,
}

impl Summary {
    pub fn detail(&self) -> serde_json::Value {
        serde_json::json!({"job_id": self.job_id, "status": self.status})
    }
}
impl Notices {
    pub fn apply_to(&self, state: &mut ox_desk_core::host_protocol::ScopeState) {
        for summary in self.latest.iter().filter(|s| s.scope == state.scope) {
            if let Some(row) = state.sessions.iter_mut().find(|r| r.id == summary.session) {
                row.detail["worker_notice"] = summary.detail();
            }
        }
    }

    pub fn observe(&mut self, origin: &Origin, status: &str, preview: &str) {
        let (Some(scope), Some(session)) = (&origin.parent.scope, &origin.parent.session) else {
            return;
        };
        if origin.relation != Relation::Worker
            || origin.by_operator
            || self
                .observed
                .get(&origin.job_id)
                .is_some_and(|s| s == status)
        {
            return;
        }
        self.observed.insert(origin.job_id, status.into());
        if status == "running" {
            return;
        }
        self.latest
            .retain(|s| &s.scope != scope || &s.session != session);
        self.latest.push(Summary {
            scope: scope.clone(),
            session: session.clone(),
            job_id: origin.job_id,
            status: status.into(),
        });
        self.next += 1;
        let excerpt: String = preview
            .trim()
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(480)
            .collect();
        let mut text = format!(
            "OX worker #{}: {status} (update {}).",
            origin.job_id, self.next
        );
        if !excerpt.is_empty() {
            text.push('\n');
            text.push_str(excerpt.trim_end());
        }
        self.pending.push(Notice {
            id: self.next,
            scope: scope.clone(),
            session: session.clone(),
            job: origin.job_id,
            status: status.into(),
            text,
            submitted: false,
            announced: false,
        });
    }
}
