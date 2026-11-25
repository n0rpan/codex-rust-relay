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
    handoff::HandoffStart,
    host_protocol::{self as wire, Change, Client, Cursor, ProcessView, Scope, ScopeState, Server},
    inbox::Inbox,
    links::LinkEvent,
    modes::YoloAllowance,
    origin::{Origin, ParentCall, WorkerMode, WorkerStart},
    runtime::RequestId,
    store::{Connection, Store},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
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
    /// Upgrade handover: refuse new turns, let running turns finish until the
    /// deadline, then stop. The next host reads why the previous one stopped.
    Drain(Duration),
}
/// A handoff target waiting for its first turn to be accepted.
pub(crate) struct Confirming {
    thread: String,
    deadline: Instant,
    launch: Launching,
}

/// Whether a new session's first turn was accepted: `Some(Ok)` once nothing is
/// left in flight, `Some(Err)` if the runtime refused part of it (such as
/// thread/inject_items) or it took too long, `None` while it is still being sent.
fn first_turn(store: &Store, thread: &str, deadline: Instant) -> Option<Result<(), String>> {
    if let Some(error) = store.transcript(thread).and_then(|t| t.error()) {
        return Some(Err(format!(
            "The new session was created, but its carried history or first turn was refused: {error}"
        )));
    }
    if !store.sending(Some(thread)) {
        return Some(Ok(()));
    }
    (deadline <= Instant::now()).then(|| {
        Err("The new session was created, but its carried history and first turn were not confirmed in time.".to_owned())
    })
}

