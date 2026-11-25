//! In-memory presentation state for a subscribed client. Native I/O exists only
//! on the publishing store. The caller provides one ordered, nonblocking writer.
use super::*;
use crate::{
    actions::{Action, Effect},
    asks::Decision,
    host_protocol::{Change, ScopeState, Server, SessionView, TimelineItem},
    runtime::{Access, RequestId, Route},
    slash::Capabilities,
    transcript::{DeltaTarget, item::Item},
};
use serde::de::DeserializeOwned;
use serde_json::Value;
type Writer = Box<dyn Fn(Action, Option<String>) -> io::Result<String> + Send + Sync>;
pub struct Remote {
    writer: Writer,
    pub(super) scope_caps: Vec<String>,
    caps: HashMap<String, Vec<String>>,
    tokens: HashMap<RequestId, String>,
    pub(super) routes: HashMap<String, Route>,
    pub(super) access: HashMap<String, Access>,
    pub(super) changing: HashSet<String>,
    pub(super) unconfirmed_options: HashMap<String, String>,
    pub(super) commands: HashMap<String, Capabilities>,
    pub(super) sending: HashSet<String>,
    pub(super) schema: bool,
    pub(super) options: HashMap<String, routes_thread::Options>,
    pub(super) draft_access: Option<Access>,
    pending: HashMap<String, (Option<String>, Option<compose::Draft>)>,
    pub(super) origins: HashMap<String, crate::origin::Origin>,
    /// Host marks on a session: handoffs from it and its YOLO grant.
    pub(super) marks: HashMap<String, Value>,
    pub(super) await_new: bool,
}
impl Remote {
    pub(super) fn can(&self, id: &str, cap: &str) -> bool {
        self.caps
            .get(id)
            .is_some_and(|c| c.iter().any(|v| v == cap))
    }
}
fn bits<T: DeserializeOwned>(value: &Value) -> Option<T> {
    serde_json::from_value(value.clone()).ok()
}
pub fn row(view: &SessionView) -> Option<ThreadRow> {
    let mut row: ThreadRow = bits(&view.detail["row"])?;
    row.worker = view.detail["worker"] == true;
    row.generated_title = bits(&view.detail["generated"]);
    row.unread = view.detail["unread"] == true;
    row.activity_order = view.detail["order"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    Some(row)
}
impl Store {
    pub fn remote_client(
        profile: Profile,
        send: impl Fn(Action, Option<String>) -> io::Result<String> + Send + Sync + 'static,
    ) -> Self {
        let remote = Remote {
            writer: Box::new(send),
            scope_caps: vec![],
            caps: HashMap::new(),
            tokens: HashMap::new(),
            routes: HashMap::new(),
            access: HashMap::new(),
            commands: HashMap::new(),
            changing: HashSet::new(),
            unconfirmed_options: HashMap::new(),
            sending: HashSet::new(),
            schema: false,
            draft_access: None,
            options: HashMap::new(),
            await_new: false,
            pending: HashMap::new(),
            origins: HashMap::new(),
            marks: HashMap::new(),
        };
        Self::with_link(profile, Link::Host(Box::new(remote)))
    }
    /// The host's immutable lineage stamp for a worker session or its fork.
    pub fn origin(&self, id: &str) -> Option<&crate::origin::Origin> {
        self.remote()?.origins.get(id)
    }
    /// A host mark on a session: `handed_off_to`, `handoff_attempts`,
    /// `yolo_grant` or `yolo_children`.
    pub fn session_mark(&self, id: &str, key: &str) -> Option<&Value> {
        self.remote()?
            .marks
            .get(id)
            .and_then(|m| m.get(key))
            .filter(|v| !v.is_null())
    }
    pub fn is_host_client(&self) -> bool {
        self.remote().is_some()
    }
    pub(crate) fn remote(&self) -> Option<&Remote> {
        if let Link::Host(r) = &self.link {
            Some(r)
        } else {
            None
        }
    }
    fn remote_mut(&mut self) -> &mut Remote {
        match &mut self.link {
            Link::Host(r) => r,
            _ => unreachable!("remote state requires a host subscription"),
        }
    }
    /// A local confirmation must still refer to the exact incarnation the user reviewed.
    pub fn command_review_token(&self, id: &RequestId) -> Option<&str> {
        self.remote()?.tokens.get(id).map(String::as_str)
    }
    pub fn decline_reviewed_command(&self, id: &RequestId, token: &str) -> io::Result<String> {
        let decline = self
            .asks()
            .iter()
            .find(|ask| &ask.id == id)
            .and_then(|ask| ask.command.as_ref())
            .filter(|_| self.command_review_token(id) == Some(token))
            .ok_or_else(|| {
                io::Error::other("The command request changed or was already answered.")
            })?
            .decline;
        (self.remote().expect("verified host token").writer)(
            Action::AnswerAsk(id.clone(), decline),
            Some(token.to_owned()),
        )
    }
    pub(crate) fn forward(&self, action: Action) -> io::Result<()> {
        let r = self
            .remote()
            .ok_or_else(|| io::Error::other("No host connection"))?;
        let token = match &action {
            Action::AnswerAsk(id, _) | Action::AnswerQuestions(id, _) => Some(
                r.tokens
                    .get(id)
                    .cloned()
                    .ok_or_else(|| io::Error::other("This request has already been resolved"))?,
            ),
            _ => None,
        };
        (r.writer)(action, token).map(|_| ())
    }
    pub(super) fn pending_input(&self, owner: Option<&str>) -> bool {
        self.remote()
            .is_some_and(|r| r.pending.values().any(|(id, _)| id.as_deref() == owner))
    }
    fn send_input(
        &mut self,
        action: Action,
        owner: Option<String>,
        from_draft: bool,
    ) -> io::Result<()> {
        let id = (self.remote().unwrap().writer)(action, None)?;
        let draft = if from_draft {
            self.drafts.remove(&owner)
        } else {
            None
        };
        self.remote_mut().pending.insert(id, (owner, draft));
        Ok(())
    }
    fn remote_command(
        &mut self,
        owner: Option<String>,
        text: String,
        from_draft: bool,
    ) -> io::Result<Effect> {
        use crate::slash::{self, Action as Command};
        let caps = self.command_caps(owner.as_deref());
        match slash::parse(&text, &caps) {
            Some(Command::New) => {
                self.dispatch_remote(Action::NewDraft)?;
                if from_draft {
                    self.edit(owner.as_deref(), String::new());
                }
            }
            Some(Command::Resume) => {
                if from_draft {
                    self.edit(owner.as_deref(), String::new());
                }
                return Ok(Effect::FocusSearch);
            }
            Some(Command::Terminal) => self.forward(Action::TerminalText(owner, text))?,
            Some(Command::Hint(message)) => return Err(io::Error::other(message)),
            command => {
                let opens = owner.is_none() && command.is_some_and(|c| c.starts_thread());
                self.send_input(
                    Action::PaletteCommand(owner.clone(), text),
                    owner,
                    from_draft,
                )?;
                if opens {
                    self.remote_mut().await_new = true;
                }
            }
        }
        Ok(Effect::Applied)
    }
    fn select_replica(&mut self, id: &str) {
        self.selected = Some(id.to_owned());
        {
            let t = self.transcripts.entry(id.to_owned()).or_default();
            if !t.is_ready() {
                t.loading = true;
            }
        }
        self.changed.transcript(id);
    }
    pub(crate) fn dispatch_remote(&mut self, action: Action) -> io::Result<Effect> {
        match action {
            Action::DraftEdit(id, text) => self.edit(id.as_deref(), text),
            Action::ForkThread(_) => {
                self.forward(action)?;
                self.remote_mut().await_new = true;
            }
            Action::NewDraft => {
                self.selected = None;
                self.remote_mut().await_new = false;
                self.forward(Action::NewDraft)?;
            }
            Action::SelectThread(ref id) => {
                let row = self.catalog_row(id).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "Thread is outside this catalog")
                })?;
                self.check_route(row)?;
                let id = id.clone();
                self.forward(action)?;
                self.select_replica(&id);
                self.remote_mut().await_new = false;
            }
            Action::OpenCatalogRow(ref row) | Action::ConfirmAliasAndResume(ref row) => {
                if !row.worker {
                    self.check_route(row)?;
                }
                let id = row.id.clone();
                self.forward(action)?;
                self.select_replica(&id);
                self.remote_mut().await_new = false;
            }
            Action::Submit(owner) => {
                if !self.can_send(owner.as_deref()) || self.sending(owner.as_deref()) {
                    return Err(io::Error::other("Wait for the thread to be ready"));
                }
                if self.draft(owner.as_deref()).trim().is_empty()
                    && self.draft_images(owner.as_deref()).is_empty()
                {
                    return Err(io::Error::other("Enter a message"));
                }
                if let Some(warning) = self.image_model_warning(owner.as_deref()) {
                    return Err(io::Error::other(warning));
                }

                let text = self.draft(owner.as_deref()).to_string();
                let images = self.draft_images(owner.as_deref());
                let request = if images.is_empty() {
                    Action::SendText(owner.clone(), text)
                } else {
                    Action::SendImages(owner.clone(), text, images.to_vec())
                };
                let opens = owner.is_none();
                self.send_input(request, owner, true)?;
                if opens {
                    self.remote_mut().await_new = true;
                }
            }
            Action::ExecuteCommand(owner) => {
                let text = self.draft(owner.as_deref()).to_owned();
                return self.remote_command(owner, text, true);
            }
            Action::PaletteCommand(owner, text) => return self.remote_command(owner, text, false),
            Action::ModelOptions(_, routes_thread::Choice::Model(ref model))
                if !self.same_model_route(model) =>
            {
                return Err(io::Error::other(
                    "Open a new session on this model’s billing route.",
                ));
            }
            Action::ModelOptions(Some(ref id), _) => {
                let id = id.clone();
                let receipt = (self.remote().unwrap().writer)(action, None)?;
                self.remote_mut().unconfirmed_options.insert(receipt, id);
            }
            Action::Terminal(owner) => {
                let text = self.draft(owner.as_deref()).to_owned();
                self.forward(Action::TerminalText(owner, text))?;
            }
            Action::DismissNotice => {
                self.notice = None;
                self.forward(action)?;
            }
            Action::InjectHandoff(..) => {
                self.forward(action)?;
                self.remote_mut().await_new = true;
            }
            action => self.forward(action)?,
        }
        Ok(Effect::Applied)
    }
    pub(super) fn apply_host(&mut self, frame: Server) -> io::Result<()> {
        if self.remote().is_none() {
            return Err(io::Error::other("Unexpected client frame on runtime store"));
        }
        match frame {
            Server::Handoff {
                argv,
                scope,
                thread,
                command,
                ..
            } if scope.project == self.profile.project && scope.route == self.profile.route => {
                self.handoff_ready = Some(commands::Handoff {
                    thread,
                    command,
                    argv,
                });
                self.changed.mark(Changed::NOTICE);
            }
            Server::Opened { scope, thread, .. }
                if scope.project == self.profile.project && scope.route == self.profile.route =>
            {
                if self.remote().unwrap().await_new {
                    self.select_replica(&thread);
                    self.remote_mut().await_new = false;
                    self.changed.mark(Changed::THREADS);
                }
                self.remote_mut().sending.remove("");
            }
            Server::WorkerReply { result, error, .. } => {
                let handoff = result
                    .as_ref()
                    .filter(|r| r["relation"] == "handoff" && error.is_none())
                    .cloned();
                if let Some(r) = handoff {
                    let scope: Option<crate::host_protocol::Scope> =
                        serde_json::from_value(r["scope"].clone()).ok();
                    let session = r["session"].as_str().unwrap_or_default().to_owned();
                    let label = scope
                        .as_ref()
                        .map_or("another route".into(), |s| s.route.label());
                    self.notice = Some(format!("Continued in {label}."));
                    if scope.is_some_and(|s| {
                        s.project == self.profile.project && s.route == self.profile.route
                    }) && self.catalog_row(&session).is_some()
                    {
                        self.select_replica(&session);
                    }
                    self.changed.mark(Changed::NOTICE | Changed::THREADS);
                    return Ok(());
                }
                self.notice = Some(match (result, error) {
                    (_, Some(error)) => error,
                    (Some(status), None) if status["status"] == "waiting_for_operator" => format!(
                        "OX worker {} waits for you to accept YOLO on its parent session.",
                        status["job_id"]
                    ),
                    (Some(status), None) => format!(
                        "Started OX worker {} on {}.",
                        status["job_id"],
                        status["model"].as_str().unwrap_or("its route")
                    ),
                    (None, None) => "The worker request finished.".into(),
                });
                self.changed.mark(Changed::NOTICE);
            }
            Server::Snapshot { scopes, .. } => {
                for scope in scopes {
                    self.apply_scope(scope)?;
                }
            }
            Server::Delta { changes, .. } => {
                for change in changes {
                    self.apply_change(change)?;
                }
            }
            Server::Ack { id } => {
                if let Some(thread) = self.remote_mut().unconfirmed_options.remove(&id) {
                    self.changed.transcript(&thread);
                }
                if self.remote_mut().pending.remove(&id).is_some() {
                    self.changed.mark(Changed::NOTICE);
                }
            }
            Server::Error { id, message } => {
                if let Some(thread) = id
                    .as_ref()
                    .and_then(|id| self.remote_mut().unconfirmed_options.remove(id))
                {
                    self.changed.transcript(&thread);
                }
                if let Some((owner, Some(draft))) =
                    id.and_then(|id| self.remote_mut().pending.remove(&id))
                {
                    draft.restore_before(self.drafts.entry(owner).or_default());
                }
                self.notice = Some(message);
                self.remote_mut().await_new = false;
                self.changed.mark(Changed::NOTICE);
            }
            _ => (),
        }
        Ok(())
    }
    fn apply_scope(&mut self, s: ScopeState) -> io::Result<()> {
        self.transcripts.clear();
        self.apply_change(Change::State {
            connection: s.connection,
            capabilities: s.capabilities,
            detail: s.detail,
        })?;
        self.apply_change(Change::Sessions {
            sessions: s.sessions,
        })?;
        self.apply_change(Change::Asks { asks: s.asks })?;
        for (thread, timeline) in s.transcripts {
            self.apply_change(Change::Transcript { thread, timeline })?;
        }
        Ok(())
    }
    fn apply_change(&mut self, change: Change) -> io::Result<()> {
        match change {
            Change::State {
                connection,
                capabilities,
                detail,
            } => {
                self.state = match connection.as_str() {
                    "ready" => Connection::Ready,
                    "sign_in_required" => Connection::SignInRequired,
                    "closed" => Connection::Closed(bits(&detail["error"]).unwrap_or_default()),
                    _ => Connection::Connecting,
                };
                self.profile.provider = bits(&detail["provider"]).unwrap_or_default();
                if detail["next_sending"] == true {
                    self.remote_mut().sending.insert(String::new());
                } else {
                    self.remote_mut().sending.remove("");
                }
                self.next_options = bits(&detail["default_options"]);
                self.next_preset = bits(&detail["next_access"]);
                self.notice = bits(&detail["notice"]);
                self.commands.schema = bits(&detail["commands"]);
                self.commands.loading = detail["loading_commands"] == true;
                self.usage = bits(&detail["usage"]).unwrap_or_default();
                self.mention = bits(&detail["mention"]).unwrap_or_default();
                self.archived_threads = bits(&detail["archived"]).unwrap_or_default();
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|id| self.archived_threads.contains(id))
                {
                    self.selected = None;
                }
                self.cursor = (detail["more"] == true).then(String::new);
                self.palette_search.query = bits(&detail["palette_query"]).unwrap_or_default();
                self.palette_search.cursor = (detail["palette_more"] == true).then(String::new);
                self.palette_search.error = bits(&detail["palette_error"]);
                self.palette_search.rows.clear();
                for row in bits::<Vec<ThreadRow>>(&detail["palette_rows"]).unwrap_or_default() {
                    self.palette_search.rows.insert(row);
                }
                self.remote_mut().draft_access = bits(&detail["applied_draft_access"]);
                self.remote_mut().schema = detail["schema"] == true;
                self.remote_mut().scope_caps = capabilities;
                self.changed.mark(Changed::CONNECTION | Changed::NOTICE);
            }
            Change::Sessions { sessions } => {
                self.catalog.clear();
                self.terminal_threads.clear();
                let r = self.remote_mut();
                r.caps.clear();
                r.origins.clear();
                r.marks.clear();
                r.routes.clear();
                r.commands.clear();
                r.options.clear();
                r.access.clear();
                for session in sessions {
                    self.apply_session(session);
                }
                self.catalog_revision += 1;
                self.changed.mark(Changed::THREADS);
            }
            Change::Session { session } => {
                self.apply_session(session);
                self.catalog_revision += 1;
                self.changed.mark(Changed::THREADS);
            }
            Change::Asks { asks } => {
                self.remote_mut().tokens.clear();
                self.asks = asks
                    .into_iter()
                    .map(|v| {
                        self.remote_mut().tokens.insert(v.id.clone(), v.token);
                        Ask {
                            command: v.command,
                            id: v.id,
                            thread: v.thread,
                            turn: v.turn,
                            item: v.item,
                            title: v.title,
                            detail: v.detail,
                            questions: v.questions,
                            decisions: v
                                .choices
                                .into_iter()
                                .enumerate()
                                .map(|(index, label)| {
                                    let mut d = Decision::new(label, Value::Null);
                                    d.detail = v.choice_details.get(index).cloned().flatten();
                                    d
                                })
                                .collect(),
                        }
                    })
                    .collect();
                self.changed.mark(Changed::ASK);
            }
            Change::Transcript { thread, timeline } => {
                let t = self.transcripts.entry(thread.clone()).or_default();
                let old_len = t.len();
                *t = Transcript::default();
                for item in timeline.items {
                    apply_item(t, item)?;
                }
                t.host_history_replaced(old_len);
                apply_meta(t, timeline.detail);
                self.changed.transcript(&thread);
            }
            Change::TimelineState { thread, detail } => {
                apply_meta(self.transcripts.entry(thread.clone()).or_default(), detail);
                self.changed.transcript(&thread);
            }
            Change::Items { thread, items } => {
                let t = self.transcripts.entry(thread.clone()).or_default();
                for item in items {
                    apply_item(t, item)?;
                }
                self.changed.transcript(&thread);
            }
            Change::Append {
                thread,
                turn,
                id,
                text,
                revision,
            } => {
                let t = self.transcripts.entry(thread.clone()).or_default();
                let key = crate::transcript::ItemKey {
                    turn: turn.clone(),
                    item: id.clone(),
                };
                t.observe_delta(turn, id, DeltaTarget::Agent, text);
                t.host_revision(&key, revision);
                self.changed.transcript(&thread);
            }
            Change::Retired { thread } => {
                self.transcripts.remove(&thread);
                self.changed.transcript(&thread);
            }
            Change::Notification { .. } => (),
        }
        Ok(())
    }
    fn apply_session(&mut self, s: SessionView) {
        let id = s.id;
        let data = &s.detail;
        if let Some(mut row) = bits::<ThreadRow>(&data["row"]) {
            row.worker = data["worker"] == true;
            row.unread = data["unread"] == true;
            row.activity_order = data["order"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let generated = bits(&data["generated"]);
            self.record_lineage(&row);
            self.catalog.insert(row);
            if let Some(r) = self.catalog.get_mut(&id) {
                r.generated_title = generated;
            }
        }
        if data["terminal"] == true {
            self.terminal_threads.insert(id.clone());
        } else {
            self.terminal_threads.remove(&id);
        }
        let r = self.remote_mut();
        r.caps.insert(id.clone(), s.capabilities);
        if data["changing"] == true {
            r.changing.insert(id.clone());
        } else {
            r.changing.remove(&id);
        }
        if let Some(route) = bits(&data["route"]) {
            r.routes.insert(id.clone(), route);
        }
        if let Some(access) = bits(&data["access"]) {
            r.access.insert(id.clone(), access);
        }
        if let Some(opts) = bits(&data["options"]) {
            r.options.insert(id.clone(), opts);
        }
        if let Some(caps) = bits(&data["command_caps"]) {
            r.commands.insert(id.clone(), caps);
        }
        if data["sending"] == true {
            r.sending.insert(id.clone());
        } else {
            r.sending.remove(&id);
        }
        let marks: serde_json::Map<String, Value> = [
            "handed_off_to",
            "handoff_attempts",
            "yolo_grant",
            "yolo_children",
        ]
        .into_iter()
        .filter_map(|k| Some((k.to_owned(), data.get(k)?.clone())))
        .collect();
        if marks.is_empty() {
            r.marks.remove(&id);
        } else {
            r.marks.insert(id.clone(), Value::Object(marks));
        }
        match bits(&data["origin"]) {
            Some(origin) => {
                r.origins.insert(id, origin);
            }
            None => {
                r.origins.remove(&id);
            }
        }
    }
}
fn apply_meta(t: &mut Transcript, d: Value) {
    t.active_turn = bits(&d["active"]);
    t.last_finished = bits(&d["last_finished"]);
    t.ready = d["ready"] == true;
    t.verified = t.ready;
    t.loading = d["loading"] == true;
    t.error = bits(&d["error"]);
    t.cursor = (d["older"] == true).then(String::new);
    t.access = bits(&d["access"]);
    t.reviews = bits(&d["reviews"]).unwrap_or_default();
    t.usage = bits(&d["usage"]);
    t.command_notice = bits(&d["command_notice"]);
    t.model = bits(&d["model"]);
    t.effort = bits(&d["effort"]);
    t.tier = bits(&d["tier"]);
    t.unread = d["unread"] == true;
}
fn apply_item(t: &mut Transcript, i: TimelineItem) -> io::Result<()> {
    let item = match i.role.as_str() {
        "assistant" => Item::AgentMessage {
            text: i.text,
            is_update: i.detail["update"] == true,
        },
        "plan" => Item::Plan { text: i.text },
        "tool" => serde_json::from_value(i.detail["item"].clone())?,
        _ => serde_json::from_value(i.detail.clone())?,
    };
    let key = crate::transcript::ItemKey {
        turn: i.turn.clone(),
        item: i.id.clone(),
    };
    t.observe_item(i.turn, i.id, item, true, bits(&i.detail["output"]));
    t.host_revision(&key, i.revision);
    Ok(())
}

