//! A Claude billing route owns a catalog and an independent stock process for each
//! open session. The session key and launch generation accompany every callback.
mod events;
mod history;
mod native;

use super::{Connection, Profile, Store, ThreadRow, catalog::now_secs};
use crate::{
    changed::Changed,
    claude::{
        self as wire,
        catalog::{Bindings, Origin, Profiles},
    },
    links::{
        self, LinkEvent,
        claude::{self as pipe, Session},
    },
    runtime::{Access, Billing, RequestId, Runtime},
    transcript::item::{Content, Item},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    fmt,
    fs::File,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    thread,
};

pub const SUBSCRIPTION_PROVIDER: &str = "claude-subscription";
pub const API_PROVIDER: &str = "claude-ox-api";

pub struct Launcher {
    pub ox: PathBuf,
    pub path: OsString,
    pub profiles: Profiles,
    pub index: PathBuf,
    pub aliases: Vec<String>,
}

pub struct Project {
    launcher: Arc<Launcher>,
    deliver: Arc<dyn Fn(LinkEvent) + Send + Sync>,
    sessions: HashMap<String, Process>,
    catalog: HashMap<String, ThreadRow>,
    asks: HashMap<RequestId, (String, wire::Permission)>,
    blank: Option<String>,
    /// The worker contract the next new session starts with; one launch consumes it.
    worker: Option<crate::claude_worker_contract::Contract>,
    /// The process started under that contract for one worker launch. It is kept apart
    /// from `blank`, so no other send can take it, and no worker can take a normal blank.
    worker_blank: Option<String>,
    serial: u64,
    native: native::Native,
}
#[derive(Default)]
struct Process {
    generation: u64,
    events: events::EventState,
    handoff: Option<super::commands::Handoff>,
    link: Option<pipe::Link>,
    initialized: bool,
    /// The first `system/init` matched the project. Later ones report the Bash
    /// tool's current directory, which follows `cd`, so only the first is checked.
    project_checked: bool,
    capabilities: Option<crate::slash::Capabilities>,
    mode: Option<Access>,
    controls: HashMap<String, Control>,
    first: Option<(String, String)>,
    /// The sender statement of `first`, fixed when the host accepted that message. It
    /// waits with the message while the process starts, and goes out with it.
    first_context: Option<String>,
    /// The permission mode this process was launched with.
    #[cfg_attr(not(test), allow(dead_code))]
    launch_mode: String,
    /// Read-only view of a journal a stock client is writing right now: no process.
    viewer: Option<native::Viewer>,
}
enum Control {
    Mode(Access),
    Model(String),
    Interrupt,
}

pub struct Update {
    owner: String,
    generation: u64,
    data: Data,
}
impl fmt::Debug for Update {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeUpdate")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
enum Data {
    Connected(pipe::Link),
    Stock(pipe::Event),
    History(history::Batch),
    /// Appended journal rows of a read-only viewer, to apply in place.
    Tail(history::Tail),
    /// A viewer's read finished; the follower resumes from where it stopped.
    Viewed(Option<Box<history::Follower>>),
    Failed(String),
}
impl Update {
    pub(crate) fn unblocks(&self) -> bool {
        !matches!(
            self.data,
            Data::History(_)
                | Data::Tail(_)
                | Data::Stock(pipe::Event::Frame(
                    wire::Event::Delta { .. } | wire::Event::Item { .. }
                ))
        )
    }
}

fn new_id() -> io::Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let n = u128::from_be_bytes(bytes);
    Ok(format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        n >> 96,
        (n >> 80) & 0xffff,
        (n >> 64) & 0xffff,
        (n >> 48) & 0xffff,
        n & 0xffffffffffff
    ))
}
fn session_key(billing: &Billing, id: &str) -> String {
    let scope = match billing {
        Billing::Subscription => "subscription",
        Billing::Api(_) => "api",
    };
    format!("claude:{scope}:{id}")
}
fn stock_id(key: &str) -> io::Result<&str> {
    key.split(':')
        .nth(2)
        .filter(|id| wire::catalog::valid_session_id(id))
        .ok_or_else(|| io::Error::other("Invalid Claude session ID"))
}
fn child_key(root: &str, tool: &str) -> String {
    format!("{root}/{tool}")
}
fn row(profile: &Profile, id: String, title: String, updated_at: i64) -> ThreadRow {
    ThreadRow {
        id,
        name: None,
        generated_title: None,
        preview: title,
        cwd: profile.project.clone(),
        model_provider: profile.provider.clone(),
        model: match &profile.route.billing {
            Billing::Api(alias) => Some(alias.clone()),
            Billing::Subscription => None,
        },
        agent_nickname: None,
        updated_at,
        parent_thread_id: None,
        worker: false,
        unread: false,
        status: None,
        activity_order: 0,
    }
}
/// Read only session metadata in both native profiles. A client is spawned when a user opens a session.
pub fn saved_rows(profiles: &Profiles, index: &std::path::Path) -> io::Result<Vec<ThreadRow>> {
    let aliases = Bindings::new(index.into()).aliases(&profiles.api)?;
    Ok(profiles
        .all()?
        .into_iter()
        .map(|s| saved_row(s, &aliases))
        .collect())
}

