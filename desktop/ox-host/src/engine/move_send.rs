//! Source-side validation at the boundary of the connected destination host.
use super::{Engine, Message, Scope, Server};
use crate::{
    move_bundle::{Bundle, Presentation},
    move_ledger::{Activated, ArchivePin, Commit, Plan, Prepared, Proposal, Status},
};
use ox_desk_core::{
    actions::{Action, MoveSession},
    host_protocol::Timeline,
    store::Connection,
};
use ox_host_service::checkout::project_location;
use ox_host_web::{
    move_manifest::Manifest,
    move_transport::{Offer, Reservation, Sender},
    paired_hosts::SavedHost,
};
use std::{
    collections::{HashMap, HashSet},
    io,
    os::unix::ffi::OsStrExt,
    path::Path,
    thread,
};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Outgoing {
    reserving: HashMap<Uuid, Request>,
    transferring: HashSet<Uuid>,
    pub(super) errors: HashMap<Uuid, String>,
}

struct Request {
    client: u64,
    key: String,
    scope: Scope,
    thread: String,
    turn: Option<String>,
}

pub(crate) enum Outcome {
    Reserved(Uuid, io::Result<Plan>),
    Prepared(Plan, io::Result<(ArchivePin, Prepared)>),
    Activated(Plan, io::Result<Activated>),
}

impl Engine {
    pub(super) fn start_move(
        &mut self,
        client: u64,
        key: String,
        scope: Scope,
        action: MoveSession,
    ) -> io::Result<()> {
        let operation = parse_id(&action.operation)?;
        let destination = parse_id(&action.destination)?;
        if self.host_id == destination || self.move_send.reserving.contains_key(&operation) {
            return Err(io::Error::other(
                "Choose another connected host for this move.",
            ));
        }
        if let Some(plan) = self.source_moves.ledger.source_plan(operation)? {
            if plan.source.scope == scope
                && plan.source.native == action.thread
                && plan.target_host == destination
            {
                return self.reply(client, Server::Ack { id: key });
            }
            return Err(io::Error::other("The request ID belongs to another move."));
        }
        if self
            .move_send
            .reserving
            .values()
            .any(|request| request.scope == scope && request.thread == action.thread)
        {
            return Err(io::Error::other(
                "The destination is already checking this session.",
            ));
        }
        let live = self
            .running
            .get(&scope)
            .ok_or_else(|| io::Error::other("Open the session before moving it."))?;
        if live.store.state() != &Connection::Ready
            || !live
                .state
                .sessions
                .iter()
                .any(|view| view.id == action.thread && view.detail["owned"] == true)
        {
            return Err(io::Error::other(
                "Open the source session before moving it.",
            ));
        }
        let turn = if action.after_turn {
            Some(
                live.store
                    .transcript(&action.thread)
                    .and_then(|view| view.active_turn())
                    .ok_or_else(|| {
                        io::Error::other("The active turn changed. Open Move session again.")
                    })?
                    .to_owned(),
            )
        } else {
            None
        };
        let (session, source) = self
            .source_moves
            .ledger
            .source_identity(&scope, &action.thread)?;
        let state = self.state_dir.clone();
        let actor = self.tx.clone();
        self.move_send.reserving.insert(
            operation,
            Request {
                client,
                key,
                scope,
                thread: action.thread,
                turn,
            },
        );
        thread::spawn(move || {
            let result = (|| {
                let destination = linked_host(&state, destination)?;
                let (_, relative) = project_location(&source.scope.project)?;
                reserve(
                    &destination,
                    operation,
                    Proposal {
                        session,
                        source,
                        project_in_checkout: relative.as_os_str().as_bytes().to_vec(),
                    },
                )
            })();
            let _ = actor.send(Message::MoveSend(Box::new(Outcome::Reserved(
                operation, result,
            ))));
        });
        Ok(())
    }

    pub(super) fn move_command(&mut self, scope: &Scope, command: &Action) -> io::Result<()> {
        let id = match command {
            Action::CancelMove(id) | Action::CheckMove(id) | Action::RetryMove(id) => parse_id(id)?,
            _ => return Err(io::Error::other("Unknown move action.")),
        };
        let plan = self
            .source_moves
            .ledger
            .source_plan(id)?
            .filter(|plan| &plan.source.scope == scope)
            .ok_or_else(|| io::Error::other("This move belongs to another session."))?;
        if matches!(command, Action::CancelMove(_)) {
            self.source_moves.ledger.cancel_source(&plan)?;
        }
        self.move_send.errors.remove(&id);
        self.drive_move_transfers()
    }

    pub(super) fn move_outcome(&mut self, outcome: Outcome) -> io::Result<()> {
        let (id, result) = match outcome {
            Outcome::Reserved(id, result) => {
                let Some(request) = self.move_send.reserving.remove(&id) else {
                    return Ok(());
                };
                let result = result.and_then(|plan| match request.turn {
                    Some(turn) => self.queue_move_for_turn(&plan, turn),
                    None => self.prepare_source_move(&plan, false),
                });
                let frame = match result {
                    Ok(_) => Server::Ack { id: request.key },
                    Err(error) => Server::Error {
                        id: Some(request.key),
                        message: error.to_string(),
                    },
                };
                self.reply(request.client, frame)?;
                return self.drive_move_transfers();
            }
            Outcome::Prepared(plan, result) => {
                if self.source_moves.ledger.source_status(&plan)? != Status::Preparing {
                    self.move_send.transferring.remove(&plan.id);
                    return Ok(());
                }
                let result = result.and_then(|(archive, prepared)| {
                    self.bind_move_manifest(&plan, archive)?;
                    self.stage_source_move(plan.target_host, &prepared)
                        .map(|_| ())
                });
                (plan.id, result)
            }
            Outcome::Activated(plan, result) => (
                plan.id,
                result.and_then(|receipt| self.confirm_move(plan.target_host, &receipt)),
            ),
        };
        self.move_send.transferring.remove(&id);
        if let Err(error) = result {
            self.move_send.errors.insert(id, error.to_string());
        }
        self.drive_move_transfers()
    }