/// A launch that ended, successfully or not.
pub(crate) struct Finished {
    reply: Replier,
    result: Result<(), String>,
    thread: Option<String>,
    origin: Origin,
    record: Option<String>,
    detached: bool,
}
impl Finished {
    fn new(l: Launching, result: Result<(), String>, thread: Option<String>) -> Self {
        Self {
            record: match &l.payload {
                Payload::Handoff { record, .. } => Some(record.clone()),
                Payload::Worker(_) => None,
            },
            reply: l.reply,
            result,
            thread,
            origin: l.origin,
            detached: l.detached,
        }
    }
}
pub(crate) enum Accepted {
    Launch(Scope, Launching),
    Ask(Asking),
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
/// What a launch sends once its runtime is ready.
pub(crate) enum Payload {
    Worker(WorkerStart),
    /// A handoff: the first turn (for Claude, with its context block) and the
    /// Codex items injected before it. `record` names the ledger entry.
    Handoff {
        text: String,
        items: Vec<Value>,
        record: String,
    },
}
/// A worker launch or handoff accepted by policy, waiting for its runtime to create the session.
pub(crate) struct Launching {
    origin: Origin,
    payload: Payload,
    reply: Replier,
    stage: Stage,
    deadline: Instant,
    /// The launcher already has its answer (a YOLO request the operator accepted).
    /// A failure is recorded as the job's outcome instead.
    detached: bool,
}
/// A child waiting for the operator to accept YOLO.
pub(crate) struct Asking {
    pub(crate) origin: Origin,
    pub(crate) start: WorkerStart,
    pub(crate) target: Scope,
    pub(crate) parent_title: String,
}
/// A handoff waiting for its source's older history to load.
struct Preparing {
    source: Scope,
    start: HandoffStart,
    reply: Replier,
    device: String,
    deadline: Instant,
    /// The target's context window, looked up off the engine thread. The outer
    /// None means the lookup is still running.
    window: std::sync::Arc<std::sync::Mutex<Option<Option<u64>>>>,
    window_deadline: Instant,
}
/// A read_ox_thread call waiting for its source transcript to load.
struct Reading {
    scope: Scope,
    session: String,
    source: Box<ox_desk_core::handoff::HandoffSource>,
    args: ReadArgs,
    reply: Replier,
    deadline: Instant,
    selected: bool,
}
#[derive(Clone, Debug)]
struct ReadArgs {
    item: Option<String>,
    after: Option<String>,
    limit: usize,
}
struct ClientState {
    peer: Peer,
    hello: bool,
    prepared: Option<crate::runtime::Prepared>,
    subscription: Option<Option<Scope>>,
    catalog: bool,
    /// A desktop window that may take a terminal handoff held for its scope.
    terminal: bool,
}
struct Live {
    launching: Option<Launching>,
    fork_origin: Option<Origin>,
    /// A Codex handoff target that exists but whose carried history and first
    /// turn are not confirmed yet: thread/inject_items and turn/start follow
    /// thread/start. Its launcher hears the outcome only once they do.
    confirming: Option<Confirming>,
    opening: Option<(u64, String)>,
    terminal: Option<(u64, String)>,
    schema_started: bool,
    recovered: Vec<wire::SessionView>,
    store: Store,
    inbox: Inbox<LinkEvent>,
    generation: u64,
    state: ScopeState,
    tokens: HashMap<RequestId, String>,
    /// When each live process started and was last used, for the process list and the idle reaper.
    tracker: Tracker,
    /// A terminal handoff whose requesting client was gone when it was ready. The
    /// next scoped subscriber (or a replayed request) receives it until the
    /// deadline, after which it is dropped so the runtime can be reaped.
    held_handoff: Option<(String, Server, Instant)>,
}

/// How long a terminal handoff waits for a client to take it before it is dropped.
const HELD_HANDOFF_TTL: Duration = Duration::from_secs(600);

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
/// Start and last-use times of one process (a Claude session or a Codex scope).
#[derive(Clone, Copy)]
struct Seen {
    started: i64,
    last: Instant,
    last_unix: i64,
}
impl Seen {
    fn now() -> Self {
        Self {
            started: now_unix(),
            last: Instant::now(),
            last_unix: now_unix(),
        }
    }
}
pub(crate) struct Tracker {
    scope: Seen,
    threads: BTreeMap<String, Seen>,
}
impl Tracker {
    pub(crate) fn new() -> Self {
        Self {
            scope: Seen::now(),
            threads: BTreeMap::new(),
        }
    }
    fn touch(&mut self, thread: &str) {
        let now = Seen::now();
        self.scope.last = now.last;
        self.scope.last_unix = now.last_unix;
        let seen = self.threads.entry(thread.to_owned()).or_insert(now);
        seen.last = now.last;
        seen.last_unix = now.last_unix;
    }
    /// Track exactly the threads that have a live process.
    fn sync(&mut self, live: &[String]) {
        self.threads.retain(|t, _| live.contains(t));
        for t in live {
            self.threads.entry(t.clone()).or_insert_with(Seen::now);
        }
    }
}

/// How long a Claude session or empty Codex scope may sit unused before the
/// host ends it. Zero disables the reaper. `OX_HOST_IDLE_MINUTES` overrides it.
pub(crate) const DEFAULT_IDLE_MINUTES: u64 = 30;
pub(crate) fn idle_timeout(setting: Option<&str>) -> Duration {
    let minutes = setting
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_IDLE_MINUTES);
    Duration::from_secs(minutes.saturating_mul(60))
}
/// Whether the reaper ends a process: it has sat unused for the whole timeout and
/// ending it would lose nothing (no running turn, pending approval, or worker).
pub(crate) fn reapable(idle: Duration, timeout: Duration, busy: bool) -> bool {
    !timeout.is_zero() && !busy && idle >= timeout
}
const REAP_EVERY: Duration = Duration::from_secs(60);

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
    /// Last project default read from the settings snapshot, kept for a failed read.
    yolo_projects: Vec<PathBuf>,
    /// Projects whose YOLO default was already announced since the host started.
    yolo_announced: HashSet<PathBuf>,
    /// Launch cautions by job number, recorded on the worker when it is stamped.
    launch_warnings: HashMap<u64, String>,
    discovered: HashMap<(Scope, String), ox_desk_core::store::ThreadRow>,
    asking: BTreeMap<u64, Asking>,
    preparing: Vec<Preparing>,
    reading: Vec<Reading>,
    in_flows: bool,
    draining: Option<Instant>,
    state_dir: PathBuf,
    idle: Duration,
    next_reap: Instant,
}
mod flows;
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
        ledger.retire(&crate::handover::take(dir))?;
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
            yolo_projects: Vec::new(),
            yolo_announced: HashSet::new(),
            launch_warnings: HashMap::new(),
            discovered: Default::default(),
            asking: Default::default(),
            preparing: Default::default(),
            reading: Default::default(),
            in_flows: false,
            draining: None,
            state_dir: dir.to_owned(),
            idle: idle_timeout(std::env::var("OX_HOST_IDLE_MINUTES").ok().as_deref()),
            next_reap: Instant::now() + REAP_EVERY,
        })
    }
    pub(crate) fn serve(&mut self, rx: Receiver<Message>) -> io::Result<()> {
        loop {
            if let Some(at) = self.draining {
                let running = self.running_turns();
                if running == 0 || Instant::now() >= at {
                    crate::handover::record(&self.state_dir, "upgrade", running)?;
                    break;
                }
            }
            let deadline = self
                .running
                .values()
                .flat_map(|r| {
                    r.store
                        .next_deadline()
                        .into_iter()
                        .chain(r.launching.as_ref().map(|l| l.deadline))
                        .chain(r.confirming.as_ref().map(|c| c.deadline))
                })
                .chain(self.preparing.iter().map(|p| p.deadline))
                .chain(
                    self.preparing
                        .iter()
                        .map(|p| p.window_deadline)
                        .filter(|at| *at > Instant::now()),
                )
                .chain(self.reading.iter().map(|r| r.deadline))
                .chain(self.draining)
                .chain(Some(self.next_reap))
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
                                || s.confirming.as_ref().is_some_and(|c| c.deadline <= now)
                        })
                        .map(|(k, _)| k.clone())
                        .collect();
                    for scope in due {
                        // An earlier pass in this loop may have ended the scope.
                        if self.running.contains_key(&scope) {
                            self.process(&scope, vec![], false)?;
                        }
                    }
                    if now >= self.next_reap {
                        self.next_reap = now + REAP_EVERY;
                        self.reap_idle(now)?;
                    }
                    self.advance_flows()?;
                }
                Some(Message::Stop) => {
                    crate::handover::record(&self.state_dir, "stop", self.running_turns())?;
                    break;
                }
                Some(Message::Drain(bound)) => {
                    let at = Instant::now()
                        .checked_add(bound)
                        .unwrap_or_else(|| Instant::now() + crate::control::MAX_BOUND);
                    self.draining = Some(self.draining.map_or(at, |d| d.min(at)));
                    eprintln!(
                        "OX host: draining for an update, {} running turn(s), at most {}s",
                        self.running_turns(),
                        bound.as_secs()
                    );
                }
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
                    Gateway::Launch(_) if self.draining.is_some() => {
                        self.answer(reply, Err(crate::handover::REFUSAL.into()))?
                    }
                    Gateway::Launch(start) => self.launch(caller, start, reply)?,
                    Gateway::Call(call @ ParentCall::ReadThread { .. }) => {
                        self.read_thread(&caller, call, reply)?
                    }
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
                            terminal: false,
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
                    let message = wire::version_mismatch(wire::VERSION, version);
                    let _ = self.reply(
                        id,
                        Server::Error {
                            id: None,
                            message: message.clone(),
                        },
                    );
                    return Err(io::Error::other(message));
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
                            "end",
                            "processes",
                        ]
                        .map(str::to_owned)
                        .into(),
                        routes,
                        projects,
                        host: Some(wire::HostInfo {
                            version: crate::handover::version().into(),
                            epoch: self.replay.cursor.epoch.clone(),
                        }),
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
            Client::ListProcesses { id: key } => {
                let processes = self.processes();
                self.reply(id, Server::Processes { id: key, processes })
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
            Client::Subscribe {
                scope,
                after,
                terminal,
            } => {
                if let Some(client) = self.clients.get_mut(&id) {
                    client.terminal = terminal;
                }
                self.subscribe(id, scope, after)
            }
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
                if matches!(call, ParentCall::ReadThread { .. }) {
                    return self.read_thread(&Caller::External(caller), call, reply);
                }
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
                // A scope the host dropped while idle reopens for a client still subscribed to it.
                if !self.running.contains_key(&scope)
                    && self.cold.contains_key(&scope)
                    && let Err(e) = self.ensure(&scope, self.config.prepare().ok())
                {
                    return self.reply(id, fail(e.to_string()));
                }
                let Some(generation) = self.running.get(&scope).map(|live| live.generation) else {
                    return self.reply(
                        id,
                        fail("Open this project and billing route before sending actions".into()),
                    );
                };
                if self.draining.is_some() && crate::handover::starts_work(&action) {
                    return self.reply(id, fail(crate::handover::REFUSAL.into()));
                }
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
                        } else {
                            let live = self.running.get_mut(&scope).unwrap();
                            if let Some((client, opening)) = &mut live.opening
                                && opening == &key
                                && !self.clients.contains_key(client)
                            {
                                *client = id;
                            }
                            // The same for a terminal handoff the old connection never received.
                            if let Some((client, terminal)) = &mut live.terminal
                                && terminal == &key
                                && !self.clients.contains_key(client)
                            {
                                *client = id;
                            }
                            if live
                                .held_handoff
                                .as_ref()
                                .is_some_and(|(k, _, _)| k == &key)
                                && let Some((_, frame, _)) = live.held_handoff.take()
                            {
                                self.reply(id, frame)?;
                            }
                        }
                        return self.reply(id, receipt.result);
                    }
                    Ok(None) => (),
                }
                let ends = matches!(action, Action::EndSession(_) | Action::ForceEndSession(_));
                let result = match self.act(id, &key, &scope, action, approval) {
                    Ok(()) => Server::Ack { id: key.clone() },
                    Err(e) => fail(e.to_string()),
                };
                let accepted = matches!(result, Server::Ack { .. });
                // Only accepted actions are receipts. A refusal is often transient, so a corrected
                // retry with the same key must run again rather than replay the refusal.
                if accepted {
                    self.receipts
                        .insert(scope.clone(), key, generation, hash, result.clone());
                }
                if accepted && self.running.contains_key(&scope) {
                    self.process(&scope, vec![], true)?;
                    if ends {
                        self.reap_empty(&scope)?;
                    }
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
        let mut replayed = None;
        if !fresh
            && same_subscription
            && let Some(cursor) = after
            && let Some(mut frames) = self.replay.after(scope.as_ref(), &cursor)
        {
            replayed = Some(frames.all(|p| client.peer.send_for(p, "replay")));
        }
        match replayed {
            Some(true) => return self.deliver_held_handoff(id, scope.as_ref()),
            Some(false) => {
                self.clients.remove(&id);
                return Ok(());
            }
            None => (),
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
        )?;
        self.deliver_held_handoff(id, scope.as_ref())
    }
    /// Give a terminal handoff whose requester disconnected to the next subscriber of its scope.
    fn deliver_held_handoff(&mut self, id: u64, scope: Option<&Scope>) -> io::Result<()> {
        let Some(scope) = scope else {
            return Ok(());
        };
        // A phone, push or operator gateway subscribed to the scope must never consume a
        // desktop's terminal handoff.
        if !self.clients.get(&id).is_some_and(|c| c.terminal) {
            return Ok(());
        }
        if let Some(frame) = self
            .running
            .get_mut(scope)
            .and_then(|l| l.held_handoff.take())
            .filter(|(_, _, deadline)| *deadline > Instant::now())
            .map(|(_, frame, _)| frame)
        {
            self.reply(id, frame)?;
        }
        Ok(())
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
        annotate(&self.workers, &self.ledger.data, scope, &mut store);
        let recovered = self
            .cold
            .remove(scope)
            .or(prior)
            .map(|old| old.sessions)
            .unwrap_or_default();
        // Threads the host had no process for stay resumable by a send.
        for saved in &recovered {
            store.mark_ended(&saved.id);
        }
        let mut state = store.host_snapshot(scope);
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
                confirming: None,
                opening: None,
                terminal: None,
                schema_started: false,
                recovered,
                store,
                inbox,
                generation,
                state,
                tokens: Default::default(),
                tracker: Tracker::new(),
                held_handoff: None,
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
        if let Action::AnswerAsk(ask, choice) = &action
            && let Some(job) = crate::yolo::job_of(ask)
        {
            return self.answer_yolo(id, scope, job, *choice);
        }
        if let Action::AnswerQuestions(ask, _) = &action
            && crate::yolo::job_of(ask).is_some()
        {
            return Err(io::Error::other(
                "Answer a YOLO request with one of its choices.",
            ));
        }
        match action {
            Action::EndSession(thread) => return self.end_thread(scope, &thread, false),
            Action::ForceEndSession(thread) => return self.end_thread(scope, &thread, true),
            Action::StartHandoff(start) => return self.start_handoff(id, key, scope, start),
            Action::YoloGrant(session, on) => return self.set_grant(scope, &session, on),
            Action::InjectHandoff(..) => {
                return Err(io::Error::other(
                    "Start a handoff with its source and request.",
                ));
            }
            _ => (),
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
        // A send to a thread this reopened route has not listed yet: add its row first, so
        // the store can resume the thread and deliver the message.
        if operator_turn
            && let Some(t) = &target
            && !live.store.has_live_session(t)
            && live.store.catalog_row(t).is_none()
            && let Some(row) = from_catalog(t)
        {
            live.store.adopt_catalog_row(row)?;
        }
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
        if let Some(t) = &target {
            live.tracker.touch(t);
        }
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
        self.process_scope(scope, events, from_action)?;
        self.advance_flows()
    }
    fn process_scope(
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
        let reported = live.store.take_context_windows();
        if !reported.is_empty() {
            let route = scope.route.clone();
            self.ledger.change(|d| {
                for (model, tokens) in reported {
                    crate::durable::record_window(
                        &mut d.windows,
                        crate::durable::Window {
                            route: route.clone(),
                            model,
                            tokens,
                        },
                    );
                }
            })?;
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
        // The requesting client may have gone while the handoff was prepared. Keep
        // it for the next subscriber instead of losing it with the terminal mode set.
        let handoff = match handoff {
            Some((client, frame)) if !self.clients.contains_key(&client) => {
                let key = match &frame {
                    Server::Handoff { id, .. } => id.clone(),
                    _ => String::new(),
                };
                live.held_handoff = Some((key, frame, Instant::now() + HELD_HANDOFF_TTL));
                None
            }
            other => other,
        };
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
                    launched = Some(Finished::new(l, Err("The runtime did not confirm a permission mode within the granted ceiling.".to_owned()), None));
                } else if matches!(l.payload, Payload::Handoff { .. })
                    && l.origin.runtime == ox_desk_core::runtime::Runtime::Codex
                {
                    // Stamp it now so no client sees it unstamped, but report the
                    // handoff only once the carried history and request are in.
                    self.workers.stamp(scope, thread, l.origin.clone());
                    live.confirming = Some(Confirming {
                        thread: thread.clone(),
                        deadline: Instant::now() + Duration::from_secs(60),
                        launch: l,
                    });
                } else {
                    self.workers.stamp(scope, thread, l.origin.clone());
                    if let Payload::Worker(w) = &l.payload
                        && let Some(entry) = self.workers.get_mut(scope, thread)
                    {
                        entry.marks.max_minutes = Some(w.max_minutes.unwrap_or_else(|| {
                            crate::workers::env_limit(
                                "OX_WORKER_MAX_MINUTES",
                                crate::workers::DEFAULT_MAX_MINUTES.into(),
                            ) as u32
                        }));
                        entry.marks.warning = self.launch_warnings.remove(&l.origin.job_id);
                    }
                    launched = Some(Finished::new(l, Ok(()), Some(thread.clone())));
                }
            }
        }
        if launched.is_none()
            && let Some(c) = &live.confirming
            && let Some(result) = first_turn(&live.store, &c.thread, c.deadline)
        {
            let c = live.confirming.take().expect("confirming");
            launched = Some(Finished::new(c.launch, result, Some(c.thread)));
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
            launched = Some(Finished::new(l, Err(error), None));
        }
        annotate(&self.workers, &self.ledger.data, scope, &mut live.store);
        let previous_yolo: Vec<wire::AskView> = {
            let (yolo, native): (Vec<_>, Vec<_>) = std::mem::take(&mut live.state.asks)
                .into_iter()
                .partition(crate::yolo::is_request);
            live.state.asks = native;
            yolo
        };
        dirty.merge(live.store.drain(vec![], Instant::now()));
        for thread in dirty.dirty_threads() {
            live.tracker.touch(thread);
        }
        let processes = live.store.live_threads();
        for thread in &processes {
            if live.store.thread_busy(thread) {
                live.tracker.touch(thread);
            }
        }
        live.tracker.sync(&processes);
        let mut changes = live.store.host_project(scope, &dirty, &mut live.state);
        crate::yolo::merge(
            &self.asking,
            scope,
            previous_yolo,
            &mut live.state.asks,
            &mut changes,
        );
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
    fn finish_launch(&mut self, scope: &Scope, launched: Option<Finished>) -> io::Result<()> {
        let Some(f) = launched else {
            return Ok(());
        };
        if let Some(record) = &f.record {
            let error = f.result.as_ref().err().cloned();
            self.ledger.change(|d| match error {
                None => d.handoffs.retain(|h| &h.id != record),
                Some(e) => {
                    if let Some(h) = d.handoffs.iter_mut().find(|h| &h.id == record) {
                        h.status = "failed".into();
                        h.message = Some(e);
                    }
                }
            })?;
            if let Some(source) = f.origin.parent.scope.clone()
                && self.running.contains_key(&source)
                && &source != scope
            {
                self.process(&source, vec![], true)?;
            }
            let result = f
                .result
                .map(|()| json!({"scope": scope, "session": f.thread, "relation": "handoff"}));
            return self.answer(f.reply, result);
        }
        if f.detached
            && let Err(e) = &f.result
        {
            let origin = f.origin.clone();
            let message = e.clone();
            self.ledger.change(|d| {
                d.outcomes.push(crate::durable::Outcome {
                    origin,
                    status: "failed".into(),
                    message,
                })
            })?;
        }
        let result = f.result.and_then(|()| {
            let thread = f.thread.expect("launched session");
            let job = self
                .workers
                .get(scope, &thread)
                .map(|e| e.origin.job_id)
                .ok_or("The worker stamp was not recorded.")?;
            self.job_status(job)
        });
        self.answer(f.reply, result)
    }
    /// Work a drain waits for: turns in progress in any loaded thread, worker
    /// launches and handoff targets not yet confirmed, and handoffs still
    /// preparing or reading their source.
    fn running_turns(&self) -> usize {
        self.running
            .values()
            .map(|live| {
                live.store.active_turns()
                    + usize::from(live.launching.is_some())
                    + usize::from(live.confirming.is_some())
            })
            .sum::<usize>()
            + self.preparing.len()
            + self.reading.len()
    }
    /// Check policy, then hand the launch to the worker's own runtime. Nothing
    /// starts unless every check passes. The reply waits for the native session,
    /// or, for a YOLO child that needs the operator, returns at once with
    /// `waiting_for_operator`.
    fn launch(&mut self, caller: Caller, start: WorkerStart, reply: Replier) -> io::Result<()> {
        match self.accept_launch(&caller, start) {
            Ok(Accepted::Launch(scope, launching)) => {
                let live = self.running.get_mut(&scope).expect("ensured worker scope");
                live.launching = Some(Launching { reply, ..launching });
                self.process(&scope, vec![], false)
            }
            Ok(Accepted::Ask(asking)) => self.ask_yolo(asking, reply),
            Err(e) => self.answer(reply, Err(e)),
        }
    }
    fn new_job_id(&self) -> Result<u64, String> {
        loop {
            let candidate = getrandom::u64().map_err(|e| e.to_string())? & ((1u64 << 52) - 1);
            if candidate > 0
                && !self.workers.job_ids().any(|j| j == candidate)
                && !self.asking.contains_key(&candidate)
                && !self
                    .ledger
                    .data
                    .outcomes
                    .iter()
                    .any(|o| o.origin.job_id == candidate)
            {
                return Ok(candidate);
            }
        }
    }
    fn accept_launch(&mut self, caller: &Caller, start: WorkerStart) -> Result<Accepted, String> {
        start.validate()?;
        let (project, parent, access, parent_origin, external, parent_scope) = match caller {
            Caller::Hosted { scope, hint, .. } => {
                let live = self
                    .running
                    .get(scope)
                    .ok_or("This OX helper's route is not open on the session host.")?;
                let parent = crate::policy::single_parent(&live.store, hint.as_deref())?;
                let access = live.store.applied_access(Some(&parent));
                let origin = self.workers.get(scope, &parent).map(|e| e.origin.clone());
                (
                    scope.project.clone(),
                    Some(parent),
                    access,
                    origin,
                    false,
                    Some(scope),
                )
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
                    Some(scope),
                )
            }
            Caller::External(c) => (c.project.clone(), None, None, None, true, None),
        };
        let source = parent_scope.and_then(|s| {
            crate::workers::source_level(s.route.runtime, access, parent_origin.as_ref())
        });
        let resolved =
            crate::workers::grant(source, external, start.mode_request(), start.runtime)?;
        let this_parent = crate::workers::parent_ref(caller, parent.clone());
        self.check_worker_cap(&this_parent)?;
        let scope = canonical_scope(Scope {
            project,
            route: start.route(),
        })
        .map_err(|e| e.to_string())?;
        let job_id = self.new_job_id()?;
        let mut origin = crate::workers::origin(caller, parent.clone(), &start, job_id, resolved);
        if let Some(warning) = self.write_overlap(&scope, resolved.level) {
            self.launch_warnings.insert(job_id, warning.clone());
            if let Some(live) = caller_scope(caller).and_then(|s| self.running.get_mut(s)) {
                live.store.post_notice(warning);
            }
        }
        if resolved.level.needs_person() {
            match caller {
                Caller::Operator { device, .. } if start.yolo_confirmed => {
                    origin.yolo = Some(YoloAllowance::Operator {
                        device: device.clone(),
                    });
                }
                Caller::Operator { .. } => {
                    return Err("Confirm YOLO for this worker first. It would run without permission prompts.".into());
                }
                Caller::Hosted { scope: ps, .. } => {
                    let session = parent.as_deref().expect("hosted parent");
                    match self.yolo_grant(ps, session) {
                        Some(grant) => origin.yolo = Some(grant),
                        None => {
                            let parent_title = self.running[ps]
                                .store
                                .catalog_row(session)
                                .map(|r| r.title().to_owned())
                                .unwrap_or_else(|| "OX session".into());
                            return Ok(Accepted::Ask(Asking {
                                origin,
                                start,
                                target: scope,
                                parent_title,
                            }));
                        }
                    }
                }
                Caller::External(_) => {
                    return Err("A terminal launch cannot start YOLO workers.".into());
                }
            }
        }
        let launching = self.prepare_launch(&scope, origin, Payload::Worker(start))?;
        Ok(Accepted::Launch(scope, launching))
    }
    /// Open the child's route and check that nothing else is opening there.
    fn prepare_launch(
        &mut self,
        scope: &Scope,
        origin: Origin,
        payload: Payload,
    ) -> Result<Launching, String> {
        self.ensure(scope, None).map_err(|e| e.to_string())?;
        let live = &self.running[scope];
        if live.opening.is_some() || live.launching.is_some() {
            return Err("Another session is opening on the target route. Retry shortly.".into());
        }
        Ok(Launching {
            origin,
            payload,
            reply: Replier::Channel(std::sync::mpsc::sync_channel(1).0),
            stage: Stage::Opening,
            deadline: Instant::now() + Duration::from_secs(60),
            detached: false,
        })
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
                let unstarted = self.unstarted_jobs(&me);
                let jobs = jobs
                    .into_iter()
                    .filter_map(|j| self.job_status(j).ok())
                    .chain(
                        unstarted
                            .into_iter()
                            .filter_map(|j| self.unstarted_job(&me, j)),
                    )
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
            ParentCall::ReadThread { .. } => {
                return Err("read_ox_thread is answered separately.".into());
            }
        };
        if self.workers.job(job_id).is_none()
            && let Some(status) = self.unstarted_job(&me, job_id)
        {
            return match call {
                ParentCall::Steer { .. } => Err(format!(
                    "This worker has not started: {}.",
                    status["status"].as_str().unwrap_or("waiting")
                )),
                ParentCall::Cancel { .. } => {
                    if let Some(done) = self.withdraw(job_id).map_err(|e| e.to_string())? {
                        Ok(done)
                    } else if let Some(done) =
                        self.abort_launch(job_id).map_err(|e| e.to_string())?
                    {
                        Ok(done)
                    } else {
                        Ok(status)
                    }
                }
                _ => Ok(status),
            };
        }
        let (scope, id) = self
            .workers
            .job(job_id)
            .filter(|(_, _, e)| owned(e))
            .map(|(s, id, _)| (s.clone(), id.to_owned()))
            .ok_or("Unknown worker job for this parent session.")?;
        match call {
            ParentCall::List
            | ParentCall::Bind { .. }
            | ParentCall::Status { .. }
            | ParentCall::ReadThread { .. } => self.job_status(job_id),
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
    /// Cancel a worker whose runtime is still opening: nothing was sent to it yet, so
    /// the launch is abandoned, its launcher hears "cancelled", and the worker's
    /// scope is closed again if nothing else uses it. A launch whose first message is
    /// already on its way cannot be recalled and keeps reporting `starting`.
    fn abort_launch(&mut self, job_id: u64) -> io::Result<Option<Value>> {
        let Some(scope) = self
            .running
            .iter()
            .find(|(_, live)| {
                live.launching.as_ref().is_some_and(|l| {
                    job_id != 0 && l.origin.job_id == job_id && l.stage != Stage::Sent
                })
            })
            .map(|(scope, _)| scope.clone())
        else {
            return Ok(None);
        };
        let l = self
            .running
            .get_mut(&scope)
            .and_then(|live| live.launching.take())
            .expect("found launching");
        const MESSAGE: &str = "The worker launch was cancelled before it started.";
        if l.detached {
            let origin = l.origin.clone();
            self.ledger.change(|d| {
                d.outcomes.push(crate::durable::Outcome {
                    origin,
                    status: "cancelled".into(),
                    message: MESSAGE.into(),
                })
            })?;
        }
        self.answer(l.reply, Err(MESSAGE.into()))?;
        self.process(&scope, vec![], false)?;
        self.reap_empty(&scope)?;
        Ok(Some(json!({"job_id": job_id, "status": "cancelled"})))
    }
    /// The live processes this host runs: one per Claude session, one per Codex scope.
    fn processes(&self) -> Vec<ProcessView> {
        let mut out = Vec::new();
        for (scope, live) in &self.running {
            let threads = live.store.live_threads();
            if threads.is_empty() {
                continue;
            }
            out.extend(self.scope_rows(scope, live, threads));
        }
        out.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| a.threads.cmp(&b.threads))
        });
        out
    }
    /// The process rows for one route's live threads: one per Claude session, one per Codex scope.
    fn scope_rows(&self, scope: &Scope, live: &Live, threads: Vec<String>) -> Vec<ProcessView> {
        let mut out = Vec::new();
        let claude = scope.route.runtime == ox_desk_core::runtime::Runtime::Claude;
        let groups: Vec<Vec<String>> = if claude {
            threads.into_iter().map(|t| vec![t]).collect()
        } else {
            vec![threads]
        };
        for group in groups {
            let seen = |t: &String| live.tracker.threads.get(t).copied();
            let started = group
                .iter()
                .filter_map(seen)
                .map(|s| s.started)
                .min()
                .unwrap_or(live.tracker.scope.started);
            let last = group
                .iter()
                .filter_map(seen)
                .map(|s| s.last_unix)
                .max()
                .unwrap_or(live.tracker.scope.last_unix);
            let worker = group.iter().all(|t| self.workers.get(scope, t).is_some());
            out.push(ProcessView {
                kind: if worker {
                    "worker"
                } else if claude {
                    "claude"
                } else {
                    "codex"
                }
                .into(),
                scope: scope.clone(),
                project: scope.project.clone(),
                route: scope.route.label(),
                model: group
                    .first()
                    .map(|t| live.store.options(Some(t)).model)
                    .filter(|m| !m.is_empty()),
                started_at: started,
                last_activity: last,
                running: group.iter().any(|t| self.thread_busy(scope, t)),
                draft: claude && group.iter().all(|t| live.store.is_draft(t)),
                threads: group,
            });
        }
        out
    }
    /// Whether ending `thread` in `scope` would interrupt work: its own turn, approval,
    /// sign-in or terminal, a launch still opening for it, or a busy host-owned worker
    /// it started on any scope, at any depth. The reaper and the process list share this.
    fn thread_busy(&self, scope: &Scope, thread: &str) -> bool {
        let mut stack = vec![(scope.clone(), thread.to_owned())];
        let mut seen = HashSet::new();
        while let Some((s, t)) = stack.pop() {
            if !seen.insert((s.clone(), t.clone())) {
                continue;
            }
            let launching = self.running.values().any(|l| {
                l.launching.as_ref().is_some_and(|launch| {
                    launch.origin.parent.scope.as_ref() == Some(&s)
                        && launch.origin.parent.session.as_deref() == Some(t.as_str())
                })
            });
            if launching {
                return true;
            }
            if let Some(live) = self.running.get(&s)
                && (live.store.thread_busy(&t)
                    || live.store.in_terminal(&t)
                    || live.launching.is_some()
                    || live.confirming.is_some())
            {
                return true;
            }
            stack.extend(
                self.workers
                    .children_of(&s, &t)
                    .map(|(cs, cid)| (cs.clone(), cid.to_owned())),
            );
        }
        false
    }
    /// End one thread's process and the host-owned workers it started. The thread
    /// stays in the catalog; selecting it again resumes it.
    ///
    /// A busy thread is refused unless `force` is set, so a stale window or remote client
    /// cannot kill a running turn and its workers by accident.
    fn end_thread(&mut self, scope: &Scope, thread: &str, force: bool) -> io::Result<()> {
        if !self.running.contains_key(scope) {
            return Err(io::Error::other(
                "This route is not open on the session host.",
            ));
        }
        if !force && self.thread_busy(scope, thread) {
            return Err(io::Error::other(
                "This session is busy. Confirm ending it, which cancels its running work.",
            ));
        }
        let live = self
            .running
            .get(scope)
            .ok_or_else(|| io::Error::other("This route is not open on the session host."))?;
        if !live.store.has_live_session(thread) {
            return Err(io::Error::other("This session has no live process to end."));
        }
        // End the parent first: if the store refuses (not ready, sign-in needed),
        // its workers must not already be cancelled.
        let live = self.running.get_mut(scope).expect("checked above");
        let busy = live.store.thread_busy(thread);
        live.store.end_session(thread)?;
        live.tracker.threads.remove(thread);
        self.drop_grant(scope, thread)?;
        if busy && let Some(entry) = self.workers.get_mut(scope, thread) {
            entry.marks.cancelled = true;
        }
        self.end_children(scope, thread)?;
        Ok(())
    }
    /// Ending a parent ends its host-owned workers: launches still opening are
    /// aborted and running ones are cancelled and ended.
    fn end_children(&mut self, scope: &Scope, thread: &str) -> io::Result<()> {
        self.withdraw_asks_under(scope, thread)?;
        let launches: Vec<u64> = self
            .running
            .values()
            .filter_map(|l| l.launching.as_ref())
            .filter(|l| {
                l.origin.job_id != 0
                    && l.origin.parent.scope.as_ref() == Some(scope)
                    && l.origin.parent.session.as_deref() == Some(thread)
            })
            .map(|l| l.origin.job_id)
            .collect();
        for job in launches {
            self.abort_launch(job)?;
        }
        let children: Vec<(Scope, String)> = self
            .workers
            .children_of(scope, thread)
            .filter(|(s, id)| {
                self.running
                    .get(*s)
                    .is_some_and(|l| l.store.has_live_session(id))
            })
            .map(|(s, id)| (s.clone(), id.to_owned()))
            .collect();
        for (child_scope, child) in children {
            if self.end_thread(&child_scope, &child, true).is_ok()
                && self.running.contains_key(&child_scope)
            {
                self.process(&child_scope, vec![], true)?;
                self.reap_empty(&child_scope)?;
            }
        }
        Ok(())
    }
    /// Close a route's runtime once it serves nothing: no live process or
    /// subscription, no turn, approval or launch. Dropping its store kills the
    /// app-server (or the idle Claude draft process). The saved sessions stay listed
    /// and the next subscriber or action reopens the route.
    fn reap_empty(&mut self, scope: &Scope) -> io::Result<()> {
        let Some(live) = self.running.get(scope) else {
            return Ok(());
        };
        let idle = live.store.live_threads().is_empty()
            && live.store.active_turns() == 0
            && live.store.asks().is_empty()
            && !live.store.opening_thread()
            && !live.store.opening_terminal()
            && live.launching.is_none()
            && live.confirming.is_none()
            && live.opening.is_none()
            && live.terminal.is_none()
            && live.held_handoff.is_none();
        if !idle {
            return Ok(());
        }
        let live = self.running.remove(scope).expect("checked above");
        let recovered = SavedScope::from_state(&live.state).recover();
        drop(live);
        let changes = vec![
            Change::State {
                connection: recovered.connection.clone(),
                capabilities: vec![],
                detail: live_detail_after_drop(&recovered),
            },
            Change::Sessions {
                sessions: recovered.sessions.clone(),
            },
            Change::Asks { asks: vec![] },
        ];
        self.cold.insert(scope.clone(), recovered);
        self.broadcast(scope, changes)
    }
    /// End sessions and empty routes unused for the idle timeout. The sessions stay resumable.
    fn reap_idle(&mut self, now: Instant) -> io::Result<()> {
        let mut due: Vec<(Scope, String)> = Vec::new();
        let mut empty: Vec<Scope> = Vec::new();
        for (scope, live) in &mut self.running {
            if live
                .held_handoff
                .as_ref()
                .is_some_and(|(_, _, deadline)| *deadline <= now)
            {
                live.held_handoff = None;
                empty.push(scope.clone());
            }
        }
        for (scope, live) in &self.running {
            let threads = live.store.live_threads();
            if threads.is_empty() {
                if reapable(
                    now.saturating_duration_since(live.tracker.scope.last),
                    self.idle,
                    live.store.active_turns() > 0,
                ) {
                    empty.push(scope.clone());
                }
                continue;
            }
            for thread in threads {
                let Some(seen) = live.tracker.threads.get(&thread) else {
                    continue;
                };
                let busy = self.thread_busy(scope, &thread);
                if reapable(now.saturating_duration_since(seen.last), self.idle, busy) {
                    due.push((scope.clone(), thread));
                }
            }
        }
        for (scope, thread) in due {
            if self.end_thread(&scope, &thread, false).is_ok() && self.running.contains_key(&scope)
            {
                self.process(&scope, vec![], true)?;
            }
            self.reap_empty(&scope)?;
        }
        for scope in empty {
            self.reap_empty(&scope)?;
        }
        self.expire_workers()
    }
    fn broadcast(&mut self, scope: &Scope, changes: Vec<Change>) -> io::Result<()> {
        self.replay.cursor.seq = self.ledger.next_sequence(self.replay.cursor.seq)?;
        if self.clients.values().any(catalog_only) {
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
            // Web push subscribes without a scope and needs notifications, which
            // carry no transcript content.
            catalog.extend(
                changes
                    .iter()
                    .filter(|c| matches!(c, Change::Notification { .. }))
                    .cloned(),
            );
            if !catalog.is_empty() {
                let packet = encode(&Server::Delta {
                    cursor: self.replay.cursor.clone(),
                    scope: scope.clone(),
                    changes: catalog,
                })?;
                self.clients.retain(|_, c| {
                    !catalog_only(c) || c.peer.send_for(packet.clone(), "catalog watch")
                });
            }
        }
        let packet = encode(&Server::Delta {
            cursor: self.replay.cursor.clone(),
            scope: scope.clone(),
            changes,
        })?;
        self.replay.append(scope.clone(), packet.clone());
        let what = format!("{} {}", scope.project.display(), scope.route.label());
        self.clients.retain(|_, c| match c.subscription.as_ref() {
            Some(Some(s)) if s == scope => c.peer.send_for(packet.clone(), &what),
            Some(None) if !c.catalog => c.peer.send_for(packet.clone(), &what),
            _ => true,
        });
        Ok(())
    }
}

