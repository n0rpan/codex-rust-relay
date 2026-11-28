//! Bind a destination runtime checkpoint to the authenticated staged archive.
use super::{Engine, Scope, ScopeState};
use crate::{
    move_ledger::{Activated, Commit, Prepared},
    move_target::VerifiedTarget,
};
use ox_desk_core::{
    handoff::{HandoffSource, SourceItem},
    runtime::Access,
    store::{Connection, MoveWriterPreparation},
};
use std::{collections::HashMap, io, sync::mpsc::Sender};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct TargetMoves {
    pending: HashMap<Uuid, Pending>,
}

struct Pending {
    destination: VerifiedTarget,
    source: Uuid,
    generation: u64,
    reply: Sender<io::Result<Prepared>>,
}

impl Engine {
    pub(super) fn read_moved_history(
        &self,
        scope: &Scope,
        native: &str,
        host: Option<&str>,
        session: Option<&str>,
    ) -> io::Result<Option<(HandoffSource, Vec<SourceItem>)>> {
        let ledger = &self.source_moves.ledger;
        for prepared in ledger.active_receivers()? {
            let plan = &prepared.plan;
            if &plan.target_scope == scope
                && prepared.stage.native == native
                && host.is_none_or(|id| plan.source.host.to_string() == id)
                && session
                    .is_none_or(|id| id == plan.source.native || plan.session.to_string() == id)
            {
                return VerifiedTarget::load(&self.state_dir, plan)?
                    .moved_history_for(ledger, scope, native)
                    .map(Some);
            }
        }
        Ok(None)
    }

    pub(super) fn recover_move_targets(mut self) -> io::Result<Self> {
        for prepared in self.source_moves.ledger.active_receivers()? {
            let plan = &prepared.plan;
            let restored = VerifiedTarget::load(&self.state_dir, plan).and_then(|target| {
                target.catalog_row_for(
                    &self.source_moves.ledger,
                    &plan.target_scope,
                    &prepared.stage.native,
                )
            });
            let row = match restored {
                Ok(row) => row,
                Err(error) => {
                    eprintln!(
                        "ox host: cannot restore the moved session {}: {error}",
                        plan.session
                    );
                    continue;
                }
            };
            let scope = plan.target_scope.clone();
            let state = self
                .cold
                .entry(scope.clone())
                .or_insert_with(|| ScopeState {
                    scope,
                    connection: "disconnected".into(),
                    capabilities: Vec::new(),
                    sessions: Vec::new(),
                    asks: Vec::new(),
                    transcripts: Default::default(),
                    detail: serde_json::json!({}),
                });
            state.sessions.retain(|previous| previous.id != row.id);
            state.sessions.push(row);
        }
        Ok(self)
    }

    pub(super) fn prepare_move_target(
        &mut self,
        source: Uuid,
        target: VerifiedTarget,
        access: Option<Access>,
        reply: Sender<io::Result<Prepared>>,
    ) -> io::Result<()> {
        let plan = target.plan();
        if let Some(prepared) = self.source_moves.ledger.prepared_receiver(source, plan)? {
            if !target.matches_prepared(&prepared) {
                return Err(io::Error::other(
                    "The target reservation has a different archive.",
                ));
            }
            let _ = reply.send(Ok(prepared));
            return Ok(());
        }
        let scope = &plan.target_scope;
        if !self.running.contains_key(scope) {
            self.ensure(scope, None)?;
        }
        let live = self
            .running
            .get_mut(scope)
            .ok_or_else(|| io::Error::other("Open the selected target checkout."))?;
        if live.store.state() != &Connection::Ready {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Waiting for the target runtime.",
            ));
        }
        let id = plan.id;
        if live.store.is_session_scoped() {
            let native = live.store.prepare_claude_move_writer(&id.to_string())?;
            let prepared = target.prepare(&mut self.source_moves.ledger, source, native)?;
            let _ = reply.send(Ok(prepared));
            return Ok(());
        }
        if let Some(pending) = self.target_moves.pending.get_mut(&id) {
            if pending.destination.plan() != plan || pending.source != source {
                return Err(io::Error::other(
                    "A different destination reservation is still pending.",
                ));
            }
            if pending.generation == live.generation {
                pending.reply = reply;
                return Ok(());
            }
        }
        live.store.prepare_codex_move_writer(
            id.to_string(),
            access,
            target.codex_checkpoint(None)?,
        )?;
        self.target_moves.pending.insert(
            id,
            Pending {
                destination: target,
                source,
                generation: live.generation,
                reply,
            },
        );
        Ok(())
    }

    pub(super) fn collect_target_move_preparations(&mut self, scope: &Scope) {
        let Some(live) = self.running.get_mut(scope) else {
            return;
        };
        let generation = live.generation;
        for preparation in live.store.take_move_writer_preparations() {
            self.record_target_checkpoint(scope, generation, preparation);
        }
    }

    fn record_target_checkpoint(
        &mut self,
        scope: &Scope,
        generation: u64,
        preparation: MoveWriterPreparation,
    ) {
        let Ok(id) = Uuid::parse_str(&preparation.operation) else {
            return;
        };
        if !self.target_moves.pending.get(&id).is_some_and(|pending| {
            pending.generation == generation && &pending.destination.plan().target_scope == scope
        }) {
            return;
        }
        let pending = self
            .target_moves
            .pending
            .remove(&id)
            .expect("matched preparation");
        let prepared = preparation
            .result
            .map_err(io::Error::other)
            .and_then(|thread| {
                pending
                    .destination
                    .prepare(&mut self.source_moves.ledger, pending.source, thread)
            });
        let _ = pending.reply.send(prepared);
    }

    pub(super) fn activate_move_target(
        &mut self,
        source: Uuid,
        commit: &Commit,
    ) -> io::Result<Activated> {
        let activated = self.source_moves.ledger.activate_receiver(source, commit)?;
        let plan = &commit.prepared.plan;
        if let Some(live) = self.running.get_mut(&plan.target_scope) {
            live.store
                .enable_prepared_move_writer(&plan.id.to_string(), &commit.prepared.stage.native);
        }
        Ok(activated)
    }
}