    pub(super) fn drive_move_transfers(&mut self) -> io::Result<()> {
        for (plan, phase) in self.source_moves.ledger.pending_sources()? {
            if self.move_send.transferring.contains(&plan.id)
                || self.move_send.errors.contains_key(&plan.id)
            {
                continue;
            }
            let work = match phase {
                Status::Preparing => {
                    let Ok(timeline) = self.move_source_timeline(&plan) else {
                        continue;
                    };
                    let Some(row) = self.running[&plan.source.scope]
                        .state
                        .sessions
                        .iter()
                        .find(|row| row.id == plan.source.native)
                    else {
                        continue;
                    };
                    Work::Prepare(Presentation::from(row), timeline.clone())
                }
                Status::Committed => Work::Activate(self.committed_move(&plan)?),
                _ => continue,
            };
            self.move_send.transferring.insert(plan.id);
            let state = self.state_dir.clone();
            let actor = self.tx.clone();
            thread::spawn(move || {
                let outcome = match work {
                    Work::Prepare(presentation, timeline) => {
                        let result = (|| {
                            let destination = linked_host(&state, plan.target_host)?;
                            let bundle = Bundle::capture(&state, &plan, presentation, &timeline)?;
                            let archive = ArchivePin {
                                manifest: bundle.manifest.sha256()?,
                                transcript: bundle.manifest.transcript.sha256,
                            };
                            Ok((archive, prepare(&destination, &plan, &bundle)?))
                        })();
                        Outcome::Prepared(plan, result)
                    }
                    Work::Activate(commit) => {
                        let result = linked_host(&state, plan.target_host)
                            .and_then(|destination| activate(&destination, &state, &commit));
                        Outcome::Activated(plan, result)
                    }
                };
                let _ = actor.send(Message::MoveSend(Box::new(outcome)));
            });
        }
        Ok(())
    }
}

enum Work {
    Prepare(Presentation, Timeline),
    Activate(Commit),
}

fn linked_host(state: &Path, id: Uuid) -> io::Result<SavedHost> {
    let id = id.to_string();
    ox_host_web::paired_hosts::worker_links(&state.join("worker-links.json"))?
        .into_iter()
        .find(|candidate| candidate.id == id)
        .ok_or_else(|| io::Error::other("The destination host link is unavailable."))
}

fn parse_id(id: &str) -> io::Result<Uuid> {
    Uuid::parse_str(id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid host or move ID."))
}

pub(super) fn reserve(
    destination: &SavedHost,
    operation: Uuid,
    proposal: Proposal,
) -> io::Result<Plan> {
    let sender = Sender { destination };
    let reserve = Reservation {
        source: proposal.source.host.to_string(),
        target: destination.id.clone(),
        transfer: operation.to_string(),
        proposal: serde_json::to_string(&proposal)?,
    };
    let receipt = sender.reserve(&reserve)?;
    let raw = receipt
        .as_str()
        .ok_or_else(|| io::Error::other("The destination did not return a session move plan."))?;
    let plan: Plan = serde_json::from_str(raw)?;
    if plan.id != operation
        || plan.session != proposal.session
        || plan.source != proposal.source
        || plan.target_host.to_string() != destination.id
        || plan.target_scope.route != proposal.source.scope.route
    {
        return Err(io::Error::other(
            "The destination approved a different session move.",
        ));
    }
    Ok(plan)
}

pub(super) fn prepare(
    destination: &SavedHost,
    plan: &Plan,
    bundle: &Bundle,
) -> io::Result<Prepared> {
    let offer = offer(destination, plan, &bundle.manifest)?;
    let prepared: Prepared = Sender { destination }.prepare(&offer, bundle.directory())?;
    if prepared.plan != *plan || prepared.stage.digest != offer.manifest.sha256()? {
        return Err(io::Error::other(
            "The destination prepared different move files.",
        ));
    }
    Ok(prepared)
}

pub(super) fn activate(
    destination: &SavedHost,
    state: &Path,
    commit: &Commit,
) -> io::Result<Activated> {
    let plan = &commit.prepared.plan;
    let bundle = Bundle::recover(state, plan)?;
    let offer = offer(destination, plan, &bundle.manifest)?;
    if offer.manifest.sha256()? != commit.prepared.stage.digest {
        return Err(io::Error::other(
            "The staged files differ from the committed move.",
        ));
    }
    let activated: Activated =
        serde_json::from_value(Sender { destination }.activate(&offer, commit)?)?;
    if activated.commit != *commit {
        return Err(io::Error::other(
            "The destination confirmed a different move.",
        ));
    }
    Ok(activated)
}

fn offer(destination: &SavedHost, plan: &Plan, manifest: &Manifest) -> io::Result<Offer> {
    if destination.id != plan.target_host.to_string() {
        return Err(io::Error::other("The saved link belongs to another host."));
    }
    Ok(Offer {
        source: plan.source.host.to_string(),
        target: destination.id.clone(),
        transfer: plan.id.to_string(),
        manifest: manifest.clone(),
    })
}
