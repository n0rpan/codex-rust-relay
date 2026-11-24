//! Apply events only to the process generation and billing route that delivered them.
use super::*;
use crate::{slash::Capabilities, store::routes_thread::ModelInfo, transcript::DeltaTarget};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

#[derive(Default)]
pub(super) struct EventState {
    // Background tasks can finish after their spawning turn or during the next turn.
    pub(super) pending: HashMap<String, HashMap<String, String>>,
    alive: Arc<()>,
    awaiting_echo: Vec<(String, String)>,
    needs_sign_in: bool,
}
impl EventState {
    pub(super) fn lifetime(&self) -> std::sync::Weak<()> {
        Arc::downgrade(&self.alive)
    }
    // A successful queue write alone cannot attest that the CLI read the prompt.
    pub(super) fn sent(&mut self, id: &str, text: &str) {
        self.awaiting_echo.push((id.into(), text.into()));
    }
}
impl Store {
    pub(in crate::store) fn claude_event(&mut self, update: Update) -> io::Result<()> {
        let Update {
            owner,
            generation,
            data,
        } = update;
        if !self
            .claude()
            .sessions
            .get(&owner)
            .is_some_and(|p| p.generation == generation)
        {
            return Ok(());
        }
        if matches!(&data, Data::Stock(pipe::Event::Closed { .. }))
            && self.finish_claude_handoff(&owner)
        {
            return Ok(());
        }
        let outcome = match data {
            Data::Failed(reason) | Data::Stock(pipe::Event::Closed { reason }) => {
                self.claude_closed(&owner, reason);
                Ok(())
            }
            Data::Stock(pipe::Event::SignInRequired) => {
                if self.profile.route.billing == Billing::Subscription {
                    self.claude_mut()
                        .sessions
                        .get_mut(&owner)
                        .unwrap()
                        .events
                        .needs_sign_in = true;
                    if !self
                        .claude()
                        .sessions
                        .iter()
                        .any(|(key, p)| key != &owner && p.initialized && p.link.is_some())
                    {
                        self.state = Connection::SignInRequired;
                        self.changed.mark(Changed::CONNECTION);
                    }
                }
                Ok(())
            }
            Data::Connected(link) => {
                if link.session_id() != stock_id(&owner)? {
                    Err(io::Error::other("Claude connected to another session"))
                } else {
                    self.claude_mut().sessions.get_mut(&owner).unwrap().link = Some(link);
                    self.claude_ready(&owner)
                }
            }
            Data::History(batch) => {
                batch.apply(self, &owner);
                Ok(())
            }
            Data::Stock(pipe::Event::Frame(event)) => self.claude_frame(&owner, event),
        };
        if let Err(err) = &outcome {
            self.claude_closed(&owner, err.to_string());
        }
        outcome
    }

    fn claude_ready(&mut self, owner: &str) -> io::Result<()> {
        let p = &self.claude().sessions[owner];
        if !p.initialized || p.link.is_none() {
            return Ok(());
        }
        let mode = p.mode;
        let model = p.capabilities.as_ref().map(|c| c.model.clone());
        let t = self.transcripts.entry(owner.into()).or_default();
        t.access = mode;
        t.error = None;
        t.model = model;
        self.state = Connection::Ready;
        self.changed.mark(Changed::CONNECTION);
        self.flush_claude(owner)?;
        self.changed.transcript(owner);
        let reopen = std::mem::take(&mut self.reopen);
        for key in reopen {
            let parent = key.split('/').next().unwrap_or(&key);
            if parent != owner
                && self.claude().catalog.contains_key(parent)
                && !self.claude().sessions.contains_key(parent)
                && let Err(err) = self.claude_open(parent)
            {
                self.transcripts.entry(parent.into()).or_default().error = Some(err.to_string());
                self.changed.transcript(parent);
            }
        }
        Ok(())
    }