/// Clients that receive navigation metadata only: the catalog watch and scope-less subscriptions.
// Only catalog watchers are filtered. A scope-less Subscribe means "every scope"
// (the phone and web push rely on asks and notifications from it), as on main.
fn catalog_only(client: &ClientState) -> bool {
    client.catalog
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
    let preset = l
        .origin
        .started
        .unwrap_or_else(|| l.origin.requested.preset(l.origin.runtime));
    let goal = matches!(&l.payload, Payload::Worker(w) if w.mode == WorkerMode::Goal);
    match l.stage {
        Stage::Opening if live.store.can_send(None) => {
            l.stage = Stage::AccessRequested;
            if let Err(e) = live.store.dispatch(Action::SetNextAccess(preset)) {
                return Some(e.to_string());
            }
            advance_launch(live)
        }
        Stage::AccessRequested
            if goal && (!live.store.has_command_schema() || live.store.commands_loading()) =>
        {
            None
        }
        Stage::AccessRequested if goal && !live.store.command_caps(None).supports_goal() => {
            Some("Codex Goal mode is not enabled on the worker's route.".into())
        }
        Stage::AccessRequested
            if live.store.can_send(None)
                && (live.store.next_access() == Some(preset)
                    || live.store.applied_access(None) == Some(preset)) =>
        {
            l.stage = Stage::Sent;
            let action = match &mut l.payload {
                Payload::Worker(start) if start.mode == WorkerMode::Goal => {
                    Action::NativeCommand(None, start.prompt())
                }
                Payload::Worker(start) => Action::SendText(None, start.prompt()),
                Payload::Handoff { text, items, .. } => {
                    Action::InjectHandoff(std::mem::take(text), std::mem::take(items))
                }
            };
            live.store.dispatch(action).err().map(|e| e.to_string())
        }
        _ => None,
    }
}

