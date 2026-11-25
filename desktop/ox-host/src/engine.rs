use crate::{
    durable::{Ledger, SavedScope},
    receipts::{Receipts, digest},
    replay::Replay,
    runtime::{RuntimeConfig, canonical_scope},
    transport::{Peer, encode},
    workers::{Caller, Registry},
};
use ox_desk_core::{
    actions::Action,
    changed::Changed,
    host_protocol::{self as wire, Change, Client, Cursor, Scope, ScopeState, Server},
    inbox::Inbox,
    links::LinkEvent,
    origin::{Ceiling, Origin, ParentCall, WorkerMode, WorkerStart},
    runtime::RequestId,
    store::{Connection, Store},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::Path,
    sync::mpsc::{Receiver, RecvTimeoutError, SyncSender},
    thread,
    time::{Duration, Instant},
};

pub(crate) enum Message {
    Connect(u64, Peer),
    Disconnect(u64),
    Request(u64, Box<Client>),
    Runtime(Scope, u64),
    Schema(Scope, u64, Box<ox_desk_core::slash::Capabilities>),
    Discovered(u64, Box<Server>),
    Gateway {
        caller: Caller,
        request: Gateway,
        reply: Replier,
    },
    Stop,
}
pub(crate) enum Gateway {
    Launch(WorkerStart),
    Call(ParentCall),
}
/// Where a worker launch or parent call is answered.
pub(crate) enum Replier {
    /// A hosted gateway waiting on its private lifetime endpoint.
    Channel(SyncSender<Result<Value, String>>),
    /// A same-user gateway or operator client on the main socket.
    Client(u64, String),
}
#[derive(PartialEq)]
enum Stage {
    Opening,
    AccessRequested,
    Sent,
}
/// A worker launch accepted by policy, waiting for its runtime to create the session.
struct Launching {
    origin: Origin,
    start: WorkerStart,
    reply: Replier,
    stage: Stage,
    deadline: Instant,
}
struct ClientState {
    peer: Peer,
    hello: bool,
    prepared: Option<crate::runtime::Prepared>,
    subscription: Option<Option<Scope>>,
    catalog: bool,
}
struct Live {
    launching: Option<Launching>,
    fork_origin: Option<Origin>,
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
    workers: Registry,
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
        let mut workers = Registry::default();
        for saved in &ledger.data.scopes {
            for (id, origin, marks) in saved.origins() {
                workers.restore(&saved.scope, id, origin, marks);
            }
        }
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
            workers,
            receipts: Receipts::default(),
            discovered: Default::default(),
        })
    }
    pub(crate) fn serve(&mut self, rx: Receiver<Message>) -> io::Result<()> {
        loop {
            let deadline = self
                .running
                .values()
                .flat_map(|r| {
                    r.store
                        .next_deadline()
                        .into_iter()
                        .chain(r.launching.as_ref().map(|l| l.deadline))
                })
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
                        .filter(|(_, s)| {
                            s.store.next_deadline().is_some_and(|t| t <= now)
                                || s.launching.as_ref().is_some_and(|l| l.deadline <= now)
                        })
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
                Some(Message::Gateway {
                    caller,
                    request,
                    reply,
                }) => match request {
                    Gateway::Launch(start) => self.launch(caller, start, reply)?,
                    Gateway::Call(call) => {
                        let result = self.parent_call(&caller, call);
                        self.answer(reply, result)?;
                    }
                },
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
            Client::WorkerLaunch {
                id: key,
                mut caller,
                start,
            } => {
                let reply = Replier::Client(id, key);
                match caller.project.canonicalize() {
                    Ok(project) if key_ok(&reply) => {
                        caller.project = project;
                        self.launch(Caller::External(caller), start, reply)
                    }
                    _ => self.answer(
                        reply,
                        Err("Run the worker from a project directory.".into()),
                    ),
                }
            }
            Client::WorkerCall {
                id: key,
                mut caller,
                call,
            } => {
                let reply = Replier::Client(id, key);
                let result = caller
                    .project
                    .canonicalize()
                    .map_err(|e| e.to_string())
                    .and_then(|project| {
                        caller.project = project;
                        self.parent_call(&Caller::External(caller), call)
                    });
                self.answer(reply, result)
            }
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
        annotate(&self.workers, scope, &mut store);
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
                launching: None,
                fork_origin: None,
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
        if let Action::StartWorker(start) = action {
            let caller = Caller::Operator {
                scope: scope.clone(),
                parent: start.parent.clone(),
                device: format!("client-{id}"),
            };
            return self
                .launch(caller, start, Replier::Client(id, key.to_owned()))
                .map(|_| ());
        }
        if let Action::SetAccess(target, _) = &action
            && self.running.values().any(|l| {
                l.launching.as_ref().is_some_and(|w| {
                    w.origin.parent.scope.as_ref() == Some(scope)
                        && w.origin.parent.session.as_deref() == Some(target)
                })
            })
        {
            return Err(io::Error::other("A worker is starting on this parent."));
        }
        let target = match &action {
            Action::SetAccess(t, _)
            | Action::ForkThread(t)
            | Action::Stop(t)
            | Action::SendText(Some(t), _)
            | Action::SendImages(Some(t), _, _)
            | Action::Submit(Some(t))
            | Action::NativeCommand(Some(t), _)
            | Action::PaletteCommand(Some(t), _)
            | Action::ExecuteCommand(Some(t)) => Some(t.clone()),
            _ => None,
        };
        let origin = target
            .as_deref()
            .and_then(|t| self.workers.get(scope, t))
            .map(|e| e.origin.clone());
        let active_workers = target.as_deref().is_some_and(|t| {
            self.workers.children_of(scope, t).any(|(s, child)| {
                self.running.get(s).is_some_and(|l| {
                    l.store
                        .transcript(child)
                        .is_some_and(|t| t.active_turn().is_some())
                })
            })
        });
        let live = self.running.get_mut(scope).expect("validated live scope");
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
        crate::policy::check(&live.store, &action, origin.as_ref(), active_workers)?;
        let opens = matches!(
            action,
            Action::SendText(None, _)
                | Action::SendImages(None, _, _)
                | Action::Submit(None)
                | Action::ForkThread(_)
        );
        let opens = opens
            || matches!(&action,Action::PaletteCommand(None,text) | Action::NativeCommand(None,text) if ox_desk_core::slash::parse(text,&live.store.command_caps(None)).is_some_and(|c|c.starts_thread()));
        if opens && (live.opening.is_some() || live.launching.is_some()) {
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
        // The operator's own turns on a worker are reported to its parent agent.
        let running = target.as_deref().is_some_and(|t| {
            live.store
                .transcript(t)
                .is_some_and(|t| t.active_turn().is_some())
        });
        let operator_turn = matches!(
            action,
            Action::SendText(Some(_), _)
                | Action::SendImages(Some(_), _, _)
                | Action::Submit(Some(_))
        );
        let access = match &action {
            Action::SetAccess(_, mode) => Some(*mode),
            _ => None,
        };
        let fork = matches!(action, Action::ForkThread(_));
        live.store.dispatch(action)?;
        if fork && let (Some(origin), Some(source)) = (&origin, &target) {
            live.fork_origin = Some(origin.fork(source));
        }
        if let Some(t) = &target
            && let Some(entry) = self.workers.get_mut(scope, t)
        {
            if operator_turn && running {
                entry.marks.steered += 1;
            } else if operator_turn {
                entry.marks.continued += 1;
                entry.marks.cancelled = false;
            }
            if access.is_some() {
                entry.marks.access = access;
            }
        }
        let live = self.running.get_mut(scope).expect("validated live scope");
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
        let launch_failure = advance_launch(live);
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
        let opened = live.store.take_opened();
        // A new session from a worker launch or a worker fork receives its stamp
        // before it is first published, so no client sees it unstamped.
        let mut launched = None;
        if let Some(thread) = &opened {
            if live.opening.is_some()
                && let Some(origin) = live.fork_origin.take()
            {
                self.workers.stamp(scope, thread, origin);
            } else if live
                .launching
                .as_ref()
                .is_some_and(|l| l.stage == Stage::Sent)
            {
                let l = live.launching.take().expect("launching");
                let allowed = live
                    .store
                    .applied_access(Some(thread))
                    .is_none_or(|a| l.origin.ceiling.allows(l.origin.runtime, a));
                if !allowed {
                    // The runtime did not confirm a mode inside the ceiling. Never let it run.
                    let _ = live.store.stop(thread);
                    launched = Some((l.reply, Err("The worker runtime did not confirm a permission mode within the granted ceiling.".to_owned()), None));
                } else {
                    self.workers.stamp(scope, thread, l.origin);
                    launched = Some((l.reply, Ok(()), Some(thread.clone())));
                }
            }
        }
        let launch_failure = launch_failure.or_else(|| {
            let l = live.launching.as_ref()?;
            match live.store.state() {
                Connection::Closed(e) => Some(e.clone()),
                Connection::SignInRequired => {
                    Some("Sign in to this worker's billing route first.".into())
                }
                _ if l.deadline <= Instant::now() => {
                    Some("The worker runtime did not start a session in time.".into())
                }
                _ if l.stage == Stage::Sent && !live.store.opening_thread() => live
                    .store
                    .notice()
                    .map(str::to_owned)
                    .or(Some("The worker session could not be started.".into())),
                _ => None,
            }
        });
        if let Some(error) = launch_failure
            && let Some(l) = live.launching.take()
        {
            launched = Some((l.reply, Err(error), None));
        }
        annotate(&self.workers, scope, &mut live.store);
        dirty.merge(live.store.drain(vec![], Instant::now()));
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
        if live.opening.is_none() {
            live.fork_origin = None;
        }
        if changes.is_empty() {
            if let Some((id, frame)) = handoff {
                self.reply(id, frame)?;
            }
            if let Some((id, frame)) = created {
                self.reply(id, frame)?;
            }
            return self.finish_launch(scope, launched);
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
        self.finish_launch(scope, launched)
    }
    fn answer(&mut self, reply: Replier, result: Result<Value, String>) -> io::Result<()> {
        match reply {
            Replier::Channel(tx) => {
                let _ = tx.send(result);
                Ok(())
            }
            Replier::Client(id, key) => {
                let (result, error) = match result {
                    Ok(v) => (Some(v), None),
                    Err(e) => (None, Some(e)),
                };
                self.reply(
                    id,
                    Server::WorkerReply {
                        id: key,
                        result,
                        error,
                    },
                )
            }
        }
    }
    fn finish_launch(
        &mut self,
        scope: &Scope,
        launched: Option<(Replier, Result<(), String>, Option<String>)>,
    ) -> io::Result<()> {
        let Some((reply, result, thread)) = launched else {
            return Ok(());
        };
        let result = result.and_then(|()| {
            let thread = thread.expect("launched session");
            let job = self
                .workers
                .get(scope, &thread)
                .map(|e| e.origin.job_id)
                .ok_or("The worker stamp was not recorded.")?;
            self.job_status(job)
        });
        self.answer(reply, result)
    }
    /// Check policy, then hand the launch to the worker's own runtime. Nothing
    /// starts unless every check passes. The reply waits for the native session.
    fn launch(&mut self, caller: Caller, start: WorkerStart, reply: Replier) -> io::Result<()> {
        match self.accept_launch(&caller, start) {
            Ok((scope, launching)) => {
                let live = self.running.get_mut(&scope).expect("ensured worker scope");
                live.launching = Some(Launching { reply, ..launching });
                self.process(&scope, vec![], false)
            }
            Err(e) => self.answer(reply, Err(e)),
        }
    }
    fn accept_launch(
        &mut self,
        caller: &Caller,
        start: WorkerStart,
    ) -> Result<(Scope, Launching), String> {
        start.validate()?;
        let (project, parent, access, parent_origin, external) = match caller {
            Caller::Hosted { scope, hint, .. } => {
                let live = self
                    .running
                    .get(scope)
                    .ok_or("This OX helper's route is not open on the session host.")?;
                let parent = crate::policy::single_parent(&live.store, hint.as_deref())?;
                let access = live.store.applied_access(Some(&parent));
                let origin = self.workers.get(scope, &parent).map(|e| e.origin.clone());
                (scope.project.clone(), Some(parent), access, origin, false)
            }
            Caller::Operator { scope, parent, .. } => {
                let live = self
                    .running
                    .get(scope)
                    .ok_or("Open the parent session's route first.")?;
                if live.store.catalog_row(parent).is_none()
                    || live.store.transcript(parent).is_none()
                {
                    return Err("Open the parent session before starting a worker from it.".into());
                }
                let access = live.store.applied_access(Some(parent));
                let origin = self.workers.get(scope, parent).map(|e| e.origin.clone());
                (
                    scope.project.clone(),
                    Some(parent.clone()),
                    access,
                    origin,
                    false,
                )
            }
            Caller::External(c) => (c.project.clone(), None, None, None, true),
        };
        let ceiling = crate::workers::grant(
            access,
            parent_origin.as_ref(),
            external,
            start.runtime,
            Ceiling::of(start.write),
        )?;
        let scope = canonical_scope(Scope {
            project,
            route: start.route(),
        })
        .map_err(|e| e.to_string())?;
        self.ensure(&scope, None).map_err(|e| e.to_string())?;
        let live = &self.running[&scope];
        if live.opening.is_some() || live.launching.is_some() {
            return Err("Another session is opening on the worker's route. Retry shortly.".into());
        }
        let job_id = loop {
            let candidate = getrandom::u64().map_err(|e| e.to_string())? & ((1u64 << 52) - 1);
            if candidate > 0 && !self.workers.job_ids().any(|j| j == candidate) {
                break candidate;
            }
        };
        let origin = crate::workers::origin(caller, parent, &start, job_id, ceiling);
        Ok((
            scope,
            Launching {
                origin,
                start,
                reply: Replier::Channel(std::sync::mpsc::sync_channel(1).0),
                stage: Stage::Opening,
                deadline: Instant::now() + Duration::from_secs(60),
            },
        ))
    }
    fn job_status(&self, job_id: u64) -> Result<Value, String> {
        let (scope, id, entry) = self
            .workers
            .job(job_id)
            .ok_or("Unknown worker job on this session host.")?;
        let live = self.running.get(scope);
        let state = live.map(|l| &l.state).or_else(|| self.cold.get(scope));
        let view = state.and_then(|s| s.sessions.iter().find(|v| v.id == id));
        let timeline = state.and_then(|s| s.transcripts.get(id));
        let native = live
            .and_then(|l| l.store.terminal_session_id(id).ok())
            .unwrap_or_else(|| id.rsplit(':').next().unwrap_or(id));
        let ready = live.is_some_and(|l| l.store.state() == &Connection::Ready);
        Ok(crate::workers::status(
            entry,
            id,
            native,
            view,
            timeline,
            ready,
            self.operators(),
        ))
    }
    /// Resolve the caller to exactly one parent identity, or refuse. Every
    /// parent operation uses this one result.
    fn resolve(&self, caller: &Caller) -> Result<crate::workers::Resolved, String> {
        use crate::workers::Resolved;
        match caller {
            Caller::Hosted { scope, hint, .. } => {
                let live = self
                    .running
                    .get(scope)
                    .ok_or("This OX helper's route is not open on the session host.")?;
                let session = crate::policy::single_parent(&live.store, hint.as_deref())?;
                Ok(Resolved::Hosted {
                    scope: scope.clone(),
                    session,
                })
            }
            Caller::External(c) => Ok(Resolved::Terminal {
                project: c.project.clone(),
                launch: crate::workers::launch_digest("terminal", &c.launch),
                session: c
                    .session
                    .clone()
                    .filter(|s| crate::workers::valid_session(s)),
            }),
            Caller::Operator { .. } => Err("Operators act on worker sessions directly.".into()),
        }
    }
    /// Publish and save changed worker marks for a scope, live or cold.
    fn persist_marks(&mut self, scope: &Scope) -> io::Result<()> {
        if self.running.contains_key(scope) {
            return self.process(scope, vec![], true);
        }
        let Some(cold) = self.cold.get_mut(scope) else {
            return Ok(());
        };
        for row in &mut cold.sessions {
            if let Some(entry) = self.workers.get(scope, &row.id) {
                row.detail["worker_marks"] = json!(entry.marks);
            }
        }
        self.ledger.update(SavedScope::from_state(cold))
    }
    /// Desktop windows and phones subscribed to the host right now.
    fn operators(&self) -> crate::workers::Operators {
        crate::workers::Operators {
            connected: self
                .clients
                .values()
                .filter(|c| c.catalog || c.subscription.is_some())
                .count(),
        }
    }
    /// The parent agent's four tools. Every answer comes from host session state.
    fn parent_call(&mut self, caller: &Caller, call: ParentCall) -> Result<Value, String> {
        let me = self.resolve(caller)?;
        let owned = |e: &crate::workers::Entry| crate::workers::owns(&me, e);
        let job_id = match &call {
            ParentCall::Bind { session } => {
                let crate::workers::Resolved::Terminal { launch, .. } = &me else {
                    return Err("Only a terminal OX launch binds its session.".into());
                };
                if !crate::workers::valid_session(session) {
                    return Err("Invalid native session ID.".into());
                }
                // Write-once, and only on workers this exact launch started.
                let mut scopes = Vec::new();
                for (scope, entry) in self.workers.entries_mut() {
                    if &entry.origin.launch == launch
                        && entry.origin.parent.session.is_none()
                        && entry.marks.parent_session.is_none()
                    {
                        entry.marks.parent_session = Some(session.clone());
                        scopes.push(scope.clone());
                    }
                }
                let bound = scopes.len();
                scopes.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
                scopes.dedup();
                for scope in scopes {
                    self.persist_marks(&scope).map_err(|e| e.to_string())?;
                }
                return Ok(json!({ "bound": bound }));
            }
            ParentCall::List => {
                let mut jobs: Vec<u64> = self
                    .workers
                    .jobs()
                    .filter(|(_, _, e)| owned(e))
                    .map(|(_, _, e)| e.origin.job_id)
                    .collect();
                jobs.sort_unstable();
                let jobs = jobs
                    .into_iter()
                    .filter_map(|j| self.job_status(j).ok())
                    .map(|mut v| {
                        v.as_object_mut().map(|o| o.remove("messages"));
                        v
                    })
                    .collect::<Vec<_>>();
                return Ok(json!({ "jobs": jobs }));
            }
            ParentCall::Status { job_id }
            | ParentCall::Steer { job_id, .. }
            | ParentCall::Cancel { job_id } => *job_id,
        };
        let (scope, id) = self
            .workers
            .job(job_id)
            .filter(|(_, _, e)| owned(e))
            .map(|(s, id, _)| (s.clone(), id.to_owned()))
            .ok_or("Unknown worker job for this parent session.")?;
        match call {
            ParentCall::List | ParentCall::Bind { .. } | ParentCall::Status { .. } => {
                self.job_status(job_id)
            }
            ParentCall::Steer { text, .. } => {
                let text = text.trim();
                if text.is_empty() || text.len() > 24 * 1024 {
                    return Err("Send a nonempty worker message of at most 24 KiB.".into());
                }
                let status = self.job_status(job_id)?;
                let live = self
                    .running
                    .get_mut(&scope)
                    .ok_or("This worker is not accepting input.")?;
                // Text from the parent never answers a request that waits for the operator.
                if live
                    .store
                    .asks()
                    .iter()
                    .any(|a| a.thread.as_deref() == Some(&id))
                {
                    return Err(
                        "The worker is waiting for the operator to answer a request.".into(),
                    );
                }
                if status["can_steer"] != true {
                    return Err("This worker is not accepting input.".into());
                }
                live.store
                    .dispatch(Action::SendText(Some(id), text.to_owned()))
                    .map_err(|e| e.to_string())?;
                self.process(&scope, vec![], true)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"accepted": true}))
            }
            ParentCall::Cancel { .. } => {
                let status = self.job_status(job_id)?;
                if matches!(
                    status["status"].as_str(),
                    Some("running" | "waiting_for_operator")
                ) {
                    if let Some(live) = self.running.get_mut(&scope) {
                        let _ = live.store.stop(&id);
                    }
                    if let Some(entry) = self.workers.get_mut(&scope, &id) {
                        entry.marks.cancelled = true;
                    }
                    if self.running.contains_key(&scope) {
                        self.process(&scope, vec![], true)
                            .map_err(|e| e.to_string())?;
                    }
                }
                self.job_status(job_id)
            }
        }
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
        "origin",
        "worker_marks",
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

/// Move a policy-approved launch forward once its runtime can accept it.
/// Returns an error to report to the launcher.
fn advance_launch(live: &mut Live) -> Option<String> {
    let l = live.launching.as_mut()?;
    if live.store.state() != &Connection::Ready {
        return None;
    }
    let preset = l.origin.requested.preset(l.origin.runtime);
    match l.stage {
        Stage::Opening if live.store.can_send(None) => {
            l.stage = Stage::AccessRequested;
            if let Err(e) = live.store.dispatch(Action::SetNextAccess(preset)) {
                return Some(e.to_string());
            }
            advance_launch(live)
        }
        Stage::AccessRequested
            if l.start.mode == WorkerMode::Goal
                && (!live.store.has_command_schema() || live.store.commands_loading()) =>
        {
            None
        }
        Stage::AccessRequested
            if l.start.mode == WorkerMode::Goal
                && !live.store.command_caps(None).supports_goal() =>
        {
            Some("Codex Goal mode is not enabled on the worker's route.".into())
        }
        Stage::AccessRequested
            if live.store.can_send(None)
                && (live.store.next_access() == Some(preset)
                    || live.store.applied_access(None) == Some(preset)) =>
        {
            l.stage = Stage::Sent;
            let prompt = l.start.prompt();
            let action = if l.start.mode == WorkerMode::Goal {
                Action::NativeCommand(None, prompt)
            } else {
                Action::SendText(None, prompt)
            };
            live.store.dispatch(action).err().map(|e| e.to_string())
        }
        _ => None,
    }
}

/// Publish each session's stamp with its row. A hosted parent nests the worker
/// across scopes through `origin.parent.scope`.
fn annotate(workers: &Registry, scope: &Scope, store: &mut Store) {
    for (id, entry) in workers.in_scope(scope) {
        let o = &entry.origin;
        store.annotate_session(
            id,
            o.parent.scope.as_ref().and(o.parent.session.clone()),
            json!({"origin": o, "worker_marks": entry.marks, "worker": true}),
        );
    }
}
fn key_ok(reply: &Replier) -> bool {
    match reply {
        Replier::Client(_, key) => !key.is_empty() && key.len() <= 256,
        Replier::Channel(_) => true,
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
                launching: None,
                fork_origin: None,
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

    mod workers {
        use super::*;
        use ox_desk_core::{
            host_protocol::{AskView, Timeline},
            origin::{ExternalCaller, ParentRef, Relation},
            runtime::{Access, Runtime},
            store::ThreadRow,
        };
        use std::sync::{Arc, Mutex};

        type Log = Arc<Mutex<Vec<(Action, Option<String>)>>>;

        fn config(root: &Path) -> RuntimeConfig {
            RuntimeConfig {
                ox: "/bin/false".into(),
                path: "/bin".into(),
                desktop: root.into(),
            }
        }
        fn view(scope: &Scope, id: &str, status: &str, access: &str) -> wire::SessionView {
            let row = ThreadRow {
                worker: false,
                parent_thread_id: None,
                agent_nickname: None,
                id: id.into(),
                cwd: scope.project.clone(),
                model_provider: "openai".into(),
                model: None,
                name: Some(id.into()),
                preview: String::new(),
                generated_title: None,
                updated_at: 1,
                status: None,
                unread: false,
                activity_order: 0,
            };
            wire::SessionView {
                id: id.into(),
                title: id.into(),
                parent: None,
                status: status.into(),
                updated_at: 1,
                capabilities: ["open", "send", "steer", "stop", "access"]
                    .map(String::from)
                    .into(),
                detail: json!({"row": row, "access": access, "owned": true}),
            }
        }
        /// A runtime stand-in: a host-client replica whose actions are recorded.
        fn live(
            scope: &Scope,
            sessions: Vec<wire::SessionView>,
            active: &[&str],
            asks: Vec<AskView>,
        ) -> (Live, Log) {
            let log: Log = Default::default();
            let record = log.clone();
            let mut store = Store::remote_client(
                ox_desk_core::store::Profile {
                    project: scope.project.clone(),
                    route: scope.route.clone(),
                    provider: "openai".into(),
                },
                move |action, approval| {
                    record.lock().unwrap().push((action, approval));
                    Ok("sent".into())
                },
            );
            let transcripts = active
                .iter()
                .map(|id| {
                    (
                        id.to_string(),
                        Timeline {
                            items: vec![],
                            detail: json!({"active": "t1", "ready": true}),
                        },
                    )
                })
                .collect();
            store.apply(LinkEvent::Host(Box::new(Server::Snapshot {
                cursor: Cursor {
                    epoch: "e".into(),
                    seq: 1,
                },
                scopes: vec![ScopeState {
                    scope: scope.clone(),
                    connection: "ready".into(),
                    capabilities: vec!["start".into()],
                    sessions,
                    asks,
                    transcripts,
                    detail: json!({}),
                }],
            })));
            let _ = store.drain(vec![], Instant::now());
            (
                Live {
                    state: store.host_snapshot(scope),
                    store,
                    launching: None,
                    fork_origin: None,
                    opening: None,
                    terminal: None,
                    schema_started: true,
                    recovered: vec![],
                    inbox: Inbox::new(|| {}),
                    generation: 1,
                    tokens: HashMap::new(),
                },
                log,
            )
        }
        fn origin(parent: ParentRef, ceiling: Ceiling) -> Origin {
            Origin {
                relation: Relation::Worker,
                parent,
                launch: "helper:test".into(),
                by_operator: false,
                runtime: Runtime::Codex,
                route: Route::codex_api("fixture".into()),
                mode: WorkerMode::Session,
                job_id: 41,
                requested: Ceiling::Read,
                ceiling,
                source: None,
            }
        }
        fn scopes(root: &Path) -> (Scope, Scope) {
            let parent = Scope {
                project: root.canonicalize().unwrap(),
                route: Route::CODEX_SUBSCRIPTION,
            };
            let worker = Scope {
                route: Route::codex_api("fixture".into()),
                ..parent.clone()
            };
            (parent, worker)
        }
        fn hosted(scope: &Scope) -> Caller {
            Caller::Hosted {
                scope: scope.clone(),
                hint: None,
                launch: "helper".into(),
            }
        }
        fn start(write: bool) -> WorkerStart {
            WorkerStart {
                parent: String::new(),
                runtime: Runtime::Codex,
                model: Some("fixture".into()),
                mode: WorkerMode::Session,
                task: "check".into(),
                write,
            }
        }

        #[test]
        fn ambiguous_parents_are_refused_before_any_runtime_starts() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let rows = vec![
                view(&parent, "a", "running", "workspace"),
                view(&parent, "b", "running", "workspace"),
            ];
            engine
                .running
                .insert(parent.clone(), live(&parent, rows, &["a", "b"], vec![]).0);
            let refused = engine.accept_launch(&hosted(&parent), start(false));
            assert!(refused.err().unwrap().contains("Several sessions"));
            assert!(
                !engine.running.contains_key(&worker),
                "no worker runtime was opened"
            );
            assert!(
                engine
                    .parent_call(&hosted(&parent), ParentCall::List)
                    .unwrap_err()
                    .contains("Several sessions")
            );
            // Without the calling thread, every parent operation is refused, not
            // only listing: status, steer and cancel are never looser than list.
            let o = origin(workers_parent(&parent, "a"), Ceiling::Read);
            engine.workers.stamp(&worker, "w", o);
            for call in [
                ParentCall::Status { job_id: 41 },
                ParentCall::Steer {
                    job_id: 41,
                    text: "x".into(),
                },
                ParentCall::Cancel { job_id: 41 },
            ] {
                let err = engine.parent_call(&hosted(&parent), call).unwrap_err();
                assert!(err.contains("Several sessions"), "{err}");
            }
            // With Codex's `_meta.threadId`, the caller is exact. "b" is a sibling
            // worker on the same app-server: it sees none of a's jobs.
            let thread = |id: &str| Caller::Hosted {
                scope: parent.clone(),
                hint: Some(id.into()),
                launch: "helper".into(),
            };
            assert_eq!(
                engine.parent_call(&thread("a"), ParentCall::List).unwrap()["jobs"][0]["job_id"],
                41
            );
            assert_eq!(
                engine
                    .parent_call(&thread("a"), ParentCall::Status { job_id: 41 })
                    .unwrap()["job_id"],
                41
            );
            assert_eq!(
                engine.parent_call(&thread("b"), ParentCall::List).unwrap()["jobs"],
                json!([])
            );
            for call in [
                ParentCall::Status { job_id: 41 },
                ParentCall::Steer {
                    job_id: 41,
                    text: "x".into(),
                },
                ParentCall::Cancel { job_id: 41 },
            ] {
                assert!(engine.parent_call(&thread("b"), call).is_err());
            }
            assert!(!engine.workers.get(&worker, "w").unwrap().marks.cancelled);
            // A thread ID that is not an active session on this route is refused.
            assert!(
                engine
                    .parent_call(&thread("forged"), ParentCall::List)
                    .is_err()
            );
            // A hint from a session-scoped helper resolves the ambiguity, and the
            // parent's read-only mode still caps the request before anything starts.
            let mut rows = vec![
                view(&parent, "a", "running", "read_only"),
                view(&parent, "b", "running", "workspace"),
            ];
            rows.truncate(2);
            engine
                .running
                .insert(parent.clone(), live(&parent, rows, &["a", "b"], vec![]).0);
            let hinted = Caller::Hosted {
                scope: parent.clone(),
                hint: Some("a".into()),
                launch: "h".into(),
            };
            assert!(
                engine
                    .accept_launch(&hinted, start(true))
                    .err()
                    .unwrap()
                    .contains("ceiling")
            );
            assert!(!engine.running.contains_key(&worker));
        }

        fn workers_parent(scope: &Scope, id: &str) -> ParentRef {
            ParentRef {
                project: scope.project.clone(),
                scope: Some(scope.clone()),
                session: Some(id.into()),
                runtime: Some(Runtime::Codex),
            }
        }

        #[test]
        fn the_operator_cannot_raise_a_worker_above_its_ceiling() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let (l, log) = live(
                &worker,
                vec![view(&worker, "w", "idle", "read_only")],
                &[],
                vec![],
            );
            engine.running.insert(worker.clone(), l);
            engine.workers.stamp(
                &worker,
                "w",
                origin(workers_parent(&parent, "p"), Ceiling::Read),
            );
            let err = engine
                .act(
                    1,
                    "k1",
                    &worker,
                    Action::SetAccess("w".into(), Access::Workspace),
                    None,
                )
                .unwrap_err();
            assert!(err.to_string().contains("ceiling"), "{err}");
            assert!(
                engine
                    .act(
                        1,
                        "k2",
                        &worker,
                        Action::SetAccess("w".into(), Access::Yolo),
                        None
                    )
                    .is_err()
            );
            assert!(
                log.lock().unwrap().is_empty(),
                "nothing reached the runtime"
            );
            // Within the ceiling the operator's change goes through and is reported to the parent.
            engine.workers.get_mut(&worker, "w").unwrap().origin.ceiling = Ceiling::Write;
            engine
                .act(
                    1,
                    "k3",
                    &worker,
                    Action::SetAccess("w".into(), Access::Workspace),
                    None,
                )
                .unwrap();
            assert!(matches!(
                log.lock().unwrap()[0].0,
                Action::SetAccess(_, Access::Workspace)
            ));
            assert_eq!(
                engine.workers.get(&worker, "w").unwrap().marks.access,
                Some(Access::Workspace)
            );
        }

        #[test]
        fn worker_approvals_belong_to_operators_and_survive_a_revoked_device() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let ask = AskView {
                command: None,
                id: RequestId::Number(9),
                token: "native".into(),
                thread: Some("w".into()),
                turn: Some("t1".into()),
                item: None,
                choice_details: vec![None, None],
                title: "Run command".into(),
                detail: "cargo test".into(),
                choices: vec!["Allow".into(), "Decline".into()],
                questions: vec![],
            };
            let (l, log) = live(
                &worker,
                vec![view(&worker, "w", "needs_input", "workspace")],
                &["w"],
                vec![ask],
            );
            engine.running.insert(worker.clone(), l);
            let caller = Caller::External(ExternalCaller {
                project: worker.project.clone(),
                runtime: Runtime::Claude,
                session: Some("s1".into()),
                launch: "l1".into(),
            });
            let mut o = origin(workers_parent(&parent, "p"), Ceiling::Write);
            o.parent.scope = None;
            o.parent.session = Some("s1".into());
            engine.workers.stamp(&worker, "w", o);
            engine.process(&worker, vec![], true).unwrap();
            let token = engine.running[&worker].state.asks[0].token.clone();
            assert!(!token.is_empty());
            // The parent agent has no answer operation and cannot steer past the request.
            let status = engine
                .parent_call(&caller, ParentCall::Status { job_id: 41 })
                .unwrap();
            assert_eq!(
                (status["status"].as_str(), status["can_steer"].as_bool()),
                (Some("waiting_for_operator"), Some(false))
            );
            assert!(status["waiting_for_operator"]["message"].is_string());
            let steer = engine.parent_call(
                &caller,
                ParentCall::Steer {
                    job_id: 41,
                    text: "approve it".into(),
                },
            );
            assert!(steer.unwrap_err().contains("operator"));
            assert!(
                crate::workers::parent_call(json!({"op":"answer","job_id":41,"choice":0})).is_err()
            );
            // A phone and a desktop are subscribed. Revoking the phone closes its stream.
            for id in [1, 2] {
                let (peer, _) = crate::transport::test_peer();
                engine.clients.insert(
                    id,
                    ClientState {
                        peer,
                        hello: true,
                        prepared: None,
                        subscription: Some(Some(worker.clone())),
                        catalog: false,
                    },
                );
            }
            engine.clients.remove(&1);
            assert_eq!(
                engine.running[&worker].state.asks.len(),
                1,
                "the request stays pending"
            );
            assert_eq!(
                engine.running[&worker].state.asks[0].token, token,
                "and keeps its token"
            );
            assert!(log.lock().unwrap().is_empty());
            // A stale token is refused. The remaining operator answers with the live one.
            assert!(
                engine
                    .act(
                        2,
                        "a1",
                        &worker,
                        Action::AnswerAsk(RequestId::Number(9), 0),
                        Some("stale".into())
                    )
                    .is_err()
            );
            engine
                .act(
                    2,
                    "a2",
                    &worker,
                    Action::AnswerAsk(RequestId::Number(9), 0),
                    Some(token),
                )
                .unwrap();
            assert!(
                matches!(log.lock().unwrap()[0], (Action::AnswerAsk(RequestId::Number(9), 0), Some(ref t)) if t == "native")
            );
        }

        #[test]
        fn terminal_workers_belong_to_their_launch_until_a_session_is_bound() {
            let root = tempfile::tempdir().unwrap();
            let (parent, worker) = scopes(root.path());
            let terminal = |session: Option<&str>, launch: &str| {
                Caller::External(ExternalCaller {
                    project: worker.project.clone(),
                    runtime: Runtime::Claude,
                    session: session.map(Into::into),
                    launch: launch.into(),
                })
            };
            let mut bound = origin(workers_parent(&parent, "p"), Ceiling::Read);
            bound.parent.scope = None;
            bound.parent.session = None;
            bound.launch = crate::workers::launch_digest("terminal", "token-one");
            let mut orphan = bound.clone();
            orphan.job_id = 42;
            orphan.launch = crate::workers::launch_digest("terminal", "token-gone");
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                let (l, _) = live(
                    &worker,
                    vec![
                        view(&worker, "w1", "running", "read_only"),
                        view(&worker, "w2", "running", "read_only"),
                    ],
                    &["w1", "w2"],
                    vec![],
                );
                engine.running.insert(worker.clone(), l);
                engine.workers.stamp(&worker, "w1", bound.clone());
                engine.workers.stamp(&worker, "w2", orphan.clone());
                engine.process(&worker, vec![], true).unwrap();
                // A second `ox codex` in the same repository sees and controls nothing.
                let other = terminal(None, "token-two");
                assert_eq!(
                    engine.parent_call(&other, ParentCall::List).unwrap()["jobs"],
                    json!([])
                );
                assert!(
                    engine
                        .parent_call(&other, ParentCall::Cancel { job_id: 41 })
                        .is_err()
                );
                assert!(
                    engine
                        .parent_call(
                            &other,
                            ParentCall::Steer {
                                job_id: 41,
                                text: "x".into()
                            }
                        )
                        .is_err()
                );
                // Its own launch controls it and binds its session once observed.
                let own = terminal(None, "token-one");
                assert_eq!(
                    engine
                        .parent_call(&own, ParentCall::Status { job_id: 41 })
                        .unwrap()["job_id"],
                    41
                );
                let reply = engine
                    .parent_call(
                        &own,
                        ParentCall::Bind {
                            session: "s1".into(),
                        },
                    )
                    .unwrap();
                assert_eq!(reply["bound"], 1, "only this launch's workers are bound");
                // Binding is write-once: a later claim cannot move the job.
                engine
                    .parent_call(
                        &own,
                        ParentCall::Bind {
                            session: "s2".into(),
                        },
                    )
                    .unwrap();
                assert_eq!(
                    engine
                        .workers
                        .get(&worker, "w1")
                        .unwrap()
                        .marks
                        .parent_session
                        .as_deref(),
                    Some("s1")
                );
                // A hosted caller has no binding operation.
                let hinted = Caller::Hosted {
                    scope: worker.clone(),
                    hint: Some("w1".into()),
                    launch: "h".into(),
                };
                assert!(
                    engine
                        .parent_call(
                            &hinted,
                            ParentCall::Bind {
                                session: "s1".into()
                            }
                        )
                        .is_err()
                );
            }
            // After a host restart and the original launch's exit, a resume of s1
            // reads its worker. The worker whose launch never learned a session
            // is left to operators only.
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let resumed = terminal(Some("s1"), "token-three");
            assert_eq!(
                engine
                    .parent_call(&resumed, ParentCall::Status { job_id: 41 })
                    .unwrap()["job_id"],
                41
            );
            assert!(
                engine
                    .parent_call(&resumed, ParentCall::Status { job_id: 42 })
                    .is_err()
            );
            assert!(
                engine
                    .parent_call(
                        &terminal(Some("s2"), "token-four"),
                        ParentCall::Status { job_id: 41 }
                    )
                    .is_err()
            );
            assert!(
                engine
                    .parent_call(
                        &terminal(None, "token-four"),
                        ParentCall::Cancel { job_id: 42 }
                    )
                    .is_err()
            );
            assert_eq!(
                engine.parent_call(&resumed, ParentCall::List).unwrap()["jobs"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }

        #[test]
        fn a_resumed_claude_session_reclaims_the_workers_its_new_launch_started() {
            let root = tempfile::tempdir().unwrap();
            let (_, worker) = scopes(root.path());
            // `ox claude` added `--session-id` before the first turn, so the
            // launch names its session when it calls start_ox_worker.
            let session = "6f1c2d3e-4b5a-4c6d-8e7f-90a1b2c3d4e5";
            let terminal = |session: Option<&str>, launch: &str| {
                Caller::External(ExternalCaller {
                    project: worker.project.clone(),
                    runtime: Runtime::Claude,
                    session: session.map(Into::into),
                    launch: launch.into(),
                })
            };
            let launch = terminal(Some(session), "first-launch-token");
            let started = crate::workers::origin(
                &launch,
                None,
                &WorkerStart {
                    parent: String::new(),
                    runtime: Runtime::Codex,
                    model: Some("fixture".into()),
                    mode: WorkerMode::Session,
                    task: "fix".into(),
                    write: true,
                },
                41,
                Ceiling::Write,
            );
            assert_eq!(
                started.parent.session.as_deref(),
                Some(session),
                "recorded at launch"
            );
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                let (l, _) = live(
                    &worker,
                    vec![view(&worker, "w", "running", "workspace")],
                    &["w"],
                    vec![],
                );
                engine.running.insert(worker.clone(), l);
                engine.workers.stamp(&worker, "w", started.clone());
                engine.process(&worker, vec![], true).unwrap();
                assert_eq!(
                    engine
                        .parent_call(&launch, ParentCall::Status { job_id: 41 })
                        .unwrap()["job_id"],
                    41
                );
                // A later binding from the same launch, e.g. after /clear, changes nothing.
                engine
                    .parent_call(
                        &launch,
                        ParentCall::Bind {
                            session: "after-clear".into(),
                        },
                    )
                    .unwrap();
                assert_eq!(
                    engine
                        .workers
                        .get(&worker, "w")
                        .unwrap()
                        .marks
                        .parent_session,
                    None
                );
                assert_eq!(
                    engine
                        .workers
                        .get(&worker, "w")
                        .unwrap()
                        .origin
                        .parent
                        .session
                        .as_deref(),
                    Some(session)
                );
            }
            // The terminal exits and the host restarts.
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            // Another new `ox claude` in the same project has its own session ID.
            let other = terminal(
                Some("0a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d"),
                "second-launch-token",
            );
            assert_eq!(
                engine.parent_call(&other, ParentCall::List).unwrap()["jobs"],
                json!([])
            );
            for call in [
                ParentCall::Status { job_id: 41 },
                ParentCall::Steer {
                    job_id: 41,
                    text: "x".into(),
                },
                ParentCall::Cancel { job_id: 41 },
            ] {
                assert!(engine.parent_call(&other, call).is_err());
            }
            assert!(!engine.workers.get(&worker, "w").unwrap().marks.cancelled);
            // `ox claude --resume <that uuid>` is a new launch naming the same session.
            let resumed = terminal(Some(session), "resume-launch-token");
            assert_eq!(
                engine.parent_call(&resumed, ParentCall::List).unwrap()["jobs"][0]["job_id"],
                41
            );
            assert_eq!(
                engine
                    .parent_call(&resumed, ParentCall::Status { job_id: 41 })
                    .unwrap()["job_id"],
                41
            );
            let cancelled = engine
                .parent_call(&resumed, ParentCall::Cancel { job_id: 41 })
                .unwrap();
            assert_eq!(cancelled["job_id"], 41);
        }

        #[test]
        fn a_host_restart_mid_worker_keeps_its_stamp_and_never_relaunches_it() {
            let root = tempfile::tempdir().unwrap();
            let (parent, worker) = scopes(root.path());
            let mut o = origin(workers_parent(&parent, "p"), Ceiling::Write);
            o.parent.scope = None;
            o.parent.session = Some("s1".into());
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                let (l, _) = live(
                    &worker,
                    vec![view(&worker, "w", "running", "workspace")],
                    &["w"],
                    vec![],
                );
                engine.running.insert(worker.clone(), l);
                engine.workers.stamp(&worker, "w", o.clone());
                engine
                    .workers
                    .get_mut(&worker, "w")
                    .unwrap()
                    .marks
                    .continued = 1;
                engine.process(&worker, vec![], true).unwrap();
                let saved = std::fs::read_to_string(root.path().join("ownership-v1.json")).unwrap();
                assert!(saved.contains("\"job_id\":41") && !saved.contains("\"task\""));
            }
            let (tx, rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            assert!(engine.running.is_empty(), "nothing is relaunched");
            assert!(rx.try_recv().is_err());
            let entry = engine.workers.get(&worker, "w").unwrap();
            assert_eq!(entry.origin, o, "the stamp is unchanged");
            assert_eq!(entry.marks.continued, 1);
            let row = &engine.cold[&worker].sessions[0];
            assert_eq!(row.status, "interrupted");
            assert_eq!(row.detail["origin"]["job_id"], 41);
            assert_eq!(catalog_row(row).detail["origin"]["ceiling"], "write");
            let caller = Caller::External(ExternalCaller {
                project: worker.project.clone(),
                runtime: Runtime::Claude,
                session: Some("s1".into()),
                launch: "resumed".into(),
            });
            let status = engine
                .parent_call(&caller, ParentCall::Status { job_id: 41 })
                .unwrap();
            assert_eq!(status["status"], "interrupted");
            assert_eq!(status["operator"]["continued"], 1);
            let other = Caller::External(ExternalCaller {
                project: worker.project.clone(),
                runtime: Runtime::Claude,
                session: Some("s2".into()),
                launch: "other".into(),
            });
            assert!(
                engine
                    .parent_call(&other, ParentCall::Status { job_id: 41 })
                    .is_err()
            );
            assert_eq!(
                engine.parent_call(&caller, ParentCall::List).unwrap()["jobs"][0]["job_id"],
                41
            );
        }
    }
}