    fn claude_closed(&mut self, owner: &str, reason: String) {
        let Some(mut p) = self.claude_mut().sessions.remove(owner) else {
            return;
        };
        let mut unsent: Vec<_> = p
            .events
            .awaiting_echo
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        if let Some((_, text)) = p.first.take() {
            unsent.push(text);
        }
        self.restore_claude_drafts(owner, unsent);
        if self.claude().blank.as_deref() == Some(owner) {
            self.claude_mut().blank = None;
        }
        let removed: HashSet<_> = self
            .claude()
            .asks
            .iter()
            .filter(|(_, v)| v.0 == owner)
            .map(|(k, _)| k.clone())
            .collect();
        self.asks.retain(|a| !removed.contains(&a.id));
        self.claude_mut().asks.retain(|k, _| !removed.contains(k));
        for (id, t) in &mut self.transcripts {
            if id == owner || id.strip_prefix(owner).is_some_and(|s| s.starts_with('/')) {
                t.verified = false;
                t.ready = false;
                t.loading = false;
                t.active_turn = None;
                t.error = Some(reason.clone());
                self.changed.transcript(id);
            }
        }
        if !self
            .claude()
            .sessions
            .values()
            .any(|s| s.initialized && s.link.is_some())
        {
            self.state = if p.events.needs_sign_in || self.state == Connection::SignInRequired {
                Connection::SignInRequired
            } else {
                Connection::Closed(reason.clone())
            };
        }
        self.notice = Some(reason);
        self.changed
            .mark(Changed::CONNECTION | Changed::NOTICE | Changed::ASK | Changed::THREADS);
    }

    fn restore_claude_drafts(&mut self, owner: &str, mut unsent: Vec<String>) {
        if !unsent.is_empty() {
            let draft = self.draft(Some(owner));
            if !draft.is_empty() && unsent.last().is_none_or(|s| s != draft) {
                unsent.push(draft.to_owned());
            }
            self.edit(Some(owner), unsent.join("\n"));
        }
    }

