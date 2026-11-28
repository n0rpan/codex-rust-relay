//! Release the native writer for one already-fenced session.
use super::{Call, Connection, Pending, Store};
use crate::{changed::Changed, codex::commands::UnsubscribeStatus};
use std::io;

#[derive(Debug, PartialEq, Eq)]
pub struct MoveWriterRelease {
    /// Exact OX thread key within the Store's project and billing route.
    pub thread: String,
    pub result: Result<(), String>,
}

#[derive(Default)]
pub(super) struct CodexRelease {
    acknowledged: bool,
    closed: bool,
}

impl Store {
    /// Starts release of an idle native writer. The host keeps its durable move
    /// fence in force while it waits for the asynchronous result.
    pub fn release_writer_for_move(&mut self, id: &str) -> io::Result<()> {
        if self.pending_codex_moves.contains_key(id) {
            return Ok(());
        }
        if self.is_host_client()
            || self.state != Connection::Ready
            || self.in_terminal(id)
            || self.is_native_live(id)
            || !self
                .transcript(id)
                .is_some_and(|t| t.is_ready() && !t.is_loading())
        {
            return Err(io::Error::other(
                "Open the owned source session before releasing it.",
            ));
        }
        if self.thread_busy(id)
            || self.changing_options(id)
            || self.pending.values().any(|(_, p)| p.loads_history_for(id))
        {
            return Err(io::Error::other(
                "Wait for the source session's work to finish.",
            ));
        }
        if self.is_session_scoped() {
            self.release_claude_writer_for_move(id)?;
        } else {
            self.request(
                Call::Unsubscribe(id.into()),
                Pending::LeaveForMove(id.into()),
            )?;
            self.pending_codex_moves
                .insert(id.into(), CodexRelease::default());
        }
        self.moving_threads.insert(id.into());
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }

    pub fn take_move_writer_releases(&mut self) -> Vec<MoveWriterRelease> {
        std::mem::take(&mut self.move_writer_releases)
    }

    pub(super) fn acknowledge_codex_move_release(&mut self, id: &str, status: UnsubscribeStatus) {
        if let Some(release) = self.pending_codex_moves.get_mut(id) {
            release.acknowledged = true;
            // The app-server can leave a runtime loaded after the last unsubscribe.
            // Wait for its thread/closed signal unless it is already unloaded.
            if release.closed || matches!(status, UnsubscribeStatus::NotLoaded) {
                self.complete_move_writer_release(id, Ok(()));
            }
        }
    }

    pub(super) fn observe_codex_move_close(&mut self, id: &str) -> bool {
        let Some(release) = self.pending_codex_moves.get_mut(id) else {
            return false;
        };
        release.closed = true;
        if release.acknowledged {
            self.complete_move_writer_release(id, Ok(()));
        }
        true
    }

    pub(super) fn complete_move_writer_release(&mut self, id: &str, result: Result<(), String>) {
        self.pending_codex_moves.remove(id);
        if result.is_ok()
            && let Some(t) = self.transcripts.get_mut(id)
        {
            t.ready = false;
            t.verified = false;
            t.active_turn = None;
        }
        self.move_writer_releases.push(MoveWriterRelease {
            thread: id.into(),
            result,
        });
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codex::{frame::Frame, names::Method},
        links::LinkEvent,
        runtime::Route,
        store::Profile,
    };
    use serde_json::{Value, json};