fn saved_row(
    saved: wire::catalog::Session,
    aliases: &HashMap<(PathBuf, String), String>,
) -> ThreadRow {
    let api = saved.origin == Origin::Api;
    let billing = if api {
        Billing::Api(String::new())
    } else {
        Billing::Subscription
    };
    let id = session_key(&billing, &saved.id);
    let model = if api {
        aliases
            .get(&(saved.project.clone(), saved.id))
            .cloned()
            .or(saved.model)
    } else {
        None
    };
    let profile = Profile {
        project: saved.project,
        route: Runtime::Claude.route(None),
        provider: if api {
            API_PROVIDER
        } else {
            SUBSCRIPTION_PROVIDER
        }
        .into(),
    };
    let mut result = row(&profile, id, saved.preview, saved.updated_at);
    result.name = saved.native_title;
    result.model = model;
    result
}

impl Launcher {
    fn catalog(&self, profile: &Profile) -> io::Result<HashMap<String, ThreadRow>> {
        let aliases = Bindings::new(self.index.clone()).aliases(&self.profiles.api)?;
        let mut rows = HashMap::new();
        for saved in self.profiles.list(&profile.project)? {
            // A UUID in both profiles belongs to the explicitly selected billing route.
            let saved = if saved.origin == Origin::Ambiguous {
                self.profiles.session(
                    &profile.project,
                    &saved.id,
                    if profile.route.is_api() {
                        Origin::Api
                    } else {
                        Origin::Subscription
                    },
                )?
            } else {
                saved
            };
            let row = saved_row(saved, &aliases);
            rows.insert(row.id.clone(), row);
        }
        Ok(rows)
    }
}

impl ThreadRow {
    pub fn saved_claude_api(&self) -> Option<bool> {
        match self.model_provider.as_str() {
            SUBSCRIPTION_PROVIDER => Some(false),
            API_PROVIDER => Some(true),
            _ => None,
        }
    }