    fn claude_frame(&mut self, owner: &str, event: wire::Event) -> io::Result<()> {
        use wire::Event;
        if !self.claude().sessions[owner].initialized
            && !matches!(&event,Event::ControlReply(reply) if reply.request_id=="ox-init")
        {
            return Err(io::Error::other(
                "Claude produced session activity before account verification",
            ));
        }
        let session = match &event {
            Event::Init(i) => Some(&i.session_id),
            Event::Title { session_id, .. } => Some(session_id),
            Event::Result(r) => Some(&r.session_id),
            Event::Item { scope, .. } | Event::Delta { scope, .. } => Some(&scope.session_id),
            _ => None,
        };
        if let Some(session) = session
            && session != stock_id(owner)?
        {
            return Err(io::Error::other("Claude reported an unexpected session ID"));
        }
        match event {
            Event::ControlReply(r) => self.claude_control(owner, r)?,
            Event::Title { name, .. } => {
                if let Some(row) = self.claude_mut().catalog.get_mut(owner) {
                    row.name = Some(name.clone());
                }
                if let Some(row) = self.catalog.get_mut(owner) {
                    row.name = Some(name);
                }
                self.catalog_revision += 1;
                self.changed.mark(Changed::THREADS);
            }
            Event::Init(init) => {
                let cwd = Path::new(&init.cwd);
                if cwd != self.profile.project
                    && cwd
                        .canonicalize()
                        .ok()
                        .zip(self.profile.project.canonicalize().ok())
                        .is_none_or(|(reported, expected)| reported != expected)
                {
                    return Err(io::Error::other("Claude opened a different project"));
                }
                if let Billing::Api(alias) = &self.profile.route.billing
                    && *alias != init.model
                {
                    return Err(io::Error::other(
                        "Claude opened a different API model alias",
                    ));
                }
                let mode = Access::from_mode(&init.permission_mode);
                let p = self.claude_mut().sessions.get_mut(owner).unwrap();
                p.mode = mode;
                if let Some(caps) = &mut p.capabilities {
                    caps.model = init.model.clone();
                    let offered: HashSet<_> = init
                        .slash_commands
                        .iter()
                        .map(|s| s.trim_start_matches('/'))
                        .collect();
                    if let Some(commands) = &mut caps.session_commands {
                        Arc::make_mut(commands)
                            .retain(|name, _| offered.contains(name.trim_start_matches('/')));
                    }
                }
                let t = self.transcripts.entry(owner.into()).or_default();
                t.model = Some(init.model.clone());
                t.access = mode;
                if let Some(row) = self.claude_mut().catalog.get_mut(owner) {
                    row.model = Some(init.model.clone());
                }
                if let Some(row) = self.catalog.get_mut(owner) {
                    row.model = Some(init.model);
                }
                self.changed.transcript(owner);
                self.changed.mark(Changed::THREADS);
            }
            Event::Ask(ask) => {
                let display = ask
                    .parent_tool_use_id
                    .as_deref()
                    .map(|id| child_key(owner, id))
                    .unwrap_or_else(|| owner.into());
                if display != owner && !self.claude().catalog.contains_key(&display) {
                    self.claude_child(owner, owner, &display, "Claude agent");
                }
                let key = self.claude_ask_id(owner, &ask.request_id);
                if !self.claude().asks.contains_key(&key) {
                    self.asks.push(ask.as_ask(key.clone(), display));
                    self.claude_mut().asks.insert(key, (owner.into(), ask));
                    self.changed.mark(Changed::ASK);
                }
            }
            Event::CancelAsk(id) => {
                let key = self.claude_ask_id(owner, &id);
                self.claude_mut().asks.remove(&key);
                self.asks.retain(|a| a.id != key);
                self.changed.mark(Changed::ASK);
            }
            Event::UnsupportedRequest { request_id, .. } => {
                let link = self.claude().sessions[owner].link.as_ref().ok_or_else(|| {
                    io::Error::other("Claude sent an unsupported request before connecting")
                })?;
                link.reject_request(&request_id, "This request is unsupported by OX Desktop")?;
            }
            Event::Item {
                scope,
                id,
                mut item,
                completed,
                output,
            } => {
                let display = self.claude_display(owner, scope);
                let is_user = matches!(item, Item::UserMessage { .. });
                if display == owner && is_user {
                    self.claude_mut()
                        .sessions
                        .get_mut(owner)
                        .unwrap()
                        .events
                        .awaiting_echo
                        .retain(|(uuid, _)| uuid != &id);
                }
                rebase_agents(owner, &display, &mut item, |child, tool, status| {
                    self.claude_child(owner, &display, child, tool);
                    if completed {
                        let t = self.transcripts.entry(child.into()).or_default();
                        if let Some(turn) = t.active_turn.take() {
                            t.last_finished = Some(turn);
                        }
                        if status == "failed" {
                            t.error = output.clone();
                        }
                        self.changed.transcript(child);
                        self.claude_status(
                            child,
                            if status == "failed" {
                                "systemError"
                            } else {
                                "idle"
                            },
                        );
                    }
                });
                let turn = self.claude_turn(owner, &display, &id, completed);
                self.transcripts
                    .entry(display.clone())
                    .or_default()
                    .observe_item(turn, id, item, completed, output);
                self.claude_activity(&display);
            }
            Event::Delta {
                scope,
                id,
                target,
                text,
            } => {
                let display = self.claude_display(owner, scope);
                let turn = self.claude_turn(owner, &display, &id, false);
                let target = match target {
                    wire::DeltaTarget::Text => DeltaTarget::Agent,
                    wire::DeltaTarget::Thinking => DeltaTarget::Summary(0),
                };
                self.transcripts
                    .entry(display.clone())
                    .or_default()
                    .observe_delta(turn, id, target, text);
                self.claude_activity(&display);
            }
            Event::Result(r) => {
                if !r.is_error {
                    self.title_after_turn(owner);
                }
                let t = self.transcripts.entry(owner.into()).or_default();
                t.last_finished = t.active_turn.take().or_else(|| t.last_finished.clone());
                if r.is_error {
                    t.error = Some(if r.errors.is_empty() {
                        format!("Claude turn ended ({})", r.subtype)
                    } else {
                        r.errors.join("\n")
                    });
                }
                self.claude_status(owner, if r.is_error { "systemError" } else { "idle" });
                self.claude_activity(owner);
                self.changed.mark(Changed::THREADS);
            }
        }
        Ok(())
    }
    fn claude_ask_id(&self, owner: &str, id: &str) -> RequestId {
        RequestId::Text(format!(
            "{owner}:{}:{id}",
            self.claude().sessions[owner].generation
        ))
    }
    fn claude_display(&mut self, owner: &str, scope: wire::Scope) -> String {
        let Some(tool) = scope.parent_tool_use_id else {
            return owner.into();
        };
        let display = child_key(owner, &tool);
        if !self.claude().catalog.contains_key(&display) {
            self.claude_child(owner, owner, &display, "Claude agent");
        }
        display
    }
    fn claude_child(&mut self, owner: &str, parent: &str, id: &str, label: &str) {
        if self.claude().catalog.get(id).is_none_or(|r| {
            r.parent_thread_id.as_deref() != Some(parent)
                || r.agent_nickname.as_deref() != Some(label)
        }) {
            let mut child = row(
                &self.profile,
                id.into(),
                label.into(),
                self.claude().catalog.get(owner).map_or(0, |r| r.updated_at),
            );
            child.parent_thread_id = Some(parent.into());
            child.agent_nickname = Some(label.into());
            child.worker = true;
            self.remember_claude(child);
        }
        let authorized = self.claude().sessions[owner].initialized;
        let t = self.transcripts.entry(id.into()).or_default();
        t.ready = authorized;
        t.verified = authorized;
        t.loading = false;
        self.changed.transcript(id);
    }
    fn claude_turn(&mut self, owner: &str, display: &str, id: &str, completed: bool) -> String {
        let events = &mut self.claude_mut().sessions.get_mut(owner).unwrap().events;
        if let Some(pending) = events.pending.get_mut(display)
            && let Some(turn) = if completed {
                pending.remove(id)
            } else {
                pending.get(id).cloned()
            }
        {
            return turn;
        }
        let parent = self
            .transcripts
            .get(owner)
            .and_then(|t| t.active_turn.clone().or_else(|| t.last_finished.clone()));
        let t = self.transcripts.entry(display.into()).or_default();
        let turn = t
            .active_turn
            .get_or_insert_with(|| {
                if display == owner {
                    id.to_owned()
                } else {
                    parent.unwrap_or_else(|| id.to_owned())
                }
            })
            .clone();
        if !completed {
            self.claude_mut()
                .sessions
                .get_mut(owner)
                .unwrap()
                .events
                .pending
                .entry(display.into())
                .or_default()
                .insert(id.into(), turn.clone());
        }
        turn
    }
    fn claude_status(&mut self, id: &str, kind: &str) {
        let Some(r) = self.catalog.get_mut(id) else {
            return;
        };
        if r.status.as_ref().is_some_and(|s| s.kind == kind) {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        r.updated_at = now.as_secs() as i64;
        r.activity_order = now.as_nanos();
        let status = r.status.get_or_insert_with(Default::default);
        status.kind = kind.into();
        let row = r.clone();
        self.claude_mut().catalog.insert(id.into(), row);
        self.catalog_revision += 1;
        self.changed.mark(Changed::THREADS);
    }
    fn claude_activity(&mut self, display: &str) {
        if self
            .transcripts
            .get(display)
            .is_some_and(|t| t.active_turn.is_some())
        {
            self.claude_status(display, "active");
        }
        self.changed.transcript(display);
        if self.front && self.selected.as_deref() == Some(display) {
            return;
        }
        if let Some(t) = self.transcripts.get_mut(display) {
            t.unread = true;
        }
        if let Some(row) = self.catalog.get_mut(display)
            && !row.unread
        {
            row.unread = true;
            self.changed.mark(Changed::THREADS);
        }
        if let Some(row) = self.claude_mut().catalog.get_mut(display) {
            row.unread = true;
        }
    }

    fn claude_control(&mut self, owner: &str, r: wire::ControlReply) -> io::Result<()> {
        if r.request_id == "ox-init" {
            let native = r
                .capabilities()
                .ok_or_else(|| io::Error::other("Invalid Claude initialization reply"))?;
            let mut caps = Capabilities {
                provider: self.profile.provider.clone(),
                ..Default::default()
            };
            let mut commands = BTreeMap::new();
            // Upstream resolves a marked builtin before a same-name project command.
            let builtins: HashSet<_> = native
                .commands
                .iter()
                .filter(|cmd| cmd.builtin)
                .map(|cmd| cmd.name.clone())
                .collect();
            for cmd in native.commands {
                if !cmd.builtin && builtins.contains(&cmd.name) {
                    continue;
                }
                let name = cmd.name.trim_start_matches('/');
                if headless_command(name, cmd.builtin) {
                    commands.insert(format!("/{name}"), cmd.description);
                }
            }
            caps.session_commands = Some(Arc::new(commands));
            let models = Arc::make_mut(&mut caps.models);
            match &self.profile.route.billing {
                Billing::Api(alias) => {
                    caps.model = alias.clone();
                    for alias in &self.claude().launcher.aliases {
                        models.insert(
                            alias.clone(),
                            ModelInfo {
                                label: alias.clone(),
                                ..Default::default()
                            },
                        );
                    }
                }
                Billing::Subscription => {
                    for model in native.models {
                        if model.value == "default" {
                            caps.model = model.value.clone();
                        }
                        models.insert(
                            model.value,
                            ModelInfo {
                                label: model.display_name,
                                ..Default::default()
                            },
                        );
                    }
                }
            }
            let mode = native
                .current_permission_mode
                .as_deref()
                .and_then(Access::from_mode);
            self.commands.schema = Some(caps.clone());
            let p = self.claude_mut().sessions.get_mut(owner).unwrap();
            p.capabilities = Some(caps);
            p.mode = mode;
            p.initialized = true;
            return self.claude_ready(owner);
        }
        let Some(control) = self
            .claude_mut()
            .sessions
            .get_mut(owner)
            .unwrap()
            .controls
            .remove(&r.request_id)
        else {
            return Ok(());
        };
        let result = match r.payload {
            Ok(r) => r,
            Err(reason) => {
                self.notice = Some(reason);
                self.changed.mark(Changed::NOTICE);
                return Ok(());
            }
        };
        match control {
            Control::Mode(mode) if result["mode"].as_str() == mode.mode() => {
                self.claude_mut().sessions.get_mut(owner).unwrap().mode = Some(mode);
                self.transcripts.entry(owner.into()).or_default().access = Some(mode);
            }
            Control::Model(model) if result["model"] == model => {
                if let Some(c) = &mut self
                    .claude_mut()
                    .sessions
                    .get_mut(owner)
                    .unwrap()
                    .capabilities
                {
                    c.model = model.clone();
                }
                self.transcripts.entry(owner.into()).or_default().model = Some(model.clone());
                if let Some(row) = self.catalog.get_mut(owner) {
                    row.model = Some(model.clone());
                }
                if let Some(row) = self.claude_mut().catalog.get_mut(owner) {
                    row.model = Some(model);
                }
            }
            // The interrupt acknowledgment does not complete the turn. Claude's result does.
            Control::Interrupt => {
                // The receipt names queued prompts that survived Stop, and (on supporting
                // CLIs) uuids that were expressly cancelled. Keep editable text for the latter.
                let cancelled: HashSet<_> = result["cancelled"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if !cancelled.is_empty() {
                    let pending = &mut self
                        .claude_mut()
                        .sessions
                        .get_mut(owner)
                        .unwrap()
                        .events
                        .awaiting_echo;
                    let (cancelled, survivors): (Vec<_>, _) = std::mem::take(pending)
                        .into_iter()
                        .partition(|(id, _)| cancelled.contains(id.as_str()));
                    *pending = survivors;
                    self.restore_claude_drafts(
                        owner,
                        cancelled.into_iter().map(|(_, t)| t).collect(),
                    );
                }
                if let Some(count) = result["still_queued"]
                    .as_array()
                    .map(Vec::len)
                    .filter(|n| *n > 0)
                {
                    self.transcripts
                        .entry(owner.into())
                        .or_default()
                        .command_notice =
                        Some(format!("Claude kept {count} queued message(s) after Stop."));
                }
            }
            _ => {
                self.notice = Some("Claude did not confirm the requested setting".into());
                self.changed.mark(Changed::NOTICE);
            }
        }
        self.changed.transcript(owner);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }
}
pub(super) fn rebase_agents(
    owner: &str,
    display: &str,
    item: &mut Item,
    mut child: impl FnMut(&str, &str, &str),
) {
    if let Item::AgentOperation {
        tool,
        status,
        sender_thread_id,
        receiver_thread_ids,
    } = item
    {
        *sender_thread_id = display.to_owned();
        for raw in receiver_thread_ids {
            let key = child_key(owner, raw);
            child(&key, tool, status);
            *raw = key;
        }
    }
}

fn headless_command(name: &str, builtin: bool) -> bool {
    if name.is_empty()
        || name.starts_with('_')
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
        || matches!(
            name,
            "model"
                | "effort"
                | "fast"
                | "goal"
                | "speed"
                | "clear"
                | "terminal"
                | "new"
                | "resume"
                | "status"
        )
    {
        return false;
    }
    // The initialize list includes terminal menus and internal commands. These shipped
    // skills and text commands have headless handling in stock 2.1.331. Custom project,
    // user and MCP commands have no builtin marker and come from this session's inventory.
    !builtin
        || matches!(
            name,
            "deep-research"
                | "dataviz"
                | "update-config"
                | "verify"
                | "debug"
                | "code-review"
                | "simplify"
                | "batch"
                | "fewer-permission-prompts"
                | "doctor"
                | "loop"
                | "claude-api"
                | "workflow-authoring"
                | "run"
                | "run-skill-generator"
                | "plugin-authoring"
                | "init"
                | "security-review"
                | "recap"
                | "insights"
                | "list-agents"
                | "reload-plugins"
                | "reload-skills"
                | "compact"
                | "context"
                | "usage"
                | "rename"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";
    fn fixture() -> (Store, String, String) {
        let profile = Profile {
            project: "/fixture".into(),
            route: crate::runtime::Route {
                runtime: Runtime::Claude,
                billing: Billing::Subscription,
            },
            provider: SUBSCRIPTION_PROVIDER.into(),
        };
        let a = session_key(&profile.route.billing, A);
        let b = session_key(&profile.route.billing, B);
        let launcher = Launcher {
            ox: PathBuf::new(),
            path: Default::default(),
            profiles: Profiles {
                subscription: PathBuf::new(),
                api: PathBuf::new(),
            },
            index: PathBuf::new(),
            aliases: vec![],
        };
        let project = Project {
            launcher: Arc::new(launcher),
            deliver: Arc::new(|_| panic!("test must not launch a process")),
            sessions: Default::default(),
            catalog: Default::default(),
            asks: Default::default(),
            blank: None,
            serial: 2,
        };
        let mut store = Store::with_link(profile, links::Link::Claude(Box::new(project)));
        for k in [&a, &b] {
            store.claude_mut().sessions.insert(
                k.clone(),
                Process {
                    generation: 2,
                    initialized: true,
                    ..Default::default()
                },
            );
            store.remember_claude(row(&store.profile, k.clone(), "Session".into(), 1));
        }
        (store, a, b)
    }
    fn update(store: &mut Store, owner: &str, generation: u64, data: Data) -> io::Result<()> {
        store.claude_event(Update {
            owner: owner.into(),
            generation,
            data,
        })
    }
    fn decode(value: Value) -> wire::Event {
        wire::Decoder::default()
            .decode_value(value)
            .unwrap()
            .pop()
            .unwrap()
    }
    #[test]
    fn palette_shows_only_headless_builtins_and_advertised_custom_skills() {
        assert!(headless_command("context", true));
        assert!(headless_command("project:lint", false));
        assert!(!headless_command("new-terminal-wizard", true));
        assert!(!headless_command("model", false));
        assert!(!headless_command("goal", true));
    }
    #[test]
    fn failure_retires_its_generation_only_and_restores_unsent_drafts() {
        let (mut s, a, b) = fixture();
        let request = decode(
            json!({"type":"control_request","request_id":"r","request":{"subtype":"can_use_tool","tool_name":"Read","tool_use_id":"tool","input":{"file_path":"a"}}}),
        );
        for owner in [&a, &b] {
            update(
                &mut s,
                owner,
                2,
                Data::Stock(pipe::Event::Frame(request.clone())),
            )
            .unwrap();
        }
        assert_ne!(s.asks[0].id, s.asks[1].id);
        s.claude_mut().sessions.get_mut(&a).unwrap().first = Some(("m".into(), "unsent".into()));
        let pending = &mut s.claude_mut().sessions.get_mut(&a).unwrap().events;
        pending.sent("delivered", "already confirmed");
        pending.sent("lost", "writer accepted, not echoed");
        let user = decode(
            json!({"type":"user","session_id":A,"uuid":"delivered","message":{"content":"already confirmed"}}),
        );
        update(&mut s, &a, 2, Data::Stock(pipe::Event::Frame(user))).unwrap();
        s.edit(Some(&a), "later edit".into());
        update(&mut s, &a, 1, Data::Failed("stale close".into())).unwrap();
        assert_eq!(s.asks.len(), 2);
        update(&mut s, &a, 2, Data::Failed("real close".into())).unwrap();
        assert_eq!(s.asks.len(), 1);
        assert_eq!(s.asks[0].thread.as_deref(), Some(b.as_str()));
        assert_eq!(
            s.draft(Some(&a)),
            "writer accepted, not echoed\nunsent\nlater edit"
        );
        assert!(!s.claude().sessions.contains_key(&a));
        assert!(s.claude().sessions.contains_key(&b));
        update(&mut s, &a, 2, Data::Stock(pipe::Event::Frame(request))).unwrap();
        assert_eq!(s.asks.len(), 1);
    }

    #[test]
    fn verified_fork_history_is_rebased_but_foreign_live_frames_close_only_their_process() {
        let (mut s, a, b) = fixture();
        let user = decode(
            json!({"type":"user","session_id":A,"uuid":"source-prompt","message":{"content":"prefix"}}),
        );
        let batch = history::fixture(&s.profile, &b, vec![user.clone()]);
        update(&mut s, &b, 2, Data::History(batch)).unwrap();
        assert_eq!(s.transcript(&b).unwrap().len(), 1);
        assert!(s.transcript(&a).is_none());
        assert!(s.transcript(&b).unwrap().active_turn().is_none());
        assert!(update(&mut s, &b, 2, Data::Stock(pipe::Event::Frame(user))).is_err());
        assert!(!s.claude().sessions.contains_key(&b));
        assert!(s.claude().sessions.contains_key(&a));
    }

    #[test]
    fn late_agent_completion_updates_its_spawn_turn_during_a_new_parent_turn() {
        let (mut s, a, _) = fixture();
        let mut d = wire::Decoder::default();
        let mut tool = None;
        for line in include_str!("../../../tests/fixtures/claude-agent.jsonl").lines() {
            let mut r: Value = serde_json::from_str(line).unwrap();
            r["session_id"] = json!(A);
            if r["type"] == "system" && r["subtype"] == "init" {
                r["cwd"] = json!("/fixture");
            }
            if r["subtype"] == "task_started" {
                tool = r["tool_use_id"].as_str().map(str::to_owned);
            }
            if r["subtype"] == "task_notification" {
                s.transcripts.get_mut(&a).unwrap().active_turn = Some("later-turn".into());
            }
            for e in d.decode_value(r).unwrap() {
                update(&mut s, &a, 2, Data::Stock(pipe::Event::Frame(e))).unwrap();
            }
        }
        let tool = tool.unwrap();
        let child = child_key(&a, &tool);
        assert_eq!(
            s.catalog_row(&child).unwrap().parent_thread_id.as_deref(),
            Some(a.as_str())
        );
        assert!(s.transcript(&child).unwrap().active_turn().is_none());
        let t = s.transcript(&a).unwrap();
        let first = t
            .entry(&crate::transcript::ItemKey {
                turn: tool.clone(),
                item: tool.clone(),
            })
            .unwrap();
        assert!(
            matches!(&first.item,Item::AgentOperation{status,receiver_thread_ids,..} if status=="completed" && receiver_thread_ids==&[child])
        );
        assert!(
            t.entry(&crate::transcript::ItemKey {
                turn: "later-turn".into(),
                item: tool
            })
            .is_none()
        );
    }
}
