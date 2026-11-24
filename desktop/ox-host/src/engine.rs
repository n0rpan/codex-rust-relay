use crate::{
    durable::{Ledger, SavedScope},
    receipts::{Receipts, digest},
    replay::Replay,
    runtime::{RuntimeConfig, canonical_scope},
    transport::{Peer, encode},
};
use ox_desk_core::{
    actions::Action,
    changed::Changed,
    host_protocol::{self as wire, Change, Client, Cursor, Scope, ScopeState, Server},
    inbox::Inbox,
    links::LinkEvent,
    runtime::RequestId,
    store::{Connection, Store},
};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::Path,
    sync::mpsc::{Receiver, RecvTimeoutError, SyncSender},
    thread,
    time::Instant,
};

pub(crate) enum Message {
    Connect(u64, Peer),
    Disconnect(u64),
    Request(u64, Box<Client>),
    Runtime(Scope, u64),
    Schema(Scope, u64, Box<ox_desk_core::slash::Capabilities>),
    Discovered(u64, Box<Server>),
    Worker {
        scope: Scope,
        parent: Option<String>,
        write: bool,
        claude: bool,
        reply: SyncSender<Option<(u64, String)>>,
    },
    RegisterJob {
        permit: u64,
        registration: Box<crate::jobs::Registration>,
        reply: SyncSender<bool>,
    },
    Release(u64),
    JobReply {
        scope: Scope,
        generation: u64,
        id: String,
        action: ox_desk_core::worker::WorkerAction,
        result: Result<ox_desk_core::codex::jobs::Response, String>,
    },
    Stop,
}
struct ClientState {
    peer: Peer,
    hello: bool,
    prepared: Option<crate::runtime::Prepared>,
    subscription: Option<Option<Scope>>,
    catalog: bool,
}
struct Live {
    gateways: crate::jobs::Gateways,
    opening: Option<(u64, String)>,
    terminal: Option<(u64, String)>,
    schema_started: bool,
    recovered: Vec<wire::SessionView>,
    store: Store,
    inbox: Inbox<LinkEvent>,
    generation: u64,
    state: ScopeState,
    tokens: HashMap<RequestId, String>,
}
pub(crate) struct Engine {
    config: RuntimeConfig,
    ledger: Ledger,
    replay: Replay,
    tx: SyncSender<Message>,
    clients: BTreeMap<u64, ClientState>,
    running: HashMap<Scope, Live>,
    cold: HashMap<Scope, ScopeState>,
    launched: u64,
    grants: HashMap<u64, (Scope, u64, String, bool, bool)>,
    next_grant: u64,
    receipts: Receipts,
    discovered: HashMap<(Scope, String), ox_desk_core::store::ThreadRow>,
}
fn nonce() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
impl Engine {
    pub(crate) fn open(
        config: RuntimeConfig,
        dir: &Path,
        tx: SyncSender<Message>,
    ) -> io::Result<Self> {
        let mut ledger = Ledger::open(dir)?;
        let seq = ledger.data.reserved;
        let cold = ledger
            .data
            .scopes
            .iter()
            .map(|saved| (saved.scope.clone(), saved.recover()))
            .collect();
        ledger.begin(seq)?;
        Ok(Self {
            config,
            ledger,
            replay: Replay::new(seq, nonce()?),
            tx,
            clients: Default::default(),
            running: Default::default(),
            cold,
            launched: 0,
            grants: HashMap::new(),
            next_grant: 0,
            receipts: Receipts::default(),
            discovered: Default::default(),
        })
    }
    pub(crate) fn serve(&mut self, rx: Receiver<Message>) -> io::Result<()> {
        loop {
            let deadline = self
                .running
                .values()
                .filter_map(|r| r.store.next_deadline())
                .min();
            let msg = if deadline.is_some_and(|at| at <= Instant::now()) {
                None
            } else if let Some(deadline) = deadline {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                Some(match rx.recv() {
                    Ok(m) => m,
                    Err(_) => break,
                })
            };
            match msg {
                None => {
                    let now = Instant::now();
                    let due: Vec<_> = self
                        .running
                        .iter()
                        .filter(|(_, s)| s.store.next_deadline().is_some_and(|t| t <= now))
                        .map(|(k, _)| k.clone())
                        .collect();
                    for scope in due {
                        self.process(&scope, vec![], false)?;
                    }
                }
                Some(Message::Stop) => break,
                Some(Message::Schema(scope, generation, caps)) => {
                    if let Some(live) = self.running.get_mut(&scope)
                        && live.generation == generation
                        && live.store.state() == &Connection::Ready
                    {
                        let _ = live.store.install_command_schema(*caps);
                        self.process(&scope, vec![], true)?;
                    }
                }
                Some(Message::Worker {
                    scope,
                    parent,
                    write,
                    claude,
                    reply,
                }) => {
                    let approved = self.running.get(&scope).and_then(|live| {
                        if let Some(id) = &parent
                            && live.store.job(id).is_some()
                            && !live.gateways.permits_nested(id, write, claude)
                        {
                            return None;
                        }
                        let parent =
                            crate::policy::owner(&live.store, parent.as_deref(), write, claude)?;
                        self.next_grant += 1;
                        let permit = self.next_grant;
                        self.grants.insert(
                            permit,
                            (
                                scope.clone(),
                                live.generation,
                                parent.clone(),
                                write,
                                claude,
                            ),
                        );
                        Some((permit, parent))
                    });
                    if let Err(mpsc_error) = reply.send(approved)
                        && let Some((permit, _)) = mpsc_error.0
                    {
                        self.grants.remove(&permit);
                    }
                }
                Some(Message::Release(permit)) => {
                    self.grants.remove(&permit);
                }
                Some(Message::RegisterJob {
                    permit,
                    registration,
                    reply,
                }) => {
                    let ok = self.grants.remove(&permit).and_then(
                        |(scope, generation, parent, write, claude)| {
                            let live = self.running.get_mut(&scope)?;
                            if live.generation != generation {
                                return None;
                            }
                            live.gateways
                                .attach(
                                    format!("ox:{parent}:{}", registration.snapshot.job_id),
                                    &registration,
                                    write,
                                    claude,
                                )
                                .ok()?;
                            live.store.host_started_job(&parent, registration.snapshot);
                            Some(scope)
                        },
                    );
                    let _ = reply.send(ok.is_some());
                    if let Some(scope) = ok {
                        self.process(&scope, vec![], true)?;
                    }
                }
                Some(Message::JobReply {
                    scope,
                    generation,
                    id,
                    action,
                    result,
                }) => {
                    if let Some(live) = self.running.get_mut(&scope)
                        && live.generation == generation
                    {
                        live.store.host_job_reply(&id, action, result);
                        self.process(&scope, vec![], true)?;
                    }
                }
                Some(Message::Connect(id, peer)) => {
                    self.clients.insert(
                        id,
                        ClientState {
                            peer,
                            hello: false,
                            prepared: None,
                            subscription: None,
                            catalog: false,
                        },
                    );
                }
                Some(Message::Disconnect(id)) => {
                    self.clients.remove(&id);
                }
                Some(Message::Request(id, request)) => {
                    if let Err(e) = self.request(id, *request) {
                        if self.ledger.failed() {
                            return Err(e);
                        }
                        self.reply(
                            id,
                            Server::Error {
                                id: None,
                                message: e.to_string(),
                            },
                        )?;
                    }
                }
                Some(Message::Runtime(scope, gen_id)) => {
                    if let Some(s) = self.running.get(&scope)
                        && s.generation == gen_id
                    {
                        let batch = s.inbox.take();
                        self.process(&scope, batch, false)?;
                    }
                }
                Some(Message::Discovered(id, frame)) => {
                    if let Server::Discovered { scope, rows, .. } = frame.as_ref() {
                        for entry in rows {
                            if let Ok(mut row) =
                                serde_json::from_value::<ox_desk_core::store::ThreadRow>(
                                    entry.detail["row"].clone(),
                                )
                            {
                                row.worker = entry.detail["worker"].as_bool().unwrap_or(false);
                                self.discovered
                                    .insert((scope.clone(), entry.id.clone()), row);
                            }
                        }
                    }
                    self.reply(id, *frame)?;
                }
            }
        }
        self.running.clear();
        Ok(())
    }
    fn reply(&mut self, id: u64, frame: Server) -> io::Result<()> {
        if let Some(client) = self.clients.get(&id)
            && !client.peer.send(encode(&frame)?)
        {
            self.clients.remove(&id);
        }
        Ok(())
    }
    fn request(&mut self, id: u64, message: Client) -> io::Result<()> {
        let Some(client) = self.clients.get(&id) else {
            return Ok(());
        };
        if !client.hello && !matches!(message, Client::Hello { .. }) {
            return Err(io::Error::other("Send a protocol hello first"));
        }
        match message {
            Client::Hello { version } => {
                if version != wire::VERSION {
                    return Err(io::Error::other("Unsupported OX host protocol version"));
                }
                let prepared = self.config.prepare().ok();
                let routes = prepared
                    .as_ref()
                    .map(|s| s.routes.clone())
                    .unwrap_or_default();
                let client = self.clients.get_mut(&id).unwrap();
                client.hello = true;
                client.prepared = prepared;
                let mut projects = self.config.projects();
                projects.extend(
                    self.running
                        .keys()
                        .chain(self.cold.keys())
                        .map(|s| s.project.clone()),
                );
                projects.sort();
                projects.dedup();
                self.reply(
                    id,
                    Server::Hello {
                        version,
                        capabilities: [
                            "snapshot",
                            "resume",
                            "actions",
                            "approvals",
                            "workers",
                            "live_catalog",
                        ]
                        .map(str::to_owned)
                        .into(),
                        routes,
                        projects,
                    },
                )
            }
            Client::TerminalPreview {
                id: key,
                scope,
                thread,
            } => {
                let scope = canonical_scope(scope)?;
                let result=self.running.get(&scope)
                    .filter(|_|client.subscription.as_ref()==Some(&Some(scope.clone())) && !key.is_empty() && key.len()<=256)
                    .filter(|l|l.store.can_send(Some(&thread)) && !l.store.in_terminal(&thread)
                        && l.store.transcript(&thread).is_some_and(|t|t.active_turn().is_none())
                        && !l.store.asks().iter().any(|a|a.thread.as_deref()==Some(&thread))
                        && l.store.catalog_row(&thread).is_some_and(|r|!r.worker && r.parent_thread_id.is_none()))
                    .ok_or_else(||io::Error::other("Open an idle parent session in its verified project and billing route."))
                    .and_then(|l|l.store.handoff_argv(Some(&thread)));
                match result {
                    Ok(argv) => self.reply(
                        id,
                        Server::TerminalPreview {
                            id: key,
                            scope,
                            thread,
                            argv,
                        },
                    ),
                    Err(e) => self.reply(
                        id,
                        Server::Error {
                            id: Some(key),
                            message: e.to_string(),
                        },
                    ),
                }
            }
            Client::WatchCatalog => {
                let client = self.clients.get_mut(&id).expect("connected");
                client.catalog = true;
                client.subscription = None;
                let scopes = self
                    .running
                    .values()
                    .map(|live| &live.state)
                    .chain(self.cold.values())
                    .map(catalog_snapshot)
                    .collect();
                self.reply(
                    id,
                    Server::Snapshot {
                        cursor: self.replay.cursor.clone(),
                        scopes,
                    },
                )
            }
            Client::Subscribe { scope, after } => self.subscribe(id, scope, after),
            Client::Act {
                id: key,
                scope,
                action,
                approval,
            } => {
                if key.is_empty() || key.len() > 256 {
                    return Err(io::Error::other("Invalid action request ID"));
                }
                let fail = |message: String| Server::Error {
                    id: Some(key.clone()),
                    message,
                };
                let scope = match canonical_scope(scope) {
                    Ok(s) => s,
                    Err(e) => return self.reply(id, fail(e.to_string())),
                };
                let subscribed = self.clients[&id].subscription.as_ref();
                if !matches!(subscribed, Some(None))
                    && !matches!(subscribed,Some(Some(s)) if s==&scope)
                {
                    return self.reply(
                        id,
                        fail("Subscribe to the target scope before sending actions".into()),
                    );
                }
                let Some(generation) = self.running.get(&scope).map(|live| live.generation) else {
                    return self.reply(
                        id,
                        fail("Open this project and billing route before sending actions".into()),
                    );
                };
                let hash = digest(&action, &approval)?;
                match self
                    .receipts
                    .get(&scope, &key, generation, hash)
                    .map(|entry| entry.cloned())
                {
                    Err(e) => return self.reply(id, fail(e.to_string())),
                    Ok(Some(receipt)) => {
                        if let Some(thread) = receipt.opened {
                            self.reply(
                                id,
                                Server::Opened {
                                    id: key.clone(),
                                    scope: scope.clone(),
                                    thread,
                                },
                            )?;
                        } else if let Some((client, opening)) =
                            &mut self.running.get_mut(&scope).unwrap().opening
                            && opening == &key
                            && !self.clients.contains_key(client)
                        {
                            *client = id;
                        }
                        return self.reply(id, receipt.result);
                    }
                    Ok(None) => (),
                }
                let result = match self.act(id, &key, &scope, action, approval) {
                    Ok(()) => Server::Ack { id: key.clone() },
                    Err(e) => fail(e.to_string()),
                };
                let accepted = matches!(result, Server::Ack { .. });
                self.receipts
                    .insert(scope.clone(), key, generation, hash, result.clone());
                if accepted {
                    self.process(&scope, vec![], true)?;
                }
                self.reply(id, result)
            }
            Client::Discover { id: key } => {
                let cfg = self.config.clone();
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let errors = ox_desk_core::native_catalog::discover(
                        &cfg.ox,
                        &cfg.path,
                        &cfg.desktop.join("claude-routes.json"),
                        &cfg.desktop.join("titles"),
                        |scope, rows| {
                            tx.send(Message::Discovered(
                                id,
                                Box::new(Server::Discovered {
                                    id: key.clone(),
                                    scope,
                                    rows,
                                }),
                            ))
                            .is_ok()
                        },
                    );
                    let _ = tx.send(Message::Discovered(
                        id,
                        Box::new(Server::DiscoveredEnd { id: key, errors }),
                    ));
                });
                Ok(())
            }
        }
    }
    fn subscribe(
        &mut self,
        id: u64,
        scope: Option<Scope>,
        after: Option<Cursor>,
    ) -> io::Result<()> {
        let scope = scope.map(canonical_scope).transpose()?;
        let client = self.clients.get_mut(&id).expect("connected client");
        let old = client.subscription.take();
        let same_subscription = old.as_ref().is_none_or(|previous| previous == &scope);
        let prepared = client.prepared.take();
        let fresh = if let Some(scope) = &scope {
            match self.ensure(scope, prepared) {
                Ok(fresh) => fresh,
                Err(e) => {
                    self.clients
                        .get_mut(&id)
                        .expect("connected client")
                        .subscription = old;
                    return Err(e);
                }
            }
        } else {
            false
        };
        let client = self.clients.get_mut(&id).expect("connected client");
        client.catalog = false;
        client.subscription = Some(scope.clone());
        if !fresh
            && same_subscription
            && let Some(cursor) = after
            && let Some(mut frames) = self.replay.after(scope.as_ref(), &cursor)
        {
            if !frames.all(|p| client.peer.send(p)) {
                self.clients.remove(&id);
            }
            return Ok(());
        }
        let scopes = self
            .running
            .iter()
            .map(|(s, l)| (s, &l.state))
            .chain(self.cold.iter())
            .filter(|(s, _)| scope.as_ref().is_none_or(|wanted| wanted == *s))
            .map(|(_, s)| s.clone())
            .collect();
        self.reply(
            id,
            Server::Snapshot {
                cursor: self.replay.cursor.clone(),
                scopes,
            },
        )
    }
    fn ensure(
        &mut self,
        scope: &Scope,
        prepared: Option<crate::runtime::Prepared>,
    ) -> io::Result<bool> {
        if self.running.get(scope).is_some_and(|live| {
            !matches!(
                live.store.state(),
                Connection::Closed(_) | Connection::SignInRequired
            )
        }) {
            return Ok(false);
        }
        self.launched += 1;
        let generation = self.launched;
        let notify = self.tx.clone();
        let src = scope.clone();
        let inbox = Inbox::new(move || {
            let event = Message::Runtime(src.clone(), generation);
            // Store operations may deliver a reply inline on the actor thread.
            // Never block that thread behind its own full mailbox.
            if let Err(std::sync::mpsc::TrySendError::Full(event)) = notify.try_send(event) {
                let tx = notify.clone();
                thread::spawn(move || {
                    let _ = tx.send(event);
                });
            }
        });
        let into = inbox.clone();
        let mut store = self
            .config
            .open(scope, prepared, move |event| into.push(event))?;
        let gateways = crate::jobs::Gateways::default();
        store.host_control(gateways.control(scope.clone(), generation, self.tx.clone()));
        let previous = self.running.get_mut(scope);
        let prior = previous
            .as_ref()
            .map(|live| SavedScope::from_state(&live.state).recover());
        let prior_transcripts = previous
            .as_ref()
            .map(|live| live.state.transcripts.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if let Some(previous) = previous {
            store.recover_from(&mut previous.store);
        }
        let mut state = store.host_snapshot(scope);
        let recovered = self
            .cold
            .remove(scope)
            .or(prior)
            .map(|old| old.sessions)
            .unwrap_or_default();
        restore(&mut state.sessions, &recovered);
        let mut changes = vec![
            Change::State {
                connection: state.connection.clone(),
                capabilities: state.capabilities.clone(),
                detail: state.detail.clone(),
            },
            Change::Sessions {
                sessions: state.sessions.clone(),
            },
            Change::Asks { asks: vec![] },
        ];
        // Other subscribers retain this scope across native reconnection. Reset
        // their old timelines, then publish the replacement runtime's baseline.
        for thread in prior_transcripts {
            if !state.transcripts.contains_key(&thread) {
                changes.push(Change::Retired { thread });
            }
        }
        changes.extend(
            state
                .transcripts
                .iter()
                .map(|(thread, timeline)| Change::Transcript {
                    thread: thread.clone(),
                    timeline: timeline.clone(),
                }),
        );
        let _ = store.drain(vec![], Instant::now());
        self.running.insert(
            scope.clone(),
            Live {
                gateways,
                opening: None,
                terminal: None,
                schema_started: false,
                recovered,
                store,
                inbox,
                generation,
                state,
                tokens: Default::default(),
            },
        );
        self.broadcast(scope, changes)?;
        Ok(true)
    }
    fn act(
        &mut self,
        id: u64,
        key: &str,
        scope: &Scope,
        action: Action,
        approval: Option<String>,
    ) -> io::Result<()> {
        let live = self.running.get_mut(scope).expect("validated live scope");
        if let Action::AnswerAsk(ask, _) | Action::AnswerQuestions(ask, _) = &action
            && (approval.is_none() || live.tokens.get(ask) != approval.as_ref())
        {
            return Err(io::Error::other(
                "Approval was already resolved or belongs to an earlier runtime",
            ));
        }
        if let Action::SetAccess(id, _) = &action
            && self
                .grants
                .values()
                .any(|(s, _, parent, _, _)| s == scope && parent == id)
        {
            return Err(io::Error::other("A worker is starting on this parent."));
        }
        let from_catalog = |key: &str| {
            self.discovered
                .get(&(scope.clone(), key.to_owned()))
                .filter(|r| r.cwd == scope.project)
                .cloned()
        };
        let action = match action {
            Action::SelectThread(key) if live.store.catalog_row(&key).is_none() => {
                Action::OpenCatalogRow(Box::new(from_catalog(&key).ok_or_else(|| {
                    io::Error::other("The thread is outside the project catalog")
                })?))
            }
            Action::OpenCatalogRow(row) => Action::OpenCatalogRow(Box::new(
                live.store
                    .catalog_row(&row.id)
                    .cloned()
                    .or_else(|| from_catalog(&row.id))
                    .ok_or_else(|| io::Error::other("The thread is outside the project catalog"))?,
            )),
            Action::ConfirmAliasAndResume(row) => {
                let saved = live
                    .store
                    .catalog_row(&row.id)
                    .cloned()
                    .or_else(|| {
                        self.discovered
                            .iter()
                            .find(|((s, id), r)| {
                                s.project == scope.project
                                    && s.route.runtime == scope.route.runtime
                                    && s.route.is_api()
                                    && id == &row.id
                                    && r.needs_claude_alias()
                            })
                            .map(|(_, r)| r.clone())
                    })
                    .ok_or_else(|| {
                        io::Error::other("The saved billing route cannot be verified")
                    })?;
                Action::ConfirmAliasAndResume(Box::new(saved))
            }
            action => action,
        };
        crate::policy::check(&live.store, &action)?;
        let opens = matches!(
            action,
            Action::SendText(None, _)
                | Action::SendImages(None, _, _)
                | Action::Submit(None)
                | Action::ForkThread(_)
        );
        let opens = opens
            || matches!(&action,Action::PaletteCommand(None,text) | Action::NativeCommand(None,text) if ox_desk_core::slash::parse(text,&live.store.command_caps(None)).is_some_and(|c|c.starts_thread()));
        if opens && live.opening.is_some() {
            return Err(io::Error::other(
                "Another session is opening on this route.",
            ));
        }
        let terminal = match &action {
            Action::Terminal(_) | Action::TerminalText(..) => true,
            Action::PaletteCommand(owner, text) => matches!(
                ox_desk_core::slash::parse(text, &live.store.command_caps(owner.as_deref())),
                Some(ox_desk_core::slash::Action::Terminal)
            ),
            Action::ExecuteCommand(owner) => matches!(
                ox_desk_core::slash::parse(
                    live.store.draft(owner.as_deref()),
                    &live.store.command_caps(owner.as_deref())
                ),
                Some(ox_desk_core::slash::Action::Terminal)
            ),
            _ => false,
        };
        if terminal && live.terminal.is_some() {
            return Err(io::Error::other(
                "Another device is opening the terminal on this route.",
            ));
        }
        live.store.dispatch(action)?;
        if terminal {
            live.terminal = Some((id, key.into()));
        }
        if opens {
            live.opening = Some((id, key.into()));
        }
        Ok(())
    }
    fn process(
        &mut self,
        scope: &Scope,
        events: Vec<LinkEvent>,
        from_action: bool,
    ) -> io::Result<()> {
        let live = self.running.get_mut(scope).expect("live scope");
        // A stock request ID may be cancelled and reused in one input batch.
        // Retire the old incarnation before considering the batch's resulting asks.
        for event in events {
            let previous = live.store.asks().len();
            live.store.apply(event);
            if !live.tokens.is_empty() && live.store.asks().len() < previous {
                live.tokens
                    .retain(|id, _| live.store.asks().iter().any(|a| &a.id == id));
            }
        }
        let mut dirty = live.store.drain(vec![], Instant::now());
        if from_action {
            dirty.mark(Changed::THREADS | Changed::ASK | Changed::NOTICE);
            if let Some(id) = live.store.selected() {
                dirty.transcript(id);
            }
        }
        if !live.schema_started
            && scope.route.runtime == ox_desk_core::runtime::Runtime::Codex
            && live.store.state() == &Connection::Ready
        {
            live.schema_started = true;
            let tx = self.tx.clone();
            let path = self.config.path.clone();
            let scope = scope.clone();
            let gen_id = live.generation;
            thread::spawn(move || {
                if let Ok(caps) = ox_desk_core::codex::installed::installed(&path) {
                    let _ = tx.send(Message::Schema(scope, gen_id, Box::new(caps)));
                }
            });
        }
        let handoff = live.store.take_handoff().and_then(|handoff| {
            live.terminal.take().map(|(id, key)| {
                (
                    id,
                    Server::Handoff {
                        argv: handoff.argv,
                        id: key,
                        scope: scope.clone(),
                        thread: handoff.thread,
                        command: handoff.command,
                    },
                )
            })
        });
        if live.terminal.is_some() && !live.store.opening_terminal() {
            live.terminal = None;
        }
        let mut changes = live.store.host_project(scope, &dirty, &mut live.state);
        if let Some(sessions) = changes.iter_mut().find_map(|c| {
            if let Change::Sessions { sessions } = c {
                Some(sessions)
            } else {
                None
            }
        }) {
            live.recovered.retain(|old| {
                !sessions
                    .iter()
                    .any(|r| r.id == old.id && r.detail["owned"] == true)
            });
            restore(sessions, &live.recovered);
            live.state.sessions.clone_from(sessions);
        }
        if live
            .state
            .asks
            .iter()
            .any(|a| !live.tokens.contains_key(&a.id))
            && !changes.iter().any(|c| matches!(c, Change::Asks { .. }))
        {
            changes.push(Change::Asks {
                asks: live.state.asks.clone(),
            });
        }
        let opened = live.store.take_opened();
        let created = opened.and_then(|thread| {
            live.opening.take().map(|(id, key)| {
                self.receipts.opened(scope, &key, &thread);
                (
                    id,
                    Server::Opened {
                        id: key,
                        scope: scope.clone(),
                        thread,
                    },
                )
            })
        });
        if live.opening.is_some() && !live.store.opening_thread() {
            live.opening = None;
        }
        if changes.is_empty() {
            if let Some((id, frame)) = handoff {
                self.reply(id, frame)?;
            }
            if let Some((id, frame)) = created {
                self.reply(id, frame)?;
            }
            return Ok(());
        }
        if changes.iter().any(|c| matches!(c, Change::Asks { .. })) {
            let mut tokens = HashMap::new();
            for ask in &mut live.state.asks {
                let token = match live.tokens.remove(&ask.id) {
                    Some(existing) => existing,
                    None => nonce()?,
                };
                ask.token.clone_from(&token);
                tokens.insert(ask.id.clone(), token);
            }
            live.tokens = tokens;
            for change in &mut changes {
                if let Change::Asks { asks } = change {
                    asks.clone_from(&live.state.asks);
                }
            }
        }
        if changes.iter().any(|c| {
            matches!(
                c,
                Change::Asks { .. }
                    | Change::Session { .. }
                    | Change::Sessions { .. }
                    | Change::Retired { .. }
            )
        }) {
            self.ledger.update(SavedScope::from_state(&live.state))?;
        }
        self.broadcast(scope, changes)?;
        if let Some((id, frame)) = handoff {
            self.reply(id, frame)?;
        }
        if let Some((id, frame)) = created {
            self.reply(id, frame)?;
        }
        Ok(())
    }
    fn broadcast(&mut self, scope: &Scope, changes: Vec<Change>) -> io::Result<()> {
        self.replay.cursor.seq = self.ledger.next_sequence(self.replay.cursor.seq)?;
        if self.clients.values().any(|c| c.catalog) {
            let mut catalog: Vec<_> = changes
                .iter()
                .filter_map(|change| match change {
                    Change::Session { session } if session.detail["owned"] == true => {
                        Some(Change::Session {
                            session: catalog_row(session),
                        })
                    }
                    Change::Sessions { sessions } => Some(Change::Sessions {
                        sessions: sessions
                            .iter()
                            .filter(|s| s.detail["owned"] == true)
                            .map(catalog_row)
                            .collect(),
                    }),
                    _ => None,
                })
                .collect();
            if changes
                .iter()
                .any(|c| matches!(c, Change::State { .. } | Change::Asks { .. }))
                && let Some(live) = self.running.get(scope)
            {
                catalog.push(Change::State {
                    detail: catalog_detail(&live.state),
                    connection: String::new(),
                    capabilities: vec![],
                });
            }
            if !catalog.is_empty() {
                let packet = encode(&Server::Delta {
                    cursor: self.replay.cursor.clone(),
                    scope: scope.clone(),
                    changes: catalog,
                })?;
                self.clients
                    .retain(|_, c| !c.catalog || c.peer.send(packet.clone()));
            }
        }
        let packet = encode(&Server::Delta {
            cursor: self.replay.cursor.clone(),
            scope: scope.clone(),
            changes,
        })?;
        self.replay.append(scope.clone(), packet.clone());
        self.clients.retain(|_, c| match c.subscription.as_ref() {
            Some(None) => c.peer.send(packet.clone()),
            Some(Some(s)) if s == scope => c.peer.send(packet.clone()),
            _ => true,
        });
        Ok(())
    }
}