    pub fn saved_runtime_route(&self, aliases: &[String]) -> Option<crate::runtime::Route> {
        match self.model_provider.as_str() {
            SUBSCRIPTION_PROVIDER => Some(Runtime::Claude.route(None)),
            API_PROVIDER => self
                .model
                .as_ref()
                .filter(|model| aliases.contains(model))
                .map(|alias| Runtime::Claude.route(Some(alias.clone()))),
            _ => None,
        }
    }
    pub fn needs_claude_alias(&self) -> bool {
        self.model_provider == API_PROVIDER && self.model.is_none()
    }
}
impl Access {
    fn mode(self) -> Option<&'static str> {
        match self {
            Self::Default => Some("default"),
            Self::Auto => Some("auto"),
            Self::Plan => Some("plan"),
            Self::AcceptEdits => Some("acceptEdits"),
            Self::BypassPermissions => Some("bypassPermissions"),
            _ => None,
        }
    }
    fn from_mode(name: &str) -> Option<Self> {
        Self::choices(Runtime::Claude)
            .iter()
            .copied()
            .find(|a| a.mode() == Some(name))
    }
}
/// A launch's dedicated worker process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerBlank {
    None,
    Starting,
    Ready,
    /// The process failed to start or exited before the first send.
    Lost,
}
impl Store {
    pub fn is_session_scoped(&self) -> bool {
        if self.remote().is_some() {
            return self.profile.route.runtime == Runtime::Claude;
        }
        matches!(self.link, links::Link::Claude(_))
    }
    pub(super) fn prepare_claude_draft(&mut self) -> io::Result<()> {
        if self.claude().blank.is_none() {
            let owner = self.spawn_claude(Session::New(new_id()?), None, false)?;
            self.claude_mut().blank = Some(owner);
        }
        Ok(())
    }
    /// A session-scoped Claude route whose blank draft was consumed by a send and
    /// whose next one is not prepared yet. Choosing the next access prepares it, so a
    /// launch can proceed from here instead of waiting for a draft nobody will start.
    pub fn claude_blank_missing(&self) -> bool {
        self.state == Connection::Ready
            && matches!(self.link, links::Link::Claude(_))
            && self.claude().blank.is_none()
    }
    /// Start a dedicated process under the shared worker contract for one launch.
    /// The warm blank is left alone: only `send_on_worker_blank` ever uses this process,
    /// so a send from any other session keeps getting a normal draft.
    pub fn stage_claude_worker(
        &mut self,
        contract: crate::claude_worker_contract::Contract,
    ) -> io::Result<()> {
        if !matches!(self.link, links::Link::Claude(_)) {
            return Ok(());
        }
        self.drop_claude_worker();
        let preset = self.next_preset.take();
        self.claude_mut().worker = Some(contract);
        let spawned = self.spawn_claude(Session::New(new_id()?), None, false);
        self.next_preset = preset;
        let owner = spawned?;
        self.claude_mut().worker_blank = Some(owner);
        Ok(())
    }
    /// Where the worker's dedicated process stands.
    pub fn claude_worker_state(&self) -> WorkerBlank {
        if !matches!(self.link, links::Link::Claude(_)) {
            return WorkerBlank::None;
        }
        let p = self.claude();
        match p.worker_blank.as_deref() {
            None => WorkerBlank::None,
            Some(k) => match p.sessions.get(k) {
                None => WorkerBlank::Lost,
                Some(s) if s.initialized && s.link.is_some() => WorkerBlank::Ready,
                Some(_) => WorkerBlank::Starting,
            },
        }
    }
    /// Send a launch's first turn on its dedicated process and nowhere else.
    pub fn send_on_worker_blank(
        &mut self,
        action: crate::actions::Action,
        sender: crate::surface::Sender,
    ) -> io::Result<()> {
        let Some(owner) = self.claude_mut().worker_blank.take() else {
            return Err(io::Error::other("The worker's runtime is not prepared"));
        };
        let normal = self.claude_mut().blank.replace(owner);
        let result = self.dispatch_as(action, sender);
        // The send consumed the dedicated process; the normal draft stays as it was.
        let p = self.claude_mut();
        p.blank = normal;
        result.map(|_| ())
    }
    /// Close a dedicated process its launch did not use.
    pub fn drop_claude_worker(&mut self) {
        if let links::Link::Claude(p) = &mut self.link {
            p.worker = None;
            if let Some(owner) = p.worker_blank.take() {
                p.sessions.remove(&owner);
            }
        }
    }
    pub(super) fn claude_applied_access(&self, id: Option<&str>) -> Option<Access> {
        let project = self.claude();
        let owner = id.or(project.blank.as_deref())?;
        let session = project.sessions.get(owner)?;
        session.link.as_ref()?;
        session.mode
    }
    pub(super) fn claude_can_submit(&self, id: Option<&str>) -> bool {
        let project = self.claude();
        let owner = id.or(project.blank.as_deref());
        owner
            .and_then(|k| project.sessions.get(k))
            .is_some_and(|p| p.initialized && p.link.is_some() && p.handoff.is_none())
            && !self.claude_settings_pending(id)
    }
    pub(super) fn claude_settings_pending(&self, id: Option<&str>) -> bool {
        let p = self.claude();
        id.or(p.blank.as_deref())
            .and_then(|k| p.sessions.get(k))
            .is_some_and(|s| {
                s.controls
                    .values()
                    .any(|c| matches!(c, Control::Mode(_) | Control::Model(_)))
            })
    }
    pub(super) fn handing_off_claude(&self) -> bool {
        self.is_session_scoped() && self.claude().sessions.values().any(|p| p.handoff.is_some())
    }
    pub(super) fn release_claude(
        &mut self,
        id: &str,
        handoff: super::commands::Handoff,
    ) -> io::Result<()> {
        let p = self
            .claude_mut()
            .sessions
            .get_mut(id)
            .ok_or_else(|| io::Error::other("Open the Claude session before handing it off"))?;
        p.handoff = Some(handoff);
        p.link.take();
        Ok(())
    }
    fn finish_claude_handoff(&mut self, id: &str) -> bool {
        let Some(handoff) = self
            .claude_mut()
            .sessions
            .get_mut(id)
            .and_then(|p| p.handoff.take())
        else {
            return false;
        };
        self.claude_mut().sessions.remove(id);
        self.handoff_ready = Some(handoff);
        self.changed.mark(Changed::NOTICE);
        true
    }
    pub(super) fn claude_palette(&self, query: &str) -> Vec<ThreadRow> {
        let q = query.to_lowercase();
        self.claude()
            .catalog
            .values()
            .filter_map(|row| {
                let mut row = row.clone();
                self.catalog.titles.decorate(&mut row);
                (row.title().to_lowercase().contains(&q) || row.preview.to_lowercase().contains(&q))
                    .then_some(row)
            })
            .collect()
    }
    pub(super) fn claude_next_mode(&mut self, mode: Access) -> io::Result<bool> {
        if let Some(id) = self.claude().blank.clone() {
            self.claude_mode(&id, mode)?;
            return Ok(true);
        }
        // A reconnect can show a new local composer after the original host draft
        // was consumed. Prepare the explicit mode so stock can confirm it.
        self.next_preset = Some(mode);
        self.prepare_claude_draft()?;
        Ok(true)
    }
    pub(super) fn claude_caps(&self, owner: Option<&str>) -> crate::slash::Capabilities {
        let p = self.claude();
        owner
            .or(p.blank.as_deref())
            .and_then(|k| p.sessions.get(k.split('/').next()?))
            .and_then(|s| s.capabilities.clone())
            .unwrap_or_else(|| self.commands.schema.clone().unwrap_or_default())
    }
    pub fn open_claude(
        profile: Profile,
        launcher: Launcher,
        deliver: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let catalog = launcher.catalog(&profile)?;
        let project = Project {
            launcher: Arc::new(launcher),
            deliver: Arc::new(deliver),
            catalog,
            sessions: Default::default(),
            asks: Default::default(),
            blank: None,
            worker: None,
            worker_blank: None,
            serial: 0,
            native: Default::default(),
        };
        let mut store = Self::with_link(profile, links::Link::Claude(Box::new(project)));
        store.claude_list();
        let id = new_id()?;
        let owner = store.spawn_claude(Session::New(id), None, false)?;
        store.claude_mut().blank = Some(owner);
        Ok(store)
    }
    fn claude(&self) -> &Project {
        match &self.link {
            links::Link::Claude(p) => p,
            _ => unreachable!("Claude integration"),
        }
    }
    fn claude_mut(&mut self) -> &mut Project {
        match &mut self.link {
            links::Link::Claude(p) => p,
            _ => unreachable!("Claude integration"),
        }
    }
    pub(super) fn claude_list(&mut self) {
        self.catalog.clear();
        let mut rows = self.claude_palette(&self.query);
        rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
        for row in rows {
            self.catalog.insert(row);
        }
        self.catalog_revision += 1;
        self.changed.mark(Changed::THREADS);
    }
    fn remember_claude(&mut self, row: ThreadRow) {
        self.claude_mut()
            .catalog
            .insert(row.id.clone(), row.clone());
        self.record_row(row);
    }
    fn spawn_claude(
        &mut self,
        session: Session,
        first: Option<(String, String)>,
        confirm_missing_alias: bool,
    ) -> io::Result<String> {
        self.spawn_claude_in(session, first, confirm_missing_alias, None)
    }
    /// `resume_mode` starts a resumed session in a chosen access mode; a new session
    /// takes its preset and any other resume starts in `default`.
    fn spawn_claude_in(
        &mut self,
        session: Session,
        first: Option<(String, String)>,
        confirm_missing_alias: bool,
        resume_mode: Option<Access>,
    ) -> io::Result<String> {
        let id = match &session {
            Session::New(id) | Session::Resume(id) | Session::Fork { id, .. } => id,
        }
        .clone();
        let owner = session_key(&self.profile.route.billing, &id);
        if self.claude().sessions.contains_key(&owner) {
            return Ok(owner);
        }
        // The journal as OX found it: only a write after this, by a client that is not
        // OX's headless one, names a foreign owner.
        let baseline = stock_id(&owner).ok().and_then(|raw| {
            let origin = if self.profile.route.is_api() {
                Origin::Api
            } else {
                Origin::Subscription
            };
            self.claude()
                .launcher
                .profiles
                .modified(&self.profile.project, raw, origin)
                .ok()
        });
        self.claude_mut()
            .native
            .ox_seen
            .insert(owner.clone(), baseline);
        let profile = self.profile.clone();
        // A draft preset belongs only to the new session it was chosen for.
        let preset = match &session {
            Session::New(_) => self.next_preset.take(),
            Session::Resume(_) => resume_mode,
            Session::Fork { .. } => None,
        };
        // A worker keeps its launch contract on every relaunch: a resume reuses its own,
        // a fork the contract of its source.
        let contract = match &session {
            Session::New(_) => self.claude_mut().worker.take(),
            Session::Resume(_) => self.worker_contracts.get(&owner).copied(),
            Session::Fork { source, .. } => self
                .worker_contracts
                .get(&session_key(&self.profile.route.billing, source))
                .copied(),
        };
        if let Some(c) = contract {
            self.worker_contracts.insert(owner.clone(), c);
        }
        // A new worker starts in the contract's mode. A resumed one keeps the access the
        // user chose since; without a recorded choice it starts in the contract's mode.
        let mode = match (&session, contract) {
            (Session::New(_), Some(c)) => Some(c.permission_mode().to_owned()),
            _ => preset.and_then(Access::mode).map(str::to_owned),
        }
        .or_else(|| contract.map(|c| c.permission_mode().to_owned()))
        .unwrap_or_else(|| "default".to_owned());
        let first_context = first.as_ref().and_then(|_| self.sender_context());
        let p = self.claude_mut();
        p.serial += 1;
        let generation = p.serial;
        let launcher = p.launcher.clone();
        let receive = p.deliver.clone();
        let key = owner.clone();
        p.sessions.insert(
            owner.clone(),
            Process {
                generation,
                first,
                first_context,
                launch_mode: mode.clone(),
                ..Default::default()
            },
        );
        let alive = p.sessions[&owner].events.lifetime();
        let history_owner = owner.clone();
        thread::spawn(move || {
            let emit = move |data| {
                receive(LinkEvent::Claude(Update {
                    owner: key.clone(),
                    generation,
                    data,
                }))
            };
            let emit = Arc::new(emit);
            let start = || -> io::Result<()> {
                let index = Bindings::new(launcher.index.clone());
                let source = match &session {
                    Session::Resume(id) => Some(id),
                    Session::Fork { source, .. } => Some(source),
                    _ => None,
                };
                if let Some(source) = source {
                    let origin = if profile.route.is_api() {
                        Origin::Api
                    } else {
                        Origin::Subscription
                    };
                    let saved = launcher
                        .profiles
                        .session(&profile.project, source, origin)?;
                    if let Billing::Api(alias) = &profile.route.billing {
                        let bound = index
                            .alias(&profile.project, &launcher.profiles.api, source)?
                            .or(saved.model.clone());
                        match bound.as_deref() {
                            Some(bound) if bound == alias => (),
                            None if confirm_missing_alias => (),
                            _ => {
                                return Err(io::Error::other(
                                    "This session was saved with a different model alias. Choose the saved alias in the sidebar to resume it.",
                                ));
                            }
                        }
                        index.bind(&profile.project, &launcher.profiles.api, source, alias)?;
                    }
                    history::load(&profile, &history_owner, &saved, &alive, |batch| {
                        emit(Data::History(batch))
                    })?;
                }
                if let Billing::Api(alias) = &profile.route.billing {
                    index.bind(&profile.project, &launcher.profiles.api, &id, alias)?
                }
                let output = emit.clone();
                let link = pipe::Link::spawn(
                    pipe::Launch {
                        ox: launcher.ox.clone(),
                        path: launcher.path.clone(),
                        project: profile.project,
                        billing: profile.route.billing,
                        session,
                        permission_mode: mode,
                        contract,
                    },
                    move |event| output(Data::Stock(event)),
                )?;
                emit(Data::Connected(link));
                Ok(())
            };
            if let Err(e) = start() {
                emit(Data::Failed(e.to_string()));
            }
        });
        Ok(owner)
    }
    pub(super) fn claude_open(&mut self, id: &str) -> io::Result<()> {
        self.claude_open_alias(id, false)
    }
    pub(super) fn claude_open_alias(&mut self, id: &str, confirm: bool) -> io::Result<()> {
        if id.contains('/') {
            return Ok(());
        }
        let raw = stock_id(id)?.to_owned();
        if self.native_live_now(id) {
            return self.view_native(id);
        }
        self.transcripts.entry(id.into()).or_default().loading = true;
        self.spawn_claude(Session::Resume(raw), None, confirm)?;
        Ok(())
    }
    /// Resume an ended Claude session with the draft as its first input, so the message
    /// goes out as soon as the process is initialised.
    pub(super) fn claude_resume_send(&mut self, id: &str) -> io::Result<()> {
        let row = self
            .catalog
            .get(id)
            .ok_or_else(|| io::Error::other("Thread is outside this catalog"))?
            .clone();
        self.check_route(&row)?;
        if self.native_live_now(id) {
            return Err(io::Error::other(native::HINT));
        }
        let text = self.draft(Some(id)).to_owned();
        self.restored_drafts.remove(id);
        let raw = stock_id(id)?.to_owned();
        // A send continues the conversation the user was having, so its first turn runs
        // in the access mode they had chosen, also after the host reaped the idle route
        // behind their back. Opening the session from the list is a fresh start and keeps
        // `default`, as does a session that never had a chosen mode.
        let mode = self.resume_access.get(id).copied();
        self.spawn_claude_in(Session::Resume(raw), Some((new_id()?, text)), false, mode)?;
        self.ended_threads.remove(id);
        let t = self.transcripts.entry(id.into()).or_default();
        t.loading = true;
        t.error = None;
        self.selected = Some(id.into());
        self.selection += 1;
        self.edit(Some(id), String::new());
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS | Changed::CONNECTION);
        Ok(())
    }
    pub(super) fn claude_submit(
        &mut self,
        id: Option<&str>,
        command: Option<&str>,
    ) -> io::Result<()> {
        if !self.can_send(id) || id.is_some_and(|s| s.contains('/')) {
            return Err(io::Error::other(
                "Open a ready parent Claude session to send",
            ));
        }
        let text = command.unwrap_or_else(|| self.draft(id)).to_owned();
        if text.trim().is_empty() {
            return Err(io::Error::other("Enter a message"));
        }
        if !self.draft_images(id).is_empty() {
            return Err(io::Error::other(
                "Remove the attachment before submitting to Claude Code",
            ));
        }
        if let Some(id) = id {
            self.restored_drafts.remove(id);
        }
        let new_conversation = id.is_none() && command.is_none();
        let message = new_id()?;
        let owner = match id {
            Some(id) => id.to_owned(),
            None => match self.claude_mut().blank.take() {
                Some(id) => id,
                None => self.spawn_claude(Session::New(new_id()?), None, false)?,
            },
        };
        // A handoff's first turn titles the session from its request, not its block.
        let visible = if new_conversation && self.claude_handoff {
            crate::handoff::split_claude_turn(&text).map_or(text.as_str(), |(_, r)| r)
        } else {
            text.as_str()
        }
        .to_owned();
        if new_conversation {
            self.first_title_prompt(&owner, &visible);
        }
        if !self.claude().catalog.contains_key(&owner) {
            let new = row(
                &self.profile,
                owner.clone(),
                visible
                    .trim()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect(),
                now_secs(),
            );
            self.remember_claude(new);
        }
        self.selected = Some(owner.clone());
        self.selection += 1;
        let context = self.sender_context();
        let process = self
            .claude_mut()
            .sessions
            .get_mut(&owner)
            .ok_or_else(|| io::Error::other("Reconnect the session before sending"))?;
        process.first = Some((message, text));
        process.first_context = context;
        self.flush_claude(&owner)?;
        if id.is_none() {
            self.host_opened = Some(owner.clone());
        }
        if command.is_none() {
            self.edit(id, String::new());
        }
        Ok(())
    }
    fn flush_claude(&mut self, owner: &str) -> io::Result<()> {
        let Some(process) = self.claude_mut().sessions.get_mut(owner) else {
            return Ok(());
        };
        let (Some(_), true) = (&process.link, process.initialized) else {
            return Ok(());
        };
        let transcript = self.transcripts.entry(owner.into()).or_default();
        transcript.loading = false;
        transcript.ready = true;
        transcript.verified = true;
        let process = self.claude_mut().sessions.get_mut(owner).unwrap();
        let Some((id, text)) = process.first.as_ref() else {
            return Ok(());
        };
        let link = process.link.as_ref().unwrap();
        let content = vec![json!({"type":"text","text":text})];
        match process.first_context.as_deref() {
            Some(context) => link.send_user_with_context(id, content, context),
            None => link.send_user(id, content),
        }?;
        let (id, text) = process.first.take().unwrap();
        process.first_context = None;
        process.events.sent(&id, &text);
        let transcript = self.transcripts.get_mut(owner).unwrap();
        let turn = transcript
            .active_turn
            .get_or_insert_with(|| id.clone())
            .clone();
        transcript.observe_item(
            turn,
            id,
            Item::UserMessage {
                content: vec![Content {
                    text: Some(text),
                    path: None,
                }],
            },
            true,
            None,
        );
        self.changed.transcript(owner);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }
    pub(super) fn claude_stop(&mut self, id: &str) -> io::Result<()> {
        let process = self
            .claude_mut()
            .sessions
            .get_mut(id)
            .ok_or_else(|| io::Error::other("No active Claude session"))?;
        let link = process
            .link
            .as_ref()
            .ok_or_else(|| io::Error::other("Waiting for Claude Code"))?;
        let request = link.interrupt()?;
        process.controls.insert(request, Control::Interrupt);
        if let Some(t) = self.transcripts.get_mut(id)
            && let Some(turn) = t.active_turn.clone()
        {
            t.stop_requested = Some(turn);
        }
        Ok(())
    }
    /// Drop a session's process, which closes its link and kills the process
    /// group. The thread stays in the catalog; selecting it resumes it.
    pub(super) fn claude_end(&mut self, id: &str) -> io::Result<()> {
        let project = self.claude();
        if project.blank.as_deref() == Some(id) || !project.sessions.contains_key(id) {
            return Err(io::Error::other("No active Claude session"));
        }
        if project.sessions[id].viewer.is_some() {
            return Err(io::Error::other(
                "Another client is running this session; OX only follows it",
            ));
        }
        if project.sessions[id].handoff.is_some() {
            return Err(io::Error::other("This session is opening in a terminal"));
        }
        let mut p = self
            .claude_mut()
            .sessions
            .remove(id)
            .expect("checked above");
        if let Some(mode) = p.mode.filter(|_| p.link.is_some()) {
            self.resume_access.insert(id.into(), mode);
        }
        let mut unsent = p.events.take_awaiting_echo();
        unsent.extend(p.first.take());
        drop(p);
        self.restore_claude_drafts(id, unsent);
        let removed: std::collections::HashSet<_> = self
            .claude()
            .asks
            .iter()
            .filter(|(_, v)| v.0 == id)
            .map(|(k, _)| k.clone())
            .collect();
        self.asks.retain(|a| !removed.contains(&a.id));
        self.claude_mut().asks.retain(|k, _| !removed.contains(k));
        for (key, t) in &mut self.transcripts {
            if key == id || key.strip_prefix(id).is_some_and(|s| s.starts_with('/')) {
                t.verified = false;
                t.ready = false;
                t.loading = false;
                t.active_turn = None;
                t.error = None;
                self.changed.transcript(key);
            }
        }
        self.changed
            .mark(Changed::CONNECTION | Changed::ASK | Changed::THREADS);
        Ok(())
    }
    pub(super) fn claude_has_live(&self, id: &str) -> bool {
        let project = self.claude();
        project.blank.as_deref() != Some(id)
            && project.worker_blank.as_deref() != Some(id)
            && project.sessions.get(id).is_some_and(|p| p.viewer.is_none())
    }
    pub(super) fn claude_is_blank(&self, id: &str) -> bool {
        self.claude().blank.as_deref() == Some(id)
    }
    /// Every OS process this store owns: the live sessions, and for Claude the warm blank
    /// draft too. `live_threads` leaves the blank out because it is not a session to end
    /// or reap, but the process list still has to show the process the host keeps.
    pub fn process_threads(&self) -> Vec<String> {
        let mut ids = self.live_threads();
        if self.remote().is_none()
            && self.is_session_scoped()
            && let Some(blank) = self.claude().blank.as_deref()
            && self
                .claude()
                .sessions
                .get(blank)
                .is_some_and(|p| p.viewer.is_none() && (p.link.is_some() || !p.initialized))
        {
            ids.push(blank.to_owned());
        }
        ids
    }
    pub(super) fn claude_live(&self) -> Vec<String> {
        let project = self.claude();
        let mut ids: Vec<String> = project
            .sessions
            .iter()
            .filter(|(k, p)| {
                project.blank.as_deref() != Some(k.as_str())
                    && project.worker_blank.as_deref() != Some(k.as_str())
                    && p.viewer.is_none()
            })
            .map(|(k, _)| k.clone())
            .collect();
        ids.sort();
        ids
    }
    pub(super) fn claude_mode(&mut self, id: &str, mode: Access) -> io::Result<()> {
        let process = self
            .claude_mut()
            .sessions
            .get_mut(id)
            .ok_or_else(|| io::Error::other("Open a Claude session first"))?;
        let link = process
            .link
            .as_ref()
            .ok_or_else(|| io::Error::other("Waiting for Claude Code"))?;
        let request = link.set_mode(
            mode.mode()
                .ok_or_else(|| io::Error::other("Unsupported permission mode"))?,
        )?;
        process.controls.insert(request, Control::Mode(mode));
        Ok(())
    }
    pub(super) fn claude_choose(
        &mut self,
        id: Option<&str>,
        choice: super::routes_thread::Choice,
    ) -> io::Result<()> {
        let super::routes_thread::Choice::Model(model) = choice else {
            return Err(io::Error::other("Unsupported Claude session setting"));
        };
        let owner = id
            .or(self.claude().blank.as_deref())
            .ok_or_else(|| io::Error::other("Open a Claude session first"))?
            .to_owned();
        if self.profile.route.is_api() {
            return Err(io::Error::other(
                "Start a new Claude session to change its billing alias",
            ));
        }
        let p = self
            .claude_mut()
            .sessions
            .get_mut(&owner)
            .ok_or_else(|| io::Error::other("Claude session has closed"))?;
        let link = p
            .link
            .as_ref()
            .ok_or_else(|| io::Error::other("Waiting for Claude Code"))?;
        let request = link.set_model(&model)?;
        p.controls.insert(request, Control::Model(model));
        Ok(())
    }
    pub(super) fn claude_fork(&mut self, id: &str) -> io::Result<()> {
        let source = stock_id(id)?.to_owned();
        let owner = self.spawn_claude(
            Session::Fork {
                source,
                id: new_id()?,
            },
            None,
            false,
        )?;
        let title = self
            .catalog
            .get(id)
            .map_or("Claude session", ThreadRow::title);
        self.remember_claude(row(
            &self.profile,
            owner.clone(),
            format!("Fork of {title}"),
            now_secs(),
        ));
        self.selected = Some(owner.clone());
        self.selection += 1;
        self.host_opened = Some(owner.clone());
        self.transcripts.entry(owner).or_default().loading = true;
        Ok(())
    }
    pub(super) fn claude_response(&mut self, id: &RequestId, answer: Value) -> io::Result<()> {
        let (owner, ask) = self
            .claude()
            .asks
            .get(id)
            .ok_or_else(|| io::Error::other("Claude request has expired"))?;
        self.claude()
            .sessions
            .get(owner)
            .and_then(|p| p.link.as_ref())
            .ok_or_else(|| io::Error::other("Claude session has closed"))?
            .respond(&ask.request_id, answer)?;
        self.claude_mut().asks.remove(id);
        Ok(())
    }
    pub(super) fn claude_questions(&mut self, id: &RequestId, values: &[String]) -> io::Result<()> {
        let (_, ask) = self
            .claude()
            .asks
            .get(id)
            .ok_or_else(|| io::Error::other("Claude request has expired"))?;
        if values.len() != ask.questions.len() {
            return Err(io::Error::other("Answer every question"));
        }
        let values = ask
            .questions
            .iter()
            .zip(values)
            .map(|(q, v)| (q.id.clone(), v.clone()))
            .collect();
        let answer = ask.answer(&values)?;
        self.claude_response(id, answer)
    }
    pub fn terminal_session_id<'a>(&self, id: &'a str) -> io::Result<&'a str> {
        if self.profile.route.runtime == Runtime::Claude {
            stock_id(id)
        } else {
            Ok(id)
        }
    }
}
