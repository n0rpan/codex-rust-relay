//! Native thread actions retain the originating selection across asynchronous replies.
use super::{Pending, Store};
use crate::codex::{
    reply::Reply,
    request::{Call, ThreadCommand},
};
use std::io;

#[derive(Debug)]
pub(crate) enum ThreadOperation {
    Rename(String),
    Archive,
    Fork,
}
#[derive(Debug)]
pub(super) struct PendingOp {
    id: String,
    selection: u64,
    op: ThreadOperation,
}

impl PendingOp {
    pub(super) fn is_fork(&self) -> bool {
        matches!(self.op, ThreadOperation::Fork)
    }
}
impl Store {
    pub fn can_edit_thread_metadata(&self) -> bool {
        if let Some(remote) = self.remote() {
            return remote.scope_caps.iter().any(|x| x == "edit_metadata");
        }
        !self.is_session_scoped()
    }
    pub fn can_manage_thread(&self, id: &str) -> bool {
        if let Some(remote) = self.remote() {
            return remote.can(id, "fork");
        }
        // A session another client is running can still be forked into a new OX one.
        (self.can_change_access(id) || self.is_native_live(id))
            && self.catalog.get(id).is_some_and(|r| !r.worker)
            && !self
                .pending
                .values()
                .any(|(_, p)| matches!(p,Pending::ThreadOp(p) if p.id==id))
    }
    pub(crate) fn manage_thread(&mut self, id: &str, op: ThreadOperation) -> io::Result<()> {
        if !self.can_manage_thread(id) {
            return Err(io::Error::other("Select an idle thread to change it"));
        }
        if matches!(op, ThreadOperation::Archive) && self.is_native_live(id) {
            return Err(io::Error::other(
                "Another client is running this thread; it can't be archived from here",
            ));
        }
        if self.is_session_scoped() {
            return match op {
                ThreadOperation::Fork => self.claude_fork(id),
                _ => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "This Claude session action is unavailable",
                )),
            };
        }
        let call = match &op {
            ThreadOperation::Rename(name) if name.trim().is_empty() => {
                return Err(io::Error::other("Enter a thread name"));
            }
            ThreadOperation::Rename(name) => Call::Command {
                thread: id.into(),
                command: ThreadCommand::Rename(name.trim().into()),
            },
            ThreadOperation::Archive => Call::Archive(id.into()),
            ThreadOperation::Fork => Call::Fork {
                preset: self.host_start_access(id),
                thread: id.into(),
                project: self
                    .catalog
                    .get(id)
                    .expect("verified catalog thread")
                    .cwd
                    .clone(),
                model: self.transcripts[id].model.clone(),
                provider: self.profile.provider.clone(),
            },
        };
        self.request(
            call,
            Pending::ThreadOp(PendingOp {
                id: id.into(),
                selection: self.selection,
                op,
            }),
        )
    }
    pub(super) fn finish_thread_op(&mut self, op: PendingOp, reply: Reply) -> io::Result<()> {
        match (op.op, reply) {
            // Codex publishes the authoritative name and archive state.
            (ThreadOperation::Rename(_) | ThreadOperation::Archive, Reply::Acknowledged) => Ok(()),
            (ThreadOperation::Fork, Reply::Opened(fork)) => {
                let id = fork.thread.id.clone();
                if id == op.id {
                    return Err(io::Error::other(
                        "Codex returned the source thread for the fork",
                    ));
                }
                if let Some(alias) = self.expected_alias(&op.id).map(str::to_owned) {
                    self.authorized_models.insert(id.clone(), alias);
                }
                if let Err(err) = self.verify_opened(&id, &fork) {
                    self.authorized_models.remove(&id);
                    return Err(err);
                }
                self.host_opened = Some(id.clone());
                // The fork produces thread/started. History loads directly from the new thread.
                if self.selected() == Some(&op.id) && self.selection == op.selection {
                    self.selection += 1;
                    self.selected = Some(id.clone());
                    self.mark_loading(&id);
                    self.checked_resume(&id, *fork);
                }
                Ok(())
            }
            _ => Err(io::Error::other("Unexpected reply to the thread action")),
        }
    }
}