// Sidebar subscribers need just the navigation metadata. Never copy a transcript,
// command schema, approval payload, or worker snapshot into this stream.
fn catalog_row(view: &wire::SessionView) -> wire::SessionView {
    let detail = [
        "row",
        "worker",
        "order",
        "generated",
        "unread",
        "owned",
        "route",
    ]
    .into_iter()
    .filter_map(|k| Some((k.to_owned(), view.detail.get(k)?.clone())))
    .collect::<serde_json::Map<_, _>>()
    .into();
    wire::SessionView {
        id: view.id.clone(),
        title: view.title.clone(),
        parent: view.parent.clone(),
        status: view.status.clone(),
        updated_at: view.updated_at,
        capabilities: view
            .capabilities
            .iter()
            .filter(|c| matches!(c.as_str(), "stop" | "steer"))
            .cloned()
            .collect(),
        detail,
    }
}
fn catalog_detail(state: &ScopeState) -> serde_json::Value {
    let requests: std::collections::BTreeSet<_> = state
        .asks
        .iter()
        .filter_map(|ask| ask.thread.as_deref())
        .collect();
    serde_json::json!({"archived":state.detail["archived"],"request_threads":requests})
}
fn catalog_snapshot(state: &ScopeState) -> ScopeState {
    ScopeState {
        scope: state.scope.clone(),
        connection: String::new(),
        capabilities: vec![],
        sessions: state
            .sessions
            .iter()
            .filter(|v| v.detail["owned"] == true)
            .map(catalog_row)
            .collect(),
        asks: vec![],
        transcripts: BTreeMap::new(),
        detail: catalog_detail(state),
    }
}

