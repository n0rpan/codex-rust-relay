//! Desktop-brokered reads use the same authenticated operator pairings as the handoff.
use super::*;
use std::collections::{HashMap, HashSet};

const UNAVAILABLE: &str = "The source belongs to another host. Open the desktop that made the handoff and connect both hosts, then retry.";

struct Pending {
    expected: wire::HistoryRef,
    waiting: HashSet<u64>,
    reply: Replier,
    deadline: Instant,
    error: Option<String>,
}
#[derive(Default)]
pub(in crate::engine) struct Relays {
    desktops: HashMap<u64, String>,
    reads: HashMap<String, Pending>,
}
impl Relays {
    pub(in crate::engine) fn watch(&mut self, client: u64, proof: &str) {
        use sha2::{Digest, Sha256};
        if proof.len() == 64 && proof.bytes().all(|b| b.is_ascii_hexdigit()) {
            self.desktops
                .insert(client, format!("{:x}", Sha256::digest(proof.as_bytes())));
        }
    }
    pub(in crate::engine) fn deadline(&self) -> Option<Instant> {
        self.reads.values().map(|r| r.deadline).min()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Export {
    recipient: String,
    source: HandoffSource,
}
impl Engine {
    fn export_path(&self, transfer: &str) -> io::Result<PathBuf> {
        let id = uuid::Uuid::parse_str(transfer).map_err(io::Error::other)?;
        Ok(self
            .state_dir
            .join("handoff-exports")
            .join(format!("{id}.json")))
    }
    pub(super) fn restore_export_boundary(
        &self,
        id: uuid::Uuid,
        recipient: &str,
        scope: &Scope,
        session: &str,
        items: &mut Vec<handoff::SourceItem>,
    ) -> io::Result<()> {
        let file = match std::fs::File::open(self.export_path(&id.to_string())?) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let old: Export = serde_json::from_reader(file)?;
        if old.recipient != recipient || &old.source.scope != scope || old.source.session != session
        {
            return Err(io::Error::other(
                "This handoff ID belongs to another source or destination.",
            ));
        }
        freeze(items, &old.source).map_err(io::Error::other)
    }
    pub(super) fn record_export(&self, packet: &handoff::HandoffTransfer) -> io::Result<()> {
        let path = self.export_path(&packet.id)?;
        match std::fs::File::open(&path) {
            Ok(f) => {
                let old: Export = serde_json::from_reader(f)?;
                if old.recipient != packet.target_host
                    || old.source.scope != packet.source.scope
                    || old.source.session != packet.source.session
                    || old.source.selection.items.last() != packet.source.selection.items.last()
                {
                    return Err(io::Error::other(
                        "The source changed since that handoff. Open the receiving host to check the earlier session.",
                    ));
                }
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => ox_desk_core::state_file::write_json(
                &path,
                &Export {
                    recipient: packet.target_host.clone(),
                    source: packet.source.clone(),
                },
            ),
            Err(e) => Err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn read_history_of(
        &mut self,
        owner: &Scope,
        session: &str,
        host: Option<&str>,
        wanted: Option<&str>,
        mut page: wire::HandoffPage,
        reply: Replier,
        broker: Option<&str>,
    ) -> io::Result<()> {
        let chain = self.handoff_chain(owner, session);
        if chain.is_empty() {
            let message = "This session was not handed off from another session, so it has no source to read.";
            return self.answer(reply, Err(message.into()));
        }
        let local = self.host_id.to_string();
        let names_host = |stamp: Option<&handoff::SourceHost>| {
            host.is_none_or(|id| stamp.is_some_and(|h| h.id == id))
        };
        let mut matches = Vec::new();
        for source in &chain {
            if names_host(source.host.as_ref())
                && wanted
                    .is_none_or(|id| id == source.session || source.native.as_deref() == Some(id))
            {
                matches.push((source, None));
            }
            if source.host.as_ref().is_some_and(|h| h.id != local) {
                for ancestor in &source.selection.earlier {
                    if let Some(machine) = &ancestor.host
                        && names_host(Some(machine))
                        && wanted.is_none_or(|s| s == ancestor.session)
                    {
                        matches.push((
                            source,
                            Some(wire::HistoryRef {
                                host: machine.id.clone(),
                                session: ancestor.session.clone(),
                            }),
                        ));
                    }
                }
            }
        }
        if matches.len() > 1 && wanted.is_some() {
            return self.answer(
                reply,
                Err("That session ID occurs more than once. Include its source host ID.".into()),
            );
        }
        let Some((source, ancestor)) = matches.into_iter().next() else {
            return self.answer(
                reply,
                Err("That session is not in this session's handoff chain.".into()),
            );
        };
        if source.host.as_ref().is_some_and(|h| h.id != local) {
            page.ancestor = ancestor;
            return self.relay_history(owner, session, source, page, reply, broker);
        }
        if source.scope.project != owner.project {
            return self.answer(
                reply,
                Err("That source is outside this session's project.".into()),
            );
        }
        self.load_source_page(source.clone(), page, reply, broker.is_some())
    }
    /// Walk the verified caller's own lineage to find the foreign transfer, including when
    /// the caller continued locally again after moving here.
    pub(super) fn relay_history(
        &mut self,
        scope: &Scope,
        session: &str,
        source: &HandoffSource,
        page: wire::HandoffPage,
        reply: Replier,
        broker: Option<&str>,
    ) -> io::Result<()> {
        let mut scope = scope.clone();
        let mut session = session.to_owned();
        let mut transfer = None;
        for _ in 0..32 {
            let Some(origin) = self.workers.get(&scope, &session).map(|e| &e.origin) else {
                break;
            };
            let Some(parent) = origin.handoff.as_deref() else {
                break;
            };
            if parent
                .host
                .as_ref()
                .is_some_and(|h| h.id != self.host_id.to_string())
            {
                if parent == source {
                    transfer = origin
                        .launch
                        .strip_prefix("handoff:")
                        .and_then(|id| uuid::Uuid::parse_str(id).ok());
                }
                break;
            }
            scope = parent.scope.clone();
            session.clone_from(&parent.session);
        }
        let Some(transfer) = transfer else {
            return self.answer(reply, Err(UNAVAILABLE.into()));
        };
        if let Some(proof) = broker.filter(|p| p.len() == 64) {
            use sha2::{Digest, Sha256};
            let digest = format!("{:x}", Sha256::digest(proof.as_bytes()));
            if source.history_broker.as_deref() == Some(&digest) {
                // The same desktop owns the earlier handoff. It can continue the read
                // on its current worker without waiting for another relay worker.
                return self.answer(
                    reply,
                    Ok(json!({"next_transfer":transfer.to_string(),"ancestor":page.ancestor})),
                );
            }
        }
        let token = nonce()?;
        let waiting: HashSet<_> = self
            .relays
            .desktops
            .iter()
            .filter(|(id, binding)| {
                source.history_broker.as_ref() == Some(binding) && self.clients.contains_key(id)
            })
            .map(|(id, _)| *id)
            .collect();
        if waiting.is_empty() {
            return self.answer(reply, Err(UNAVAILABLE.into()));
        }
        let expected = page.ancestor.clone().unwrap_or_else(|| wire::HistoryRef {
            host: source.host.as_ref().expect("foreign source").id.clone(),
            session: source.session.clone(),
        });
        let request = Server::RelayHandoffRead {
            token: token.clone(),
            transfer: transfer.to_string(),
            target_host: self.host_id.to_string(),
            page,
        };
        for &client in &waiting {
            self.reply(client, request.clone())?;
        }
        self.relays.reads.insert(
            token,
            Pending {
                expected,
                waiting,
                reply,
                deadline: Instant::now() + Duration::from_secs(75),
                error: None,
            },
        );
        Ok(())
    }

    pub(in crate::engine) fn finish_history(
        &mut self,
        client: u64,
        token: &str,
        result: Result<Value, String>,
    ) -> io::Result<()> {
        let Some(pending) = self.relays.reads.get_mut(token) else {
            return Ok(());
        };
        if !pending.waiting.remove(&client) {
            return Ok(());
        }
        let result = result.and_then(|page| {
            if page["source"]["host"]["id"].as_str() == Some(&pending.expected.host)
                && page["source"]["session"].as_str() == Some(&pending.expected.session)
                && page["items"].is_array()
            {
                Ok(page)
            } else {
                Err("The returned history page belongs to a different conversation.".into())
            }
        });
        let result = match result {
            Ok(page) => Ok(page),
            Err(e) => {
                if !e.is_empty() {
                    pending.error = Some(e);
                }
                if !pending.waiting.is_empty() {
                    return Ok(());
                }
                Err(pending.error.take().unwrap_or_else(|| UNAVAILABLE.into()))
            }
        };
        let pending = self.relays.reads.remove(token).unwrap();
        self.answer(pending.reply, result)
    }

    pub(in crate::engine) fn drop_history_relay(&mut self, client: u64) -> io::Result<()> {
        if self.relays.desktops.remove(&client).is_some() {
            let tokens: Vec<_> = self
                .relays
                .reads
                .iter()
                .filter(|(_, p)| p.waiting.contains(&client))
                .map(|(id, _)| id.clone())
                .collect();
            for token in tokens {
                self.finish_history(client, &token, Err(String::new()))?;
            }
        }
        Ok(())
    }
    pub(super) fn expire_history(&mut self, now: Instant) -> io::Result<()> {
        let tokens: Vec<_> = self
            .relays
            .reads
            .iter()
            .filter(|(_, r)| r.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for token in tokens {
            let pending = self.relays.reads.remove(&token).unwrap();
            self.answer(
                pending.reply,
                Err(pending.error.unwrap_or_else(|| UNAVAILABLE.into())),
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::engine) fn serve_history(
        &mut self,
        reply: Replier,
        source_host: &str,
        target_host: &str,
        scope: &Scope,
        transfer: &str,
        page: wire::HandoffPage,
        broker: Option<&str>,
    ) -> io::Result<()> {
        if let Replier::Client(client, _) = &reply
            && !self
                .clients
                .get(client)
                .is_some_and(|c| c.surface == Some(ox_desk_core::surface::Surface::Desktop))
        {
            return self.answer(
                reply,
                Err("Only the paired desktop can relay a handoff read.".into()),
            );
        }
        let grant = self
            .export_path(transfer)
            .and_then(std::fs::File::open)
            .and_then(|f| serde_json::from_reader::<_, Export>(f).map_err(io::Error::from));
        let source = match grant {
            Ok(export)
                if source_host == self.host_id.to_string()
                    && &export.source.scope == scope
                    && export.recipient == target_host =>
            {
                export.source
            }
            _ => {
                return self.answer(
                    reply,
                    Err("This source has no matching handoff to the selected host.".into()),
                );
            }
        };
        if let Some(ancestor) = page.ancestor.clone() {
            if !source.selection.earlier.iter().any(|a| {
                a.session == ancestor.session
                    && a.host.as_ref().is_some_and(|h| h.id == ancestor.host)
            }) {
                return self.answer(
                    reply,
                    Err("That earlier session was not part of this handoff.".into()),
                );
            }
            return self.read_history_of(
                &source.scope,
                &source.session,
                Some(&ancestor.host),
                Some(&ancestor.session),
                page,
                reply,
                Some(broker.unwrap_or("")),
            );
        }
        self.load_source_page(source, page, reply, true)
    }
}