/// Publish each session's stamp with its row. A hosted parent nests the worker
/// across scopes through `origin.parent.scope`; a handoff target does not nest.
/// A handoff source lists where it was handed off to, and a session with a YOLO
/// grant lists the running children that grant allowed.
fn annotate(workers: &Registry, data: &crate::durable::Owned, scope: &Scope, store: &mut Store) {
    use ox_desk_core::origin::Relation;
    type Notes = BTreeMap<String, (Option<String>, serde_json::Map<String, Value>)>;
    let mut out: Notes = BTreeMap::new();
    for (id, entry) in workers.in_scope(scope) {
        let o = &entry.origin;
        let nests = o.relation != Relation::Handoff && o.handoff.is_none();
        let e = out.entry(id.to_owned()).or_default();
        if nests {
            e.0 = o.parent.scope.as_ref().and(o.parent.session.clone());
        }
        e.1.insert("origin".into(), json!(o));
        e.1.insert("worker_marks".into(), json!(entry.marks));
        e.1.insert("worker".into(), nests.into());
    }
    let mut push = |id: &str, key: &str, value: Value| {
        let e = out.entry(id.to_owned()).or_default();
        let list = e.1.entry(key.to_owned()).or_insert_with(|| json!([]));
        if let Some(list) = list.as_array_mut() {
            list.push(value);
        }
    };
    for (s, id, entry) in workers.all() {
        let o = &entry.origin;
        if o.relation == Relation::Handoff
            && let Some(h) = o.handoff.as_ref().filter(|h| &h.scope == scope)
        {
            push(
                &h.session,
                "handed_off_to",
                json!({"scope": s, "session": id, "runtime": o.runtime, "route": o.route, "label": o.route.label()}),
            );
        }
    }
    for h in &data.handoffs {
        if let Some(src) = h.origin.handoff.as_ref().filter(|src| &src.scope == scope) {
            push(
                &src.session,
                "handoff_attempts",
                json!({"id": h.id, "scope": h.target, "status": h.status, "message": h.message, "label": h.target.route.label()}),
            );
        }
    }
    let now = crate::durable::unix_now();
    for g in data
        .grants
        .iter()
        .filter(|g| &g.scope == scope && g.live(now))
    {
        let children: Vec<Value> = workers
            .all()
            .filter(|(_, _, e)| {
                matches!(&e.origin.yolo, Some(YoloAllowance::Grant { session: Some(s), .. }) if s == &g.session)
            })
            .map(|(s, id, _)| json!({"scope": s, "session": id}))
            .collect();
        let e = out.entry(g.session.clone()).or_default();
        e.1.insert("yolo_grant".into(), true.into());
        e.1.insert("yolo_grant_expires".into(), g.expires_at().into());
        e.1.insert("yolo_children".into(), children.into());
    }
    for (id, (parent, detail)) in out {
        store.annotate_session(&id, parent, Value::Object(detail));
    }
}
fn caller_scope(caller: &Caller) -> Option<&Scope> {
    match caller {
        Caller::Hosted { scope, .. } | Caller::Operator { scope, .. } => Some(scope),
        Caller::External(_) => None,
    }
}
fn key_ok(reply: &Replier) -> bool {
    match reply {
        Replier::Client(_, key) => !key.is_empty() && key.len() <= 256,
        Replier::Channel(_) => true,
    }
}