    fn fixture() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::inert(Profile {
            project: root.path().into(),
            route: Route::CODEX_SUBSCRIPTION,
            provider: "openai".into(),
        })
        .unwrap();
        store.state = Connection::Ready;
        for id in ["first", "second", "unrelated"] {
            let t = store.transcripts.entry(id.into()).or_default();
            t.ready = true;
            t.verified = true;
        }
        (root, store)
    }

    fn reply(store: &mut Store, id: i64, response: Value) {
        let mut response = response;
        response["id"] = id.into();
        store.apply(LinkEvent::Frame(
            Frame::decode(&serde_json::to_vec(&response).unwrap(), |_| {
                Some(Method::Unsubscribe)
            })
            .unwrap(),
        ));
    }

    fn closed(store: &mut Store, id: &str) {
        let frame = json!({"method":"thread/closed","params":{"threadId":id}});
        store.apply(LinkEvent::Frame(
            Frame::decode(&serde_json::to_vec(&frame).unwrap(), |_| None).unwrap(),
        ));
    }

    #[test]
    fn codex_release_waits_for_the_matching_unsubscribe_reply() {
        let (_root, mut store) = fixture();
        store.release_writer_for_move("first").unwrap();
        let first = store.seq;
        store.release_writer_for_move("first").unwrap();
        assert_eq!(
            store.seq, first,
            "joining an in-flight release sends no extra request"
        );
        store.release_writer_for_move("second").unwrap();
        let second = store.seq;
        assert!(store.take_move_writer_releases().is_empty());
        assert!(store.take_handoff().is_none());
        assert!(!store.can_send(Some("first")));
        assert!(store.can_send(Some("unrelated")));
        assert!(store.open_catalog_id("first").is_err());

        reply(
            &mut store,
            second,
            json!({"error":{"code":-32000,"message":"Release refused"}}),
        );
        let failed = store.take_move_writer_releases();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].thread, "second");
        assert!(
            failed[0]
                .result
                .as_ref()
                .unwrap_err()
                .contains("Release refused")
        );
        assert!(store.transcript("second").unwrap().is_ready());
        assert!(!store.can_send(Some("second")));

        closed(&mut store, "first");
        assert!(store.take_move_writer_releases().is_empty());
        reply(
            &mut store,
            first,
            json!({"result":{"status":"unsubscribed"}}),
        );
        assert_eq!(
            store.take_move_writer_releases(),
            [MoveWriterRelease {
                thread: "first".into(),
                result: Ok(())
            }]
        );
        assert!(!store.transcript("first").unwrap().is_ready());
        assert!(store.can_send(Some("unrelated")));
        assert!(store.take_handoff().is_none());

        store.release_writer_for_move("second").unwrap();
        let retry = store.seq;
        reply(
            &mut store,
            retry,
            json!({"result":{"status":"notSubscribed"}}),
        );
        closed(&mut store, "first");
        assert!(store.take_move_writer_releases().is_empty());
        closed(&mut store, "second");
        assert_eq!(
            store.take_move_writer_releases(),
            [MoveWriterRelease {
                thread: "second".into(),
                result: Ok(())
            }]
        );
    }

    #[test]
    fn busy_and_closed_codex_writers_never_report_release_success() {
        let (_root, mut store) = fixture();
        store.transcripts.get_mut("first").unwrap().active_turn = Some("running".into());
        assert!(store.release_writer_for_move("first").is_err());
        assert!(!store.moving_threads.contains("first"));
        store.release_writer_for_move("second").unwrap();
        let request = store.seq;
        reply(
            &mut store,
            request,
            json!({"result":{"status":"unsubscribed"}}),
        );
        assert!(store.take_move_writer_releases().is_empty());
        store.apply(LinkEvent::Closed {
            reason: "App-server disconnected before thread closure".into(),
            stderr_tail: vec![],
        });
        let result = store.take_move_writer_releases();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].thread, "second");
        assert!(result[0].result.is_err());
    }

    #[test]
    fn an_unloaded_codex_reply_confirms_release_without_an_extra_close() {
        let (_root, mut store) = fixture();
        store.release_writer_for_move("first").unwrap();
        let id = store.seq;
        reply(&mut store, id, json!({"result":{"status":"notLoaded"}}));
        assert_eq!(
            store.take_move_writer_releases(),
            [MoveWriterRelease {
                thread: "first".into(),
                result: Ok(())
            }]
        );
    }
}