#[cfg(test)]
mod local_command_tests {
    use super::*;
    use crate::{
        asks::CommandApproval,
        host_protocol::{AskView, Change},
    };
    #[test]
    fn worker_replies_name_handoffs_and_yolo_waits() {
        let route = crate::runtime::Runtime::Claude.route(None);
        let mut store = Store::remote_client(
            Profile {
                project: "/p".into(),
                route: route.clone(),
                provider: "anthropic".into(),
            },
            |_, _| Ok("sent".into()),
        );
        let reply = |result: Value, error: Option<&str>| {
            LinkEvent::Host(Box::new(Server::WorkerReply {
                id: "k".into(),
                result: Some(result),
                error: error.map(Into::into),
            }))
        };
        let target = crate::runtime::Runtime::Codex.route(Some("fixture/demo".into()));
        store.apply(reply(
            serde_json::json!({"relation":"handoff","session":"t","scope":{"project":"/p","route":target}}),
            None,
        ));
        assert_eq!(store.notice(), Some("Continued in OX API · fixture/demo."));
        store.apply(reply(
            serde_json::json!({"status":"waiting_for_operator","job_id":9}),
            None,
        ));
        assert!(store.notice().unwrap().contains("accept YOLO"));
        store.apply(reply(
            serde_json::json!({}),
            Some("This session cannot be handed off yet"),
        ));
        assert_eq!(
            store.notice(),
            Some("This session cannot be handed off yet")
        );
    }
    #[test]
    fn terminal_handoff_sends_only_the_verified_stock_decline() {
        let project = PathBuf::from("/project");
        let id = RequestId::Number(7);
        let (tx, received) = std::sync::mpsc::channel();
        let mut store = Store::remote_client(
            Profile {
                project,
                route: Route::CODEX_SUBSCRIPTION,
                provider: "openai".into(),
            },
            move |action, token| {
                tx.send((action, token)).unwrap();
                Ok("my-local-receipt".into())
            },
        );
        let mut view = AskView {
            id: id.clone(),
            token: "new-ask".into(),
            thread: Some("t".into()),
            turn: None,
            item: None,
            choices: vec!["Cancel turn".into(), "Allow once".into(), "Decline".into()],
            choice_details: vec![],
            command: Some(CommandApproval {
                command: "sudo id".into(),
                cwd: Some("/tool-directory".into()),
                decline: 2,
            }),
            title: "Run command".into(),
            detail: String::new(),
            questions: vec![],
        };
        store
            .apply_change(Change::Asks {
                asks: vec![view.clone()],
            })
            .unwrap();
        assert!(store.decline_reviewed_command(&id, "stale-ask").is_err());
        assert!(received.try_recv().is_err());
        assert_eq!(
            store.decline_reviewed_command(&id, "new-ask").unwrap(),
            "my-local-receipt"
        );
        let (action, token) = received.try_recv().unwrap();
        assert!(matches!(action, Action::AnswerAsk(RequestId::Number(7), 2)));
        assert_eq!(token.as_deref(), Some("new-ask"));
        view.command = None;
        store
            .apply_change(Change::Asks { asks: vec![view] })
            .unwrap();
        assert!(store.decline_reviewed_command(&id, "new-ask").is_err());
        assert!(received.try_recv().is_err());
    }
}