/// What a scope shows its subscribers after the host closed its idle runtime.
fn live_detail_after_drop(recovered: &ScopeState) -> Value {
    let mut detail = recovered.detail.clone();
    detail["notice"] = json!("Idle session ended. Select a thread to resume it.");
    detail
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
    use super::flows::ENDED_PARENT;
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
                confirming: None,
                opening: None,
                terminal: None,
                schema_started: true,
                recovered: vec![],
                inbox: Inbox::new(|| {}),
                generation: 1,
                tokens: HashMap::new(),
                tracker: Tracker::new(),
                held_handoff: None,
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

    #[test]
    fn a_refused_action_retried_with_the_same_key_runs_once_it_is_valid() {
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
                confirming: None,
                opening: None,
                terminal: None,
                schema_started: true,
                recovered: vec![],
                inbox: Inbox::new(|| {}),
                generation: 1,
                tokens: HashMap::new(),
                tracker: Tracker::new(),
                held_handoff: None,
            },
        );
        let ask = LinkEvent::Frame(
            Frame::decode(
                br#"{"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"a","turnId":"t","itemId":"i","command":"test"}}"#,
                |_| None,
            )
            .unwrap(),
        );
        engine.process(&scope, vec![ask], false).unwrap();
        let token = engine.running[&scope].state.asks[0].token.clone();
        let (peer, frames) = crate::transport::test_peer();
        engine.clients.insert(
            1,
            ClientState {
                peer,
                hello: true,
                prepared: None,
                subscription: Some(Some(scope.clone())),
                catalog: false,
                terminal: false,
            },
        );
        let act = |approval: Option<String>| Client::Act {
            id: "retry".into(),
            scope: scope.clone(),
            action: Action::AnswerAsk(RequestId::Number(7), 0),
            approval,
        };
        // The first attempt is refused for lack of approval; the same key with the approval must run.
        engine.request(1, act(None)).unwrap();
        engine.request(1, act(Some(token))).unwrap();
        let sent: Vec<Server> = frames
            .try_iter()
            .map(|p| serde_json::from_slice(&p).unwrap())
            .filter(|f| !matches!(f, Server::Delta { .. }))
            .collect();
        assert!(
            matches!(sent.as_slice(), [Server::Error { .. }, Server::Ack { .. }]),
            "{sent:?}"
        );
    }

    #[test]
    fn catalog_watchers_get_catalog_deltas_and_all_scope_subscribers_get_everything() {
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
        let mut client = |id: u64, subscription: Option<Option<Scope>>, catalog: bool| {
            let (peer, frames) = crate::transport::test_peer();
            engine.clients.insert(
                id,
                ClientState {
                    peer,
                    hello: true,
                    prepared: None,
                    subscription,
                    catalog,
                },
            );
            frames
        };
        let legacy = client(1, Some(None), false);
        let watcher = client(2, None, true);
        let scoped = client(3, Some(Some(scope.clone())), false);
        let session = wire::SessionView {
            id: "t".into(),
            title: "Title".into(),
            parent: None,
            status: "idle".into(),
            updated_at: 1,
            capabilities: vec![],
            detail: json!({"owned": true, "row": true}),
        };
        let transcript = Change::Transcript {
            thread: "t".into(),
            timeline: wire::Timeline {
                items: vec![],
                detail: Value::Null,
            },
        };
        engine
            .broadcast(
                &scope,
                vec![
                    Change::Sessions {
                        sessions: vec![session],
                    },
                    transcript,
                    Change::Notification {
                        thread: "t".into(),
                        title: "Title".into(),
                        event: ox_desk_core::host_protocol::NotificationKind::Finished,
                    },
                ],
            )
            .unwrap();
        let decode =
            |frames: std::sync::mpsc::Receiver<crate::transport::Packet>| -> Vec<Vec<Change>> {
                frames
                    .try_iter()
                    .map(|p| match serde_json::from_slice(&p).unwrap() {
                        Server::Delta { changes, .. } => changes,
                        other => panic!("unexpected frame {other:?}"),
                    })
                    .collect()
            };
        // A scope-less Subscribe means every scope: the phone and web push need its
        // asks and notifications, so it receives the full packet like on main.
        let all = decode(legacy);
        assert_eq!(all.len(), 1);
        assert!(
            all[0]
                .iter()
                .any(|c| matches!(c, Change::Transcript { .. }))
        );
        assert!(
            all[0]
                .iter()
                .any(|c| matches!(c, Change::Notification { .. }))
        );
        {
            let frames = decode(watcher);
            assert_eq!(frames.len(), 1);
            assert!(
                frames[0]
                    .iter()
                    .all(|c| !matches!(c, Change::Transcript { .. })),
                "{frames:?}"
            );
            assert!(matches!(
                frames[0].as_slice(),
                [Change::Sessions { .. }, Change::Notification { .. }]
            ));
        }
        let scoped = decode(scoped);
        assert_eq!(scoped.len(), 1);
        assert!(
            scoped[0]
                .iter()
                .any(|c| matches!(c, Change::Transcript { .. }))
        );
    }

    #[test]
    fn a_failed_settings_read_keeps_the_last_project_default() {
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
        // Every settings read fails here, so no default has ever been seen.
        assert!(engine.yolo_grant(&scope, "session").is_none());
        // Once a read has succeeded, a later failure keeps honouring that default.
        engine.yolo_projects = vec![scope.project.clone()];
        assert!(matches!(
            engine.yolo_grant(&scope, "session"),
            Some(YoloAllowance::Grant { session: None, .. })
        ));
    }

    mod workers {
        use super::*;
        use ox_desk_core::{
            host_protocol::{AskView, Timeline},
            origin::{Ceiling, ExternalCaller, ParentRef, Relation},
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
            let transcripts = active
                .iter()
                .map(|id| {
                    (
                        *id,
                        Timeline {
                            items: vec![],
                            detail: json!({"active": "t1", "ready": true}),
                        },
                    )
                })
                .collect();
            live_with(scope, sessions, transcripts, asks)
        }
        /// A runtime stand-in with the given transcripts.
        fn live_with(
            scope: &Scope,
            sessions: Vec<wire::SessionView>,
            transcripts: Vec<(&str, Timeline)>,
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
                    transcripts: transcripts
                        .into_iter()
                        .map(|(id, t)| (id.to_owned(), t))
                        .collect(),
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
                    confirming: None,
                    opening: None,
                    terminal: None,
                    schema_started: true,
                    recovered: vec![],
                    inbox: Inbox::new(|| {}),
                    generation: 1,
                    tokens: HashMap::new(),
                    tracker: Tracker::new(),
                    held_handoff: None,
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
                started: None,
                decided: None,
                yolo: None,
                handoff: None,
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
                write: Some(write),
                permission: None,
                yolo_confirmed: false,
                max_minutes: None,
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
                    .contains("above the source")
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
                        terminal: false,
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
                    write: Some(true),
                    permission: None,
                    yolo_confirmed: false,
                    max_minutes: None,
                },
                41,
                ox_desk_core::modes::Resolved {
                    level: Ceiling::Write,
                    access: Access::Workspace,
                    decision: ox_desk_core::modes::Decision::Lowered,
                },
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

        fn timeline(items: &[(&str, &str, &str)], active: bool) -> Timeline {
            Timeline {
                items: items
                    .iter()
                    .map(|(id, role, text)| wire::TimelineItem {
                        turn: format!("turn-{id}"),
                        id: (*id).into(),
                        role: (*role).into(),
                        text: (*text).into(),
                        revision: 1,
                        detail: match *role {
                            "user" => json!({"UserMessage": {"content": [{"text": text}]}}),
                            "thinking" => json!({"Reasoning": {"summary": [text]}}),
                            "tool" => json!({"item": {"CommandExecution": {
                                "command": text, "status": "completed",
                                "aggregated_output": "ok", "exit_code": 0}}, "exit": 0}),
                            _ => json!({"update": false}),
                        },
                    })
                    .collect(),
                detail: if active {
                    json!({"active": "t1", "ready": true})
                } else {
                    json!({"ready": true})
                },
            }
        }
        fn inherit(runtime: Runtime) -> WorkerStart {
            WorkerStart {
                runtime,
                write: None,
                ..start(false)
            }
        }
        fn launched(a: Result<Accepted, String>) -> Origin {
            match a {
                Ok(Accepted::Launch(_, l)) => l.origin,
                Ok(Accepted::Ask(_)) => panic!("asked the operator"),
                Err(e) => panic!("{e}"),
            }
        }
        fn routes(root: &Path) -> (Scope, Scope, Scope) {
            let (parent, worker) = scopes(root);
            let claude = Scope {
                route: Runtime::Claude.route(Some("fixture".into())),
                ..parent.clone()
            };
            (parent, worker, claude)
        }
        fn with_parent(engine: &mut Engine, parent: &Scope, access: &str) {
            engine.running.insert(
                parent.clone(),
                live(
                    parent,
                    vec![view(parent, "a", "running", access)],
                    &["a"],
                    vec![],
                )
                .0,
            );
        }
        fn thread(scope: &Scope, id: &str) -> Caller {
            Caller::Hosted {
                scope: scope.clone(),
                hint: Some(id.into()),
                launch: "helper".into(),
            }
        }

        #[test]
        fn workers_inherit_the_mapped_mode_lower_on_request_and_classifiers_need_no_ask() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, claude) = routes(root.path());
            for scope in [&worker, &claude] {
                engine
                    .running
                    .insert(scope.clone(), live(scope, vec![], &[], vec![]).0);
            }
            use ox_desk_core::modes::{Decision, Level, ModeRequest};
            for (access, runtime, request, expect, decided) in [
                (
                    "workspace",
                    Runtime::Codex,
                    None,
                    Access::Workspace,
                    Decision::Inherited,
                ),
                (
                    "workspace",
                    Runtime::Claude,
                    None,
                    Access::AcceptEdits,
                    Decision::Inherited,
                ),
                (
                    "workspace",
                    Runtime::Codex,
                    Some(Level::Read),
                    Access::ReadOnly,
                    Decision::Lowered,
                ),
                (
                    "auto_review",
                    Runtime::Claude,
                    None,
                    Access::Auto,
                    Decision::Inherited,
                ),
                (
                    "auto_review",
                    Runtime::Codex,
                    None,
                    Access::AutoReview,
                    Decision::Inherited,
                ),
            ] {
                with_parent(&mut engine, &parent, access);
                let mut s = inherit(runtime);
                s.permission = request.map(ModeRequest::Level);
                let o = launched(engine.accept_launch(&thread(&parent, "a"), s));
                assert_eq!(
                    (o.started, o.decided),
                    (Some(expect), Some(decided)),
                    "{access} {runtime:?}"
                );
                assert!(o.yolo.is_none());
            }
            // Old callers: workspace_write false is read, true is write, capped by the source.
            with_parent(&mut engine, &parent, "read_only");
            let o = launched(engine.accept_launch(&thread(&parent, "a"), start(false)));
            assert_eq!(o.started, Some(Access::ReadOnly));
            let err = engine
                .accept_launch(&thread(&parent, "a"), start(true))
                .err()
                .unwrap();
            assert!(err.contains("above the source"), "{err}");
            // A request above the source is refused before anything starts.
            let mut above = inherit(Runtime::Codex);
            above.permission = Some(ModeRequest::Level(Level::Yolo));
            assert!(engine.accept_launch(&thread(&parent, "a"), above).is_err());
            // An unverified parent mode grants nothing.
            with_parent(&mut engine, &parent, "configured");
            assert!(
                engine
                    .accept_launch(&thread(&parent, "a"), inherit(Runtime::Codex))
                    .is_err()
            );
        }

        #[test]
        fn a_yolo_child_waits_for_the_operator_who_accepts_one_and_declines_another() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "yolo");
            engine
                .running
                .insert(worker.clone(), live(&worker, vec![], &[], vec![]).0);
            let request = |engine: &mut Engine, confirmed: bool| {
                let (done, result) = std::sync::mpsc::sync_channel(1);
                let mut s = inherit(Runtime::Codex);
                // An agent cannot confirm YOLO for itself.
                s.yolo_confirmed = confirmed;
                engine
                    .launch(thread(&parent, "a"), s, Replier::Channel(done))
                    .unwrap();
                result.try_recv().unwrap().unwrap()
            };
            let first = request(&mut engine, true);
            assert_eq!(first["status"], "waiting_for_operator");
            assert_eq!(first["waiting_for_operator"]["reason"], "yolo_acceptance");
            assert!(
                engine.running[&worker].launching.is_none(),
                "nothing started"
            );
            let job = first["job_id"].as_u64().unwrap();
            let ask = engine.running[&parent]
                .state
                .asks
                .iter()
                .find(|a| crate::yolo::job_of(&a.id) == Some(job))
                .cloned()
                .expect("an operator ask on the parent session");
            assert_eq!(ask.thread.as_deref(), Some("a"));
            assert_eq!(
                ask.choices,
                ["Accept YOLO", "Start in Auto-review", "Decline"]
            );
            for part in [
                "Parent: a",
                "Runtime: Codex",
                "Route: OX API · fixture",
                "Model: fixture",
                "Task: check",
            ] {
                assert!(ask.detail.contains(part), "{part} in {}", ask.detail);
            }
            assert!(!ask.token.is_empty());
            // The parent sees waiting_for_operator and has no way to answer.
            let status = engine
                .parent_call(&thread(&parent, "a"), ParentCall::Status { job_id: job })
                .unwrap();
            assert_eq!(status["status"], "waiting_for_operator");
            assert!(
                engine
                    .parent_call(
                        &thread(&parent, "a"),
                        ParentCall::Steer {
                            job_id: job,
                            text: "yes".into()
                        }
                    )
                    .is_err()
            );
            assert!(
                crate::workers::parent_call(json!({"op":"answer","job_id":job,"choice":0}))
                    .is_err()
            );
            assert_eq!(
                engine
                    .parent_call(&thread(&parent, "a"), ParentCall::List)
                    .unwrap()["jobs"][0]["status"],
                "waiting_for_operator"
            );
            // A stale token is refused; the operator accepts with the live one.
            assert!(
                engine
                    .act(
                        7,
                        "x",
                        &parent,
                        Action::AnswerAsk(ask.id.clone(), 0),
                        Some("stale".into())
                    )
                    .is_err()
            );
            engine
                .act(
                    7,
                    "y1",
                    &parent,
                    Action::AnswerAsk(ask.id.clone(), 0),
                    Some(ask.token.clone()),
                )
                .unwrap();
            let l = engine.running[&worker]
                .launching
                .as_ref()
                .expect("launch after acceptance");
            assert_eq!(l.origin.job_id, job);
            assert_eq!(l.origin.started, Some(Access::Yolo));
            assert!(
                matches!(&l.origin.yolo, Some(YoloAllowance::Accepted { request, .. }) if request.ends_with(&job.to_string()))
            );
            assert_eq!(
                engine
                    .parent_call(&thread(&parent, "a"), ParentCall::Status { job_id: job })
                    .unwrap()["status"],
                "starting"
            );
            assert!(
                !engine.running[&parent]
                    .state
                    .asks
                    .iter()
                    .any(crate::yolo::is_request)
            );
            // A second request is declined.
            let second = request(&mut engine, false)["job_id"].as_u64().unwrap();
            engine.process(&parent, vec![], true).unwrap();
            let ask = engine.running[&parent]
                .state
                .asks
                .iter()
                .find(|a| crate::yolo::job_of(&a.id) == Some(second))
                .cloned()
                .unwrap();
            engine
                .act(
                    7,
                    "y2",
                    &parent,
                    Action::AnswerAsk(ask.id, 2),
                    Some(ask.token),
                )
                .unwrap();
            let status = engine
                .parent_call(&thread(&parent, "a"), ParentCall::Status { job_id: second })
                .unwrap();
            assert_eq!(status["status"], "declined");
            // The ledger says which acceptance allowed nothing yet, and never stores the task.
            let saved = std::fs::read_to_string(root.path().join("ownership-v1.json")).unwrap();
            assert!(saved.contains("declined") && !saved.contains("\"task\""));
            // "Start in Auto-review" starts the child below YOLO.
            let third = request(&mut engine, false)["job_id"].as_u64().unwrap();
            engine.running.get_mut(&worker).unwrap().launching = None;
            let ask = engine.running[&parent]
                .state
                .asks
                .iter()
                .find(|a| crate::yolo::job_of(&a.id) == Some(third))
                .cloned()
                .unwrap();
            engine
                .act(
                    7,
                    "y3",
                    &parent,
                    Action::AnswerAsk(ask.id, 1),
                    Some(ask.token),
                )
                .unwrap();
            let l = engine.running[&worker].launching.as_ref().unwrap();
            assert_eq!(l.origin.started, Some(Access::AutoReview));
            assert!(l.origin.yolo.is_none());
            assert_eq!(
                l.origin.decided,
                Some(ox_desk_core::modes::Decision::Capped)
            );
        }

        #[test]
        fn a_grant_expires_after_twelve_hours_and_ends_with_its_session() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, _worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "yolo");
            engine
                .act(1, "g1", &parent, Action::YoloGrant("a".into(), true), None)
                .unwrap();
            let grant = engine.ledger.data.grants[0].clone();
            assert!(grant.granted_at > 0);
            assert_eq!(grant.expires_at(), grant.granted_at + 12 * 3600);
            assert!(engine.yolo_grant(&parent, "a").is_some());
            // Past twelve hours the grant no longer covers anything and is pruned.
            engine
                .ledger
                .change(|d| d.grants[0].granted_at -= 12 * 3600 + 1)
                .unwrap();
            assert!(engine.yolo_grant(&parent, "a").is_none());
            assert!(engine.ledger.data.grants.is_empty());
            // A fresh grant is removed when its session is ended.
            engine
                .act(1, "g2", &parent, Action::YoloGrant("a".into(), true), None)
                .unwrap();
            engine.drop_grant(&parent, "a").unwrap();
            assert!(engine.yolo_grant(&parent, "a").is_none());
        }

        #[test]
        fn a_project_yolo_default_is_announced_once() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, _worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "yolo");
            engine.yolo_projects = vec![parent.project.clone()];
            assert!(engine.yolo_grant(&parent, "a").is_some());
            let notice = engine.running[&parent].store.notice().map(str::to_owned);
            assert!(
                notice
                    .unwrap()
                    .contains("YOLO is on for this project by default")
            );
            engine
                .running
                .get_mut(&parent)
                .unwrap()
                .store
                .dismiss_notice();
            assert!(engine.yolo_grant(&parent, "a").is_some());
            assert!(
                engine.running[&parent].store.notice().is_none(),
                "announced once"
            );
        }

        #[test]
        fn a_parent_runs_at_most_the_capped_number_of_workers_and_expired_ones_stop() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "workspace");
            let ids = ["w1", "w2", "w3", "w4"];
            engine.running.insert(
                worker.clone(),
                live(
                    &worker,
                    ids.iter()
                        .map(|id| view(&worker, id, "running", "read_only"))
                        .collect(),
                    &ids,
                    vec![],
                )
                .0,
            );
            let parent_ref = workers_parent(&parent, "a");
            for (n, id) in ids.iter().enumerate() {
                let mut o = origin(parent_ref.clone(), Ceiling::Write);
                o.job_id = 100 + n as u64;
                engine.workers.stamp(&worker, id, o);
            }
            let err = engine.check_worker_cap(&parent_ref).unwrap_err();
            assert!(err.contains("OX_WORKER_MAX_PER_PARENT"), "{err}");
            // A write worker beside running write workers is warned, a read one is not.
            assert!(
                engine
                    .write_overlap(&worker, Ceiling::Write)
                    .unwrap()
                    .contains("git worktree")
            );
            assert!(engine.write_overlap(&worker, Ceiling::Read).is_none());
            // Past its limit a running worker is stopped and reported as expired.
            let entry = engine.workers.get_mut(&worker, "w1").unwrap();
            entry.marks.max_minutes = Some(1);
            entry.marks.started_at -= 120;
            engine.expire_workers().unwrap();
            let status = engine.job_status(100).unwrap();
            assert_eq!(status["status"], "cancelled");
            assert!(status["error"].as_str().unwrap().contains("1 minutes"));
            assert!(status["elapsed_seconds"].as_i64().unwrap() >= 120);
            assert_eq!(engine.job_status(101).unwrap()["status"], "running");
        }

        #[test]
        fn a_grant_covers_descendants_and_revoking_it_makes_the_next_spawn_ask() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "yolo");
            engine.running.insert(
                worker.clone(),
                live(
                    &worker,
                    vec![view(&worker, "w", "running", "yolo")],
                    &["w"],
                    vec![],
                )
                .0,
            );
            engine
                .act(1, "g1", &parent, Action::YoloGrant("a".into(), true), None)
                .unwrap();
            let o = launched(engine.accept_launch(&thread(&parent, "a"), inherit(Runtime::Codex)));
            assert_eq!(o.started, Some(Access::Yolo));
            assert!(
                matches!(&o.yolo, Some(YoloAllowance::Grant { session: Some(s), .. }) if s == "a")
            );
            // A YOLO worker of the granted session is covered too.
            let mut child = origin(workers_parent(&parent, "a"), Ceiling::Yolo);
            child.yolo = o.yolo.clone();
            engine.workers.stamp(&worker, "w", child);
            engine.process(&parent, vec![], true).unwrap();
            let row = engine.running[&parent]
                .state
                .sessions
                .iter()
                .find(|r| r.id == "a")
                .unwrap();
            assert_eq!(row.detail["yolo_grant"], true);
            assert_eq!(row.detail["yolo_children"][0]["session"], "w");
            let nested =
                launched(engine.accept_launch(&thread(&worker, "w"), inherit(Runtime::Codex)));
            assert!(matches!(nested.yolo, Some(YoloAllowance::Grant { .. })));
            // Revoking affects new spawns only; the running child keeps its stamp.
            engine
                .act(1, "g2", &parent, Action::YoloGrant("a".into(), false), None)
                .unwrap();
            assert!(matches!(
                engine.accept_launch(&thread(&parent, "a"), inherit(Runtime::Codex)),
                Ok(Accepted::Ask(_))
            ));
            assert!(matches!(
                engine.accept_launch(&thread(&worker, "w"), inherit(Runtime::Codex)),
                Ok(Accepted::Ask(_))
            ));
            assert_eq!(
                engine.workers.get(&worker, "w").unwrap().origin.started,
                None
            );
            assert!(
                engine
                    .workers
                    .get(&worker, "w")
                    .unwrap()
                    .origin
                    .yolo
                    .is_some()
            );
            // The operator's own /worker needs the YOLO confirmation instead.
            let operator = Caller::Operator {
                scope: parent.clone(),
                parent: "a".into(),
                device: "client-1".into(),
            };
            engine.running.insert(
                parent.clone(),
                live_with(
                    &parent,
                    vec![view(&parent, "a", "idle", "yolo")],
                    vec![("a", timeline(&[], false))],
                    vec![],
                )
                .0,
            );
            assert!(
                engine
                    .accept_launch(&operator, inherit(Runtime::Codex))
                    .err()
                    .unwrap()
                    .contains("Confirm YOLO")
            );
            let mut confirmed = inherit(Runtime::Codex);
            confirmed.yolo_confirmed = true;
            let o = launched(engine.accept_launch(&operator, confirmed));
            assert!(matches!(o.yolo, Some(YoloAllowance::Operator { .. })));
        }

        #[test]
        fn a_host_restart_declines_a_pending_yolo_request() {
            let root = tempfile::tempdir().unwrap();
            let (parent, worker, _) = routes(root.path());
            let job = {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                with_parent(&mut engine, &parent, "bypass_permissions");
                engine
                    .running
                    .insert(worker.clone(), live(&worker, vec![], &[], vec![]).0);
                let (done, result) = std::sync::mpsc::sync_channel(1);
                // A Claude parent in bypass: the parent scope here is Codex, so use the table directly.
                let mut s = inherit(Runtime::Codex);
                s.permission = None;
                engine.running.insert(
                    parent.clone(),
                    live(
                        &parent,
                        vec![view(&parent, "a", "running", "yolo")],
                        &["a"],
                        vec![],
                    )
                    .0,
                );
                engine
                    .launch(thread(&parent, "a"), s, Replier::Channel(done))
                    .unwrap();
                result.try_recv().unwrap().unwrap()["job_id"]
                    .as_u64()
                    .unwrap()
            };
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            assert!(engine.asking.is_empty() && engine.ledger.data.asking.is_empty());
            with_parent(&mut engine, &parent, "yolo");
            let status = engine
                .parent_call(&thread(&parent, "a"), ParentCall::Status { job_id: job })
                .unwrap();
            assert_eq!(status["status"], "declined");
            assert!(status["error"].as_str().unwrap().contains("restarted"));
            assert!(
                !engine.running.contains_key(&worker),
                "nothing is relaunched"
            );
        }

        /// A live parent asks for a YOLO worker; the operator has not answered yet.
        fn waiting_yolo_request(engine: &mut Engine, parent: &Scope, worker: &Scope) -> u64 {
            with_parent(engine, parent, "yolo");
            engine
                .running
                .insert(worker.clone(), live(worker, vec![], &[], vec![]).0);
            let (done, result) = std::sync::mpsc::sync_channel(1);
            let mut s = inherit(Runtime::Codex);
            s.yolo_confirmed = true;
            engine
                .launch(thread(parent, "a"), s, Replier::Channel(done))
                .unwrap();
            let waiting = result.try_recv().unwrap().unwrap();
            assert_eq!(waiting["status"], "waiting_for_operator");
            let job = waiting["job_id"].as_u64().unwrap();
            assert!(engine.asking.contains_key(&job));
            job
        }

        #[test]
        fn ending_the_parent_withdraws_its_worker_yolo_request_and_a_late_answer_is_refused() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            let job = waiting_yolo_request(&mut engine, &parent, &worker);
            // The fixture store has no live process to end, so run the step that follows the end.
            engine.end_children(&parent, "a").unwrap();
            assert!(!engine.asking.contains_key(&job), "ask withdrawn");
            assert!(engine.ledger.data.asking.is_empty(), "ledger cleared");
            let outcome = engine
                .ledger
                .data
                .outcomes
                .iter()
                .find(|o| o.origin.job_id == job)
                .expect("a cancelled outcome");
            assert_eq!(outcome.status, "cancelled");
            assert_eq!(outcome.message, ENDED_PARENT);
            // A late answer cannot start the worker.
            assert!(engine.answer_yolo(7, &parent, job, 0).is_err());
            assert!(
                engine.running[&worker].launching.is_none(),
                "nothing started"
            );
        }

        #[test]
        fn a_yolo_answer_is_refused_once_its_parent_session_is_no_longer_live() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            let job = waiting_yolo_request(&mut engine, &parent, &worker);
            // The parent's route goes away without the end path running.
            engine.running.remove(&parent);
            let refused = engine.answer_yolo(7, &parent, job, 0).unwrap_err();
            assert_eq!(refused.to_string(), ENDED_PARENT);
            assert!(
                !engine.asking.contains_key(&job),
                "the stale ask is withdrawn"
            );
            assert!(
                engine.running[&worker].launching.is_none(),
                "nothing started"
            );
        }

        /// Wait for the off-thread context window lookup, then advance the handoff.
        fn settle(engine: &mut Engine, target: &Scope) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while engine.running[target].launching.is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
                engine.advance_flows().unwrap();
            }
        }
        fn claude_source(root: &Path) -> Scope {
            Scope {
                project: root.canonicalize().unwrap(),
                route: Runtime::Claude.route(None),
            }
        }

        #[test]
        fn a_handoff_is_refused_while_the_source_is_busy_and_names_what_is_pending() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let source = claude_source(root.path());
            let handoff = |request: &str| {
                Action::StartHandoff(HandoffStart {
                    source: "s".into(),
                    runtime: Runtime::Codex,
                    model: Some("fixture".into()),
                    mode: Default::default(),
                    request: request.into(),
                    yolo_confirmed: false,
                })
            };
            engine.running.insert(
                source.clone(),
                live_with(
                    &source,
                    vec![view(&source, "s", "running", "accept_edits")],
                    vec![("s", timeline(&[("u1", "user", "hi")], true))],
                    vec![],
                )
                .0,
            );
            let err = engine
                .act(1, "h1", &source, handoff("next"), None)
                .unwrap_err();
            assert!(err.to_string().contains("running a turn"), "{err}");
            let ask = AskView {
                command: None,
                id: RequestId::Number(4),
                token: "t".into(),
                thread: Some("s".into()),
                turn: Some("t1".into()),
                item: None,
                choice_details: vec![None],
                title: "Run cargo test".into(),
                detail: String::new(),
                choices: vec!["Allow".into()],
                questions: vec![],
            };
            engine.running.insert(
                source.clone(),
                live_with(
                    &source,
                    vec![view(&source, "s", "needs_input", "accept_edits")],
                    vec![("s", timeline(&[("u1", "user", "hi")], false))],
                    vec![ask],
                )
                .0,
            );
            let err = engine
                .act(1, "h2", &source, handoff("next"), None)
                .unwrap_err();
            assert!(
                err.to_string()
                    .contains("1 pending request: Run cargo test"),
                "{err}"
            );
            assert!(engine.preparing.is_empty());
            assert!(engine.ledger.data.handoffs.is_empty(), "nothing started");
        }

        #[test]
        fn a_handoff_carries_real_messages_stamps_references_and_fails_on_restart() {
            let root = tempfile::tempdir().unwrap();
            let source = claude_source(root.path());
            let (_, target) = scopes(root.path());
            let items = [
                ("u1", "user", "first request"),
                ("c1", "tool", "cargo test"),
                ("a1", "assistant", "tests pass"),
                ("r1", "thinking", "private reasoning"),
                ("u2", "user", "second request"),
                ("a2", "assistant", "partial repl"),
            ];
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                engine.running.insert(
                    source.clone(),
                    live_with(
                        &source,
                        vec![view(&source, "s", "idle", "accept_edits")],
                        vec![("s", timeline(&items, false))],
                        vec![],
                    )
                    .0,
                );
                let (l, log) = live(&target, vec![], &[], vec![]);
                engine.running.insert(target.clone(), l);
                // A window a stock client reported earlier on the target route sets the budget.
                engine
                    .ledger
                    .change(|d| {
                        crate::durable::record_window(
                            &mut d.windows,
                            crate::durable::Window {
                                route: target.route.clone(),
                                model: Some("fixture".into()),
                                tokens: 300_000,
                            },
                        )
                    })
                    .unwrap();
                engine
                    .act(
                        1,
                        "h1",
                        &source,
                        Action::StartHandoff(HandoffStart {
                            source: "s".into(),
                            runtime: Runtime::Codex,
                            model: Some("fixture".into()),
                            mode: Default::default(),
                            request: "now do this".into(),
                            yolo_confirmed: false,
                        }),
                        None,
                    )
                    .unwrap();
                engine.process(&source, vec![], true).unwrap();
                settle(&mut engine, &target);
                let l = engine.running[&target]
                    .launching
                    .as_ref()
                    .expect("target opening");
                let o = &l.origin;
                assert_eq!(o.relation, ox_desk_core::origin::Relation::Handoff);
                assert_eq!(
                    o.started,
                    Some(Access::Workspace),
                    "Accept edits maps to Workspace"
                );
                let h = o.handoff.as_ref().unwrap();
                assert_eq!((h.session.as_str(), h.runtime), ("s", Runtime::Claude));
                assert_eq!(h.selection.items, ["u1", "c1", "a1", "u2", "a2"]);
                assert_eq!(h.selection.omitted, 0);
                assert_eq!(
                    (h.selection.window, h.selection.window_known),
                    (300_000, true)
                );
                assert!(matches!(
                    log.lock().unwrap()[0].0,
                    Action::SetNextAccess(Access::Workspace)
                ));
                // The stand-in runtime confirms the preset, as a native store does.
                let confirm = LinkEvent::Host(Box::new(Server::Delta {
                    cursor: Cursor {
                        epoch: "e".into(),
                        seq: 3,
                    },
                    scope: target.clone(),
                    changes: vec![Change::State {
                        connection: "ready".into(),
                        capabilities: vec!["start".into()],
                        detail: json!({"next_access": "workspace"}),
                    }],
                }));
                engine.process(&target, vec![confirm], false).unwrap();
                // The target received the real messages, in order, then the request.
                let sent = log.lock().unwrap().clone();
                let (text, carried) = sent
                    .iter()
                    .find_map(|(a, _)| match a {
                        Action::InjectHandoff(text, items) => Some((text.clone(), items.clone())),
                        _ => None,
                    })
                    .expect("the first turn was sent");
                assert_eq!(text, "now do this");
                let texts: Vec<(String, String)> = carried
                    .iter()
                    .skip(1)
                    .map(|i| {
                        (
                            i["role"].as_str().unwrap().into(),
                            i["content"][0]["text"].as_str().unwrap().into(),
                        )
                    })
                    .collect();
                assert_eq!(
                    texts,
                    [
                        ("user", "first request"),
                        ("assistant", "[Command `cargo test` exited 0]"),
                        ("assistant", "tests pass"),
                        ("user", "second request"),
                        ("assistant", "partial repl"),
                    ]
                    .map(|(a, b)| (a.to_owned(), b.to_owned()))
                );
                // The ledger keeps references, never message text.
                let saved = std::fs::read_to_string(root.path().join("ownership-v1.json")).unwrap();
                assert!(saved.contains("\"starting\"") && saved.contains("\"u2\""));
                for text in [
                    "first request",
                    "tests pass",
                    "now do this",
                    "private reasoning",
                ] {
                    assert!(!saved.contains(text), "{text} is not stored");
                }
            }
            // The host restarts before the target's first turn completes: failed, never retried.
            let (tx, rx) = std::sync::mpsc::sync_channel(64);
            let engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            assert!(engine.running.is_empty());
            assert!(rx.try_recv().is_err());
            let h = &engine.ledger.data.handoffs[0];
            assert_eq!(h.status, "failed");
            assert!(
                h.message
                    .as_deref()
                    .unwrap()
                    .contains("Nothing was retried")
            );
        }

        #[test]
        fn a_same_runtime_route_change_is_a_handoff_with_a_claude_context_block() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let source = claude_source(root.path());
            let target = Scope {
                route: Runtime::Claude.route(Some("fixture".into())),
                ..source.clone()
            };
            let items = [("u1", "user", "first request"), ("a1", "assistant", "done")];
            engine.running.insert(
                source.clone(),
                live_with(
                    &source,
                    vec![view(&source, "s", "idle", "auto")],
                    vec![("s", timeline(&items, false))],
                    vec![],
                )
                .0,
            );
            let (l, log) = live(&target, vec![], &[], vec![]);
            engine.running.insert(target.clone(), l);
            engine
                .act(
                    1,
                    "h1",
                    &source,
                    Action::StartHandoff(HandoffStart {
                        source: "s".into(),
                        runtime: Runtime::Claude,
                        model: Some("fixture".into()),
                        mode: Default::default(),
                        request: "continue on the API".into(),
                        yolo_confirmed: false,
                    }),
                    None,
                )
                .unwrap();
            engine.process(&source, vec![], true).unwrap();
            settle(&mut engine, &target);
            let o = &engine.running[&target].launching.as_ref().unwrap().origin;
            // Claude Auto is a classifier mode and is inherited without asking.
            assert_eq!(o.started, Some(Access::Auto));
            assert!(o.yolo.is_none());
            let confirm = LinkEvent::Host(Box::new(Server::Delta {
                cursor: Cursor {
                    epoch: "e".into(),
                    seq: 3,
                },
                scope: target.clone(),
                changes: vec![Change::State {
                    connection: "ready".into(),
                    capabilities: vec!["start".into()],
                    detail: json!({"next_access": "auto"}),
                }],
            }));
            engine.process(&target, vec![confirm], false).unwrap();
            let (text, carried) = log
                .lock()
                .unwrap()
                .iter()
                .find_map(|(a, _)| match a {
                    Action::InjectHandoff(t, i) => Some((t.clone(), i.clone())),
                    _ => None,
                })
                .expect("first turn");
            assert!(carried.is_empty(), "Claude Code has no history injection");
            let (block, request) = ox_desk_core::handoff::split_claude_turn(&text).unwrap();
            assert_eq!(request, "continue on the API");
            assert!(
                block.contains("first request")
                    && block.contains("<message role=\"assistant\" id=\"a1\">")
            );
        }

        #[test]
        fn a_codex_handoff_is_reported_only_after_its_history_and_first_turn_are_accepted() {
            let root = tempfile::tempdir().unwrap();
            let (_, target) = scopes(root.path());
            let later = Instant::now() + Duration::from_secs(60);
            // The runtime refused thread/inject_items: the transcript carries the error.
            let mut refused = timeline(&[], false);
            refused.detail["error"] = json!("thread/inject_items: invalid item");
            let (l, _) = live_with(
                &target,
                vec![view(&target, "t", "idle", "workspace")],
                vec![("t", refused)],
                vec![],
            );
            let err = first_turn(&l.store, "t", later).unwrap().unwrap_err();
            assert!(err.contains("invalid item"), "{err}");
            // Nothing in flight and no error: the first turn was accepted.
            let (l, _) = live_with(
                &target,
                vec![view(&target, "t", "running", "workspace")],
                vec![("t", timeline(&[], true))],
                vec![],
            );
            assert_eq!(first_turn(&l.store, "t", later), Some(Ok(())));
        }

        #[test]
        fn read_ox_thread_reads_only_the_callers_own_chain_in_its_project() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let source = claude_source(root.path());
            let (_, target) = scopes(root.path());
            let back = Scope {
                route: Runtime::Claude.route(Some("other".into())),
                ..source.clone()
            };
            engine.running.insert(
                source.clone(),
                live_with(
                    &source,
                    vec![view(&source, "s", "idle", "default")],
                    vec![(
                        "s",
                        timeline(
                            &[("u1", "user", "first"), ("a1", "assistant", "long answer")],
                            false,
                        ),
                    )],
                    vec![],
                )
                .0,
            );
            engine.running.insert(
                target.clone(),
                live_with(
                    &target,
                    vec![
                        view(&target, "t", "running", "workspace"),
                        view(&target, "x", "running", "workspace"),
                    ],
                    vec![
                        ("t", timeline(&[("v1", "user", "codex turn")], true)),
                        ("x", timeline(&[], true)),
                    ],
                    vec![],
                )
                .0,
            );
            engine.running.insert(
                back.clone(),
                live_with(
                    &back,
                    vec![view(&back, "b", "running", "default")],
                    vec![("b", timeline(&[], true))],
                    vec![],
                )
                .0,
            );
            let stamp = |from: &Scope, session: &str, project: &Path| {
                let mut o = origin(workers_parent(from, session), Ceiling::Write);
                o.relation = ox_desk_core::origin::Relation::Handoff;
                o.handoff = Some(Box::new(ox_desk_core::handoff::HandoffSource {
                    scope: Scope {
                        project: project.to_owned(),
                        route: from.route.clone(),
                    },
                    session: session.into(),
                    native: None,
                    title: session.into(),
                    runtime: from.route.runtime,
                    route: from.route.clone(),
                    source_access: None,
                    source_level: None,
                    decision: ox_desk_core::modes::Decision::Inherited,
                    selection: Default::default(),
                }));
                o
            };
            let project = source.project.clone();
            engine
                .workers
                .stamp(&target, "t", stamp(&source, "s", &project));
            engine
                .workers
                .stamp(&back, "b", stamp(&target, "t", &project));
            let read =
                |engine: &mut Engine, caller: Caller, session: Option<&str>, item: Option<&str>| {
                    let (done, result) = std::sync::mpsc::sync_channel(1);
                    engine
                        .read_thread(
                            &caller,
                            ParentCall::ReadThread {
                                session: session.map(Into::into),
                                item: item.map(Into::into),
                                after: None,
                                limit: None,
                            },
                            Replier::Channel(done),
                        )
                        .unwrap();
                    result.try_recv().expect("answered at once")
                };
            // The Codex target reads an omitted item of its Claude source in full.
            let page = read(&mut engine, thread(&target, "t"), None, Some("a1")).unwrap();
            assert_eq!(page["items"][0]["text"], "long answer");
            // The second handoff reads the whole chain, back to the first source.
            let page = read(&mut engine, thread(&back, "b"), Some("s"), None).unwrap();
            assert_eq!(page["items"][0]["text"], "first");
            let page = read(&mut engine, thread(&back, "b"), None, None).unwrap();
            assert_eq!(page["items"][0]["text"], "codex turn");
            // A sibling session on the same app-server has no chain; the source itself has none.
            assert!(
                read(&mut engine, thread(&target, "x"), None, None)
                    .unwrap_err()
                    .contains("no source")
            );
            assert!(
                read(&mut engine, thread(&target, "t"), Some("x"), None)
                    .unwrap_err()
                    .contains("not in this session's handoff chain")
            );
            // A source outside the caller's project is refused.
            engine
                .workers
                .stamp(&target, "x", stamp(&source, "s", Path::new("/elsewhere")));
            assert!(
                read(&mut engine, thread(&target, "x"), None, None)
                    .unwrap_err()
                    .contains("outside")
            );
            // A terminal launch is not a hosted session.
            let terminal = Caller::External(ExternalCaller {
                project: project.clone(),
                runtime: Runtime::Codex,
                session: Some("t".into()),
                launch: "l".into(),
            });
            assert!(read(&mut engine, terminal, None, None).is_err());
        }

        /// A Live whose store is a real local Codex store on an inert stdio process.
        fn local_live(scope: &Scope) -> Live {
            let link =
                codex::Link::from_command(std::process::Command::new("/bin/cat"), |_| {}).unwrap();
            let store = Store::open(
                ox_desk_core::store::Profile {
                    project: scope.project.clone(),
                    route: scope.route.clone(),
                    provider: "openai".into(),
                },
                link,
            )
            .unwrap();
            Live {
                state: store.host_snapshot(scope),
                store,
                launching: None,
                fork_origin: None,
                confirming: None,
                opening: None,
                terminal: None,
                schema_started: true,
                recovered: vec![],
                inbox: Inbox::new(|| {}),
                generation: 1,
                tokens: HashMap::new(),
                tracker: Tracker::new(),
                held_handoff: None,
            }
        }

        #[test]
        fn ending_a_busy_session_needs_force_and_an_idle_one_does_not() {
            let root = tempfile::tempdir().unwrap();
            let (scope, _) = scopes(root.path());
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (running, log) = live(
                &scope,
                vec![view(&scope, "a", "running", "workspace")],
                &["a"],
                vec![],
            );
            engine.running.insert(scope.clone(), running);
            let refused = engine
                .act(1, "e1", &scope, Action::EndSession("a".into()), None)
                .unwrap_err();
            assert!(refused.to_string().contains("busy"), "{refused}");
            assert!(
                !log.lock()
                    .unwrap()
                    .iter()
                    .any(|(a, _)| matches!(a, Action::EndSession(_))),
                "a refused end reaches no runtime"
            );
            // Forced, it passes the guard and only then meets the missing live process.
            let forced = engine
                .act(1, "e2", &scope, Action::ForceEndSession("a".into()), None)
                .unwrap_err();
            assert!(forced.to_string().contains("no live process"), "{forced}");
        }

        #[test]
        fn a_parent_stays_running_while_a_worker_it_started_is_busy() {
            let root = tempfile::tempdir().unwrap();
            let (parent, worker) = scopes(root.path());
            let running_of_parent = |worker_turn: &[&str]| {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                engine.running.insert(
                    parent.clone(),
                    live(
                        &parent,
                        vec![view(&parent, "a", "idle", "workspace")],
                        &[],
                        vec![],
                    )
                    .0,
                );
                engine.running.insert(
                    worker.clone(),
                    live(
                        &worker,
                        vec![view(&worker, "w", "running", "read_only")],
                        worker_turn,
                        vec![],
                    )
                    .0,
                );
                engine.workers.stamp(
                    &worker,
                    "w",
                    origin(workers_parent(&parent, "a"), Ceiling::Read),
                );
                let live = &engine.running[&parent];
                let rows = engine.scope_rows(&parent, live, vec!["a".into()]);
                rows.first().expect("parent row").running
            };
            // The parent's own turn is done, but its worker is mid-turn on another scope.
            assert!(running_of_parent(&["w"]));
            // Both idle: the parent is not running.
            assert!(!running_of_parent(&[]));
        }

        #[test]
        fn the_reaper_decision_waits_for_the_whole_timeout_and_never_ends_busy_work() {
            let timeout = Duration::from_secs(30 * 60);
            assert!(!reapable(Duration::from_secs(29 * 60), timeout, false));
            assert!(reapable(timeout, timeout, false));
            assert!(reapable(timeout * 2, timeout, false));
            assert!(!reapable(timeout * 2, timeout, true));
            assert!(
                !reapable(timeout * 2, Duration::ZERO, false),
                "zero disables"
            );
            assert_eq!(idle_timeout(None), timeout);
            assert_eq!(idle_timeout(Some("5")), Duration::from_secs(300));
            assert_eq!(idle_timeout(Some(" 0 ")), Duration::ZERO);
            assert_eq!(idle_timeout(Some("soon")), timeout);
        }

        #[test]
        fn an_empty_codex_scope_is_dropped_and_stays_listed_for_a_later_subscriber() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            engine.running.insert(scope.clone(), local_live(&scope));
            assert!(engine.processes().is_empty(), "no thread, no process row");
            // A thread that is not live cannot be ended.
            assert!(engine.end_thread(&scope, "gone", false).is_err());
            // A held handoff keeps the scope open until its subscriber takes it.
            engine.running.get_mut(&scope).unwrap().held_handoff = Some((
                "k".into(),
                Server::Handoff {
                    argv: vec![],
                    id: "k".into(),
                    scope: scope.clone(),
                    thread: None,
                    command: String::new(),
                },
                Instant::now() + Duration::from_secs(600),
            ));
            engine.reap_empty(&scope).unwrap();
            assert!(engine.running.contains_key(&scope));
            engine.running.get_mut(&scope).unwrap().held_handoff = None;
            engine.reap_empty(&scope).unwrap();
            assert!(!engine.running.contains_key(&scope), "app-server dropped");
            assert_eq!(engine.cold[&scope].connection, "disconnected");
        }

        #[test]
        fn idle_reaping_ends_nothing_while_unused_time_is_short() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            engine.running.insert(scope.clone(), local_live(&scope));
            engine.idle = Duration::from_secs(60);
            engine.reap_idle(Instant::now()).unwrap();
            assert!(engine.running.contains_key(&scope));
            // An empty scope idle past the timeout is closed.
            engine
                .reap_idle(Instant::now() + Duration::from_secs(61))
                .unwrap();
            assert!(!engine.running.contains_key(&scope));
        }

        #[test]
        fn a_handoff_held_for_a_gone_client_reaches_the_next_desktop_subscriber_only() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            let mut live = local_live(&scope);
            live.held_handoff = Some((
                "k".into(),
                Server::Handoff {
                    argv: vec!["codex".into()],
                    id: "k".into(),
                    scope: scope.clone(),
                    thread: Some("t".into()),
                    command: String::new(),
                },
                Instant::now() + Duration::from_secs(600),
            ));
            engine.running.insert(scope.clone(), live);
            let client = |terminal| {
                let (peer, frames) = crate::transport::test_peer();
                (
                    ClientState {
                        peer,
                        hello: true,
                        prepared: None,
                        subscription: None,
                        catalog: false,
                        terminal,
                    },
                    frames,
                )
            };
            let sent = |frames: &std::sync::mpsc::Receiver<crate::transport::Packet>| {
                frames
                    .try_iter()
                    .map(|p| String::from_utf8_lossy(&p).into_owned())
                    .collect::<Vec<_>>()
            };
            // A phone, push or operator subscriber of the scope never takes it.
            let (phone, phone_frames) = client(false);
            engine.clients.insert(8, phone);
            engine.subscribe(8, Some(scope.clone()), None).unwrap();
            assert!(
                !sent(&phone_frames).iter().any(|f| f.contains("\"handoff\"")),
                "a non-desktop subscriber received the terminal handoff"
            );
            assert!(engine.running[&scope].held_handoff.is_some());
            let (desktop, frames) = client(true);
            engine.clients.insert(9, desktop);
            engine.subscribe(9, Some(scope.clone()), None).unwrap();
            let sent = sent(&frames);
            assert!(sent.iter().any(|f| f.contains("\"handoff\"")), "{sent:?}");
            assert!(engine.running[&scope].held_handoff.is_none());
        }

        #[test]
        fn an_expired_held_handoff_is_dropped_and_its_scope_reaped() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            let mut live = local_live(&scope);
            live.held_handoff = Some((
                "k".into(),
                Server::Handoff {
                    argv: vec![],
                    id: "k".into(),
                    scope: scope.clone(),
                    thread: None,
                    command: String::new(),
                },
                Instant::now() + HELD_HANDOFF_TTL,
            ));
            engine.running.insert(scope.clone(), live);
            // A scope-less subscriber never receives it.
            let (peer, frames) = crate::transport::test_peer();
            engine.clients.insert(
                9,
                ClientState {
                    peer,
                    hello: true,
                    prepared: None,
                    subscription: None,
                    catalog: false,
                    terminal: false,
                },
            );
            engine.subscribe(9, None, None).unwrap();
            assert!(
                !frames
                    .try_iter()
                    .any(|p| String::from_utf8_lossy(&p).contains("\"handoff\""))
            );
            assert!(engine.running[&scope].held_handoff.is_some());
            engine.idle = Duration::from_secs(3600);
            engine.reap_idle(Instant::now()).unwrap();
            assert!(engine.running.contains_key(&scope), "still held");
            engine
                .reap_idle(Instant::now() + HELD_HANDOFF_TTL + Duration::from_secs(1))
                .unwrap();
            assert!(!engine.running.contains_key(&scope), "expired and reaped");
        }

        #[test]
        fn cancelling_a_worker_that_is_still_launching_aborts_the_launch() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let rows = vec![view(&parent, "a", "running", "workspace")];
            engine
                .running
                .insert(parent.clone(), live(&parent, rows, &["a"], vec![]).0);
            let mut starting = local_live(&worker);
            let (reply, answered) = std::sync::mpsc::sync_channel(1);
            starting.launching = Some(Launching {
                origin: origin(workers_parent(&parent, "a"), Ceiling::Read),
                payload: Payload::Worker(start(false)),
                reply: Replier::Channel(reply),
                stage: Stage::Opening,
                deadline: Instant::now() + Duration::from_secs(60),
                detached: false,
            });
            engine.running.insert(worker.clone(), starting);
            let status = engine
                .parent_call(&hosted(&parent), ParentCall::Status { job_id: 41 })
                .unwrap();
            assert_eq!(status["status"], "starting");
            let done = engine
                .parent_call(&hosted(&parent), ParentCall::Cancel { job_id: 41 })
                .unwrap();
            assert_eq!(done["status"], "cancelled");
            let told = answered.try_recv().expect("the launcher is answered");
            assert!(told.unwrap_err().contains("cancelled"));
            assert!(
                !engine.running.contains_key(&worker),
                "the unused worker runtime is closed"
            );
            assert!(
                engine
                    .unstarted_jobs(&crate::workers::Resolved::Hosted {
                        scope: parent.clone(),
                        session: "a".into(),
                    })
                    .is_empty()
            );
        }

        #[test]
        fn a_launch_whose_message_is_already_sent_cannot_be_cancelled() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let mut sent = local_live(&worker);
            let (reply, _answered) = std::sync::mpsc::sync_channel(1);
            sent.launching = Some(Launching {
                origin: origin(workers_parent(&parent, "a"), Ceiling::Read),
                payload: Payload::Worker(start(false)),
                reply: Replier::Channel(reply),
                stage: Stage::Sent,
                deadline: Instant::now() + Duration::from_secs(60),
                detached: false,
            });
            engine.running.insert(worker.clone(), sent);
            assert!(engine.abort_launch(41).unwrap().is_none());
            assert!(engine.running[&worker].launching.is_some());
        }
    }
}