fn restore(rows: &mut Vec<wire::SessionView>, recovered: &[wire::SessionView]) {
    for saved in recovered {
        if let Some(row) = rows.iter_mut().find(|r| r.id == saved.id) {
            if row.detail["owned"] != true && saved.status == "interrupted" {
                row.status.clone_from(&saved.status);
            }
            row.detail["owned"] = true.into();
            row.detail["recovered"] = true.into();
        } else {
            rows.push(saved.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::{codex::frame::Frame, links::codex, runtime::Route, store::Profile};

    #[test]
    fn resolving_and_reusing_a_request_id_in_one_batch_retires_the_old_token() {
        let root = tempfile::tempdir().unwrap();
        let scope = Scope {
            project: root.path().to_owned(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let config = RuntimeConfig {
            ox: "/bin/false".into(),
            path: "/bin".into(),
            desktop: root.path().into(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config, root.path(), tx).unwrap();
        // Input is supplied below. The inert stdio endpoint only consumes the store's initialization message.
        let link =
            codex::Link::from_command(std::process::Command::new("/bin/cat"), |_| {}).unwrap();
        let store = Store::open(
            Profile {
                project: scope.project.clone(),
                route: scope.route.clone(),
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        engine.running.insert(
            scope.clone(),
            Live {
                state: store.host_snapshot(&scope),
                store,
                gateways: Default::default(),
                opening: None,
                terminal: None,
                schema_started: true,
                recovered: vec![],
                inbox: Inbox::new(|| {}),
                generation: 1,
                tokens: HashMap::new(),
            },
        );
        let read = |line: &[u8]| LinkEvent::Frame(Frame::decode(line, |_| None).unwrap());
        let ask = || {
            read(br#"{"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"a","turnId":"t","itemId":"i","command":"test"}}"#)
        };
        engine.process(&scope, vec![ask()], false).unwrap();
        let old = engine.running[&scope].state.asks[0].token.clone();
        engine
            .process(
                &scope,
                vec![
                    read(br#"{"method":"serverRequest/resolved","params":{"requestId":7}}"#),
                    ask(),
                ],
                false,
            )
            .unwrap();
        let new = &engine.running[&scope].state.asks[0].token;
        assert!(!new.is_empty());
        assert_ne!(new, &old);
        assert!(
            engine
                .act(
                    0,
                    "stale",
                    &scope,
                    Action::AnswerAsk(RequestId::Number(7), 0),
                    Some(old)
                )
                .is_err()
        );
    }
}
