//! Bind a destination runtime checkpoint to the authenticated staged archive.
use super::{Engine, Scope};
use crate::{
    move_ledger::{Activated, Commit, Prepared},
    move_target::VerifiedTarget,
};
use ox_desk_core::{
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
    pub(super) fn prepare_codex_move_target(
        &mut self,
        source: Uuid,
        target: VerifiedTarget,
        access: Option<Access>,
        window: Option<u64>,
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
                "Waiting for the target Codex route.",
            ));
        }
        let id = plan.id;
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
            target.codex_checkpoint(window)?,
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
