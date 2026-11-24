//! Project the authoritative store onto the device protocol. Credentials, process
//! handles and the runtime-specific payload of an approval never leave the owner.
use super::{Connection, Store, ThreadRow};
use crate::{
    changed::Changed,
    host_protocol::*,
    transcript::{Entry, ItemKey, Row, Transcript, item::Item},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn keys(t: &Transcript) -> impl Iterator<Item = &ItemKey> {
    (0..t.len())
        .filter_map(|index| t.row(index))
        .flat_map(|row| {
            let (first, second) = match row {
                Row::Message(key) => (std::slice::from_ref(key), &[][..]),
                Row::Tools(group) => group.as_slices(),
            };
            first.iter().chain(second)
        })
}
fn item_view(k: &ItemKey, e: &Entry) -> TimelineItem {
    let (role, text, detail) = match &e.item {
        Item::AgentMessage { text, is_update } => {
            ("assistant", text.clone(), json!({"update":is_update}))
        }
        Item::Plan { text } => ("plan", text.clone(), Value::Null),
        Item::UserMessage { content } => (
            "user",
            content
                .iter()
                .filter_map(|p| p.text.as_deref().or(p.path.as_deref()))
                .collect::<Vec<_>>()
                .join("\n"),
            json!(e.item),
        ),
        Item::Reasoning { summary } => ("thinking", summary.join("\n"), json!(e.item)),
        Item::CommandExecution {
            command,
            status,
            aggregated_output,
            exit_code,
            ..
        } => (
            "tool",
            format!(
                "{command}\n{}",
                aggregated_output.as_deref().unwrap_or(&e.output)
            ),
            json!({"item":e.item,"output":e.output,"status":status,"exit":exit_code}),
        ),
        item => {
            let label = match item {
                Item::FileChange { .. } => "File changes",
                Item::AgentOperation { tool, .. }
                | Item::ToolCall { tool, .. }
                | Item::McpToolCall { tool, .. } => tool,
                Item::WebSearch { query } => query,
                Item::Unknown { kind, .. } => kind,
                _ => unreachable!(),
            };
            ("tool", label.into(), json!({"item":item,"output":e.output}))
        }
    };
    TimelineItem {
        turn: k.turn.clone(),
        id: k.item.clone(),
        role: role.into(),
        text,
        detail,
        revision: e.revision,
    }
}
fn timeline_detail(t: &Transcript) -> Value {
    json!({"active":t.active_turn(),"last_finished":t.last_finished,"error":t.error(),"ready":t.is_ready(),"loading":t.is_loading(),"older":t.has_older(),"access":t.access(),"reviews":t.reviews,"usage":t.usage,"command_notice":t.command_notice,"model":t.model,"effort":t.effort,"tier":t.tier,"unread":t.unread()})
}
fn timeline(t: &Transcript) -> Timeline {
    Timeline {
        items: keys(t)
            .filter_map(|k| t.entry(k).map(|e| item_view(k, e)))
            .collect(),
        detail: timeline_detail(t),
    }
}
impl Store {
    fn scope_status(&self) -> String {
        match self.state() {
            Connection::Connecting => "connecting",
            Connection::Ready => "ready",
            Connection::SignInRequired => "sign_in_required",
            Connection::Closed(_) => "closed",
        }
        .into()
    }
    fn composer_caps(&self, id: Option<&str>, caps: &mut Vec<String>) {
        let schema = self.command_caps(id);
        if schema.methods.contains("fuzzyFileSearch") {
            caps.push("mention".into());
        }
        if !self.is_session_scoped()
            && schema
                .models
                .get(&self.options(id).model)
                .and_then(|m| m.input_modalities.as_ref())
                .is_none_or(|kinds| kinds.iter().any(|kind| kind == "image"))
        {
            caps.push("attach_image".into());
        }
    }
    fn scope_caps(&self) -> Vec<String> {
        let mut caps = Vec::new();
        if self.can_send(None) {
            caps.extend(["start".into(), "access".into()]);
            self.composer_caps(None, &mut caps);
            if self.has_command_schema() && !self.commands_loading() {
                caps.extend(["model_options".into(), "native_commands".into()]);
            }
        }
        if self.can_edit_thread_metadata() {
            caps.push("edit_metadata".into());
        }
        caps
    }
    fn host_detail(&self) -> Value {
        let closed = match self.state() {
            Connection::Closed(e) => Some(e),
            _ => None,
        };
        json!({"provider":self.profile.provider,"selected":self.selected(),"error":closed,"notice":self.notice(),
           "next_access":self.next_access(),"applied_draft_access":self.applied_access(None),"default_options":self.options(None),"schema":self.has_command_schema(),"loading_commands":self.commands_loading(),
           "commands":self.command_caps(None),"usage":self.usage,"archived":self.archived_threads(),"mention":self.mention,
           "palette_query":self.palette_query(),"palette_more":self.palette_has_more(),"palette_error":self.palette_search_error(),"palette_rows":self.palette_rows(),
           "more":self.has_more(),"next_sending":self.sending(None),"author":self.runtime_name(),"access_choices":crate::runtime::Access::choices(self.profile.route.runtime)})
    }
    fn host_asks(&self) -> Vec<AskView> {
        self.asks()
            .iter()
            .map(|a| AskView {
                command: a.command.clone(),
                id: a.id.clone(),
                token: String::new(),
                thread: a.thread.clone(),
                turn: a.turn.clone(),
                item: a.item.clone(),
                choice_details: a.decisions().iter().map(|v| v.detail.clone()).collect(),
                title: a.title().into(),
                detail: a.detail.clone(),
                choices: a.decisions().iter().map(|v| v.label.clone()).collect(),
                questions: a.questions.clone(),
            })
            .collect()
    }
    fn session_view(&self, r: &ThreadRow) -> SessionView {
        let id = r.id.as_str();
        let t = self.transcript(id);
        let running = t.is_some_and(|t| t.active_turn().is_some());
        let mut caps = vec!["open".to_owned()];
        let status = {
            if self.can_send(Some(id)) {
                caps.push(if running { "steer" } else { "send" }.into());
                self.composer_caps(Some(id), &mut caps);
            }
            if running {
                caps.push("stop".into());
            }
            if self.can_send(Some(id)) && self.has_command_schema() && !self.commands_loading() {
                caps.push("native_commands".into());
                if !running
                    && !self.changing_options(id)
                    && (self.is_session_scoped()
                        || self
                            .command_caps(Some(id))
                            .methods
                            .contains("thread/settings/update"))
                {
                    caps.push("model_options".into());
                }
            }
            if r.parent_thread_id.is_none() && self.can_change_access(id) {
                caps.push("access".into());
            }
            if self.can_manage_thread(id) {
                caps.push("fork".into());
                if self.can_edit_thread_metadata() {
                    caps.extend(["archive".into(), "rename".into()]);
                }
            }
            if self.can_generate_title(id) {
                caps.push("generate_title".into());
            }
            if self.asks().iter().any(|a| a.thread.as_deref() == Some(id)) {
                "needs_input"
            } else if running || r.status.as_ref().is_some_and(|s| s.kind == "active") {
                "running"
            } else if t.and_then(|t| t.error()).is_some()
                || r.status.as_ref().is_some_and(|s| s.kind == "systemError")
            {
                "failed"
            } else {
                "idle"
            }
        };
        let detail = json!({"row":r,"worker":r.worker,"unread":r.unread,"order":r.activity_order.to_string(),"generated":r.generated_title,
            "owned":t.is_some(),"changing":self.changing_options(id),"terminal":self.in_terminal(id),"sending":self.sending(Some(id)),
            "route":self.thread_route(Some(id)),"options":t.map(|_|self.options(Some(id))),"access":self.applied_access(Some(id)),"command_caps":(self.is_session_scoped() && t.is_some()).then(||self.command_caps(Some(id)))});
        let mut detail = detail;
        let mut parent = r.parent_thread_id.clone();
        if let Some((nest, extra)) = self.host_annotations.get(id) {
            if let (Some(target), Some(fields)) = (detail.as_object_mut(), extra.as_object()) {
                target.extend(fields.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            parent = parent.or_else(|| nest.clone());
        }
        SessionView {
            id: r.id.clone(),
            title: r.title().into(),
            parent,
            status: status.into(),
            updated_at: r.updated_at,
            capabilities: caps,
            detail,
        }
    }
    pub fn host_snapshot(&self, scope: &Scope) -> ScopeState {
        ScopeState {
            scope: scope.clone(),
            connection: self.scope_status(),
            capabilities: self.scope_caps(),
            sessions: self
                .threads()
                .iter()
                .map(|r| self.session_view(r))
                .collect(),
            asks: self.host_asks(),
            transcripts: self
                .transcripts
                .iter()
                .map(|(id, t)| (id.clone(), timeline(t)))
                .collect::<BTreeMap<_, _>>(),
            detail: self.host_detail(),
        }
    }
    pub fn host_project(
        &self,
        _scope: &Scope,
        changed: &Changed,
        state: &mut ScopeState,
    ) -> Vec<Change> {
        let mut out = Vec::new();
        let scopes_changed = changed.surfaces() != 0;
        if scopes_changed {
            let detail = self.host_detail();
            let capabilities = self.scope_caps();
            let connection = self.scope_status();
            if detail != state.detail
                || connection != state.connection
                || capabilities != state.capabilities
            {
                state.detail = detail.clone();
                state.capabilities = capabilities.clone();
                state.connection = connection.clone();
                out.push(Change::State {
                    detail,
                    connection,
                    capabilities,
                });
            }
            let asks = self.host_asks();
            if asks.len() != state.asks.len()
                || asks.iter().zip(&state.asks).any(|(a, b)| {
                    a.id != b.id
                        || a.thread != b.thread
                        || a.command != b.command
                        || a.choices != b.choices
                        || a.questions != b.questions
                        || a.detail != b.detail
                })
            {
                for ask in &asks {
                    if !state.asks.iter().any(|a| a.id == ask.id)
                        && let Some(thread) = ask.thread.as_ref()
                    {
                        out.push(self.note(
                            thread,
                            if ask.questions.is_empty() {
                                NotificationKind::Approval
                            } else {
                                NotificationKind::Question
                            },
                        ));
                    }
                }
                state.asks = asks.clone();
                out.push(Change::Asks { asks });
            }
        }
        let refresh_sessions =
            changed.affects_catalog() || self.threads().len() != state.sessions.len();
        if refresh_sessions {
            let sessions: Vec<_> = self
                .threads()
                .iter()
                .map(|r| self.session_view(r))
                .collect();
            if sessions != state.sessions {
                state.sessions = sessions.clone();
                out.push(Change::Sessions { sessions });
            }
        }
        for id in changed.dirty_threads() {
            let Some(t) = self.transcript(id) else {
                if state.transcripts.remove(id).is_some() {
                    out.push(Change::Retired { thread: id.clone() });
                }
                continue;
            };
            if !refresh_sessions
                && let Some(row) = self.catalog_row(id)
                && let Some(prev) = state.sessions.iter_mut().find(|r| r.id == *id)
            {
                let next = self.session_view(row);
                if next != *prev {
                    *prev = next.clone();
                    out.push(Change::Session { session: next });
                }
            }
            let Some(previous) = state.transcripts.get_mut(id) else {
                let timeline = timeline(t);
                state.transcripts.insert(id.clone(), timeline.clone());
                out.push(Change::Transcript {
                    thread: id.clone(),
                    timeline,
                });
                continue;
            };
            let detail = timeline_detail(t);
            if previous.detail["last_finished"] != detail["last_finished"]
                && !detail["last_finished"].is_null()
            {
                out.push(self.note(
                    id,
                    if t.error().is_some() {
                        NotificationKind::Failed
                    } else {
                        NotificationKind::Finished
                    },
                ));
            }
            if detail != previous.detail {
                previous.detail = detail.clone();
                out.push(Change::TimelineState {
                    thread: id.clone(),
                    detail,
                });
            }
            let mut count = 0;
            let mut reset = false;
            let mut items = Vec::new();
            for k in keys(t) {
                let Some(entry) = t.entry(k) else {
                    continue;
                };
                match previous.items.get_mut(count) {
                    Some(old) if old.turn != k.turn || old.id != k.item => {
                        reset = true;
                        break;
                    }
                    Some(old) if old.revision != entry.revision => {
                        if let Item::AgentMessage { text, is_update } = &entry.item
                            && old.role == "assistant"
                            && old.detail["update"] == *is_update
                            && let Some(suffix) = text.strip_prefix(&old.text)
                        {
                            old.text.push_str(suffix);
                            old.revision = entry.revision;
                            out.push(Change::Append {
                                thread: id.clone(),
                                turn: k.turn.clone(),
                                id: k.item.clone(),
                                text: suffix.to_owned(),
                                revision: entry.revision,
                            });
                        } else {
                            let item = item_view(k, entry);
                            *old = item.clone();
                            items.push(item);
                        }
                    }
                    Some(_) => (),
                    None => {
                        let item = item_view(k, entry);
                        previous.items.push(item.clone());
                        items.push(item);
                    }
                }
                count += 1;
            }
            if reset || count < previous.items.len() {
                let timeline = timeline(t);
                *previous = timeline.clone();
                out.push(Change::Transcript {
                    thread: id.clone(),
                    timeline,
                });
            } else if !items.is_empty() {
                out.push(Change::Items {
                    thread: id.clone(),
                    items,
                });
            }
        }
        out
    }
    fn note(&self, thread: &str, event: NotificationKind) -> Change {
        let title = self
            .catalog_row(thread)
            .and_then(|r| r.name.as_ref().or(r.generated_title.as_ref()))
            .map_or("OX session", String::as_str);
        Change::Notification {
            thread: thread.into(),
            title: title.into(),
            event,
        }
    }
}
