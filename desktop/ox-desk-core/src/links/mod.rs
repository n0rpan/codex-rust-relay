//! Owned runtime connections and events delivered to the workbench.
use crate::codex::frame::Frame;
use std::io;

pub mod claude;
pub mod codex;

pub enum Link {
    Host(Box<crate::store::replica::Remote>),
    Codex(codex::Link),
    Claude(Box<crate::store::claude::Project>),
}
impl From<codex::Link> for Link {
    fn from(link: codex::Link) -> Self {
        Self::Codex(link)
    }
}
impl Link {
    pub(crate) fn codex(&self) -> io::Result<&codex::Link> {
        match self {
            Self::Codex(link) => Ok(link),
            Self::Claude(_) | Self::Host(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "This operation requires a Codex session",
            )),
        }
    }
    pub(crate) fn forget(&self, seq: i64) {
        match self {
            Self::Codex(link) => link.forget(seq),
            Self::Claude(_) | Self::Host(_) => (),
        }
    }
}

#[derive(Debug)]
pub enum LinkEvent {
    Host(Box<crate::host_protocol::Server>),
    Title {
        key: crate::titles::Key,
        name: Option<String>,
    },
    SignInRequired,
    Frame(Frame),
    Claude(crate::store::claude::Update),
    Closed {
        reason: String,
        stderr_tail: Vec<String>,
    },
}

impl LinkEvent {
    /// Responses and permission requests unblock an interaction. Streaming activity
    /// still waits for the next display frame and retains its original wire order.
    pub fn unblocks_interaction(&self) -> bool {
        if let Self::Host(frame) = self {
            use crate::host_protocol::{Change, Server};
            return !matches!(frame.as_ref(),Server::Delta{changes,..} if changes.iter().all(|c| matches!(c,Change::Append{..}|Change::Items{..}|Change::TimelineState{..})));
        }
        if let Self::Claude(update) = self {
            return update.unblocks();
        }
        matches!(
            self,
            Self::Frame(Frame::Reply { .. } | Frame::Ask(_))
                | Self::Closed { .. }
                | Self::SignInRequired
        )
    }
}
