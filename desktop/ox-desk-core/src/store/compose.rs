use super::{Connection, Pending, Route, Store};
use crate::runtime::Billing;
use crate::{
    changed::Changed,
    codex::{
        history::ResumedThread,
        reply::Reply,
        request::{Call, ThreadCommand},
    },
};
use std::io;

#[derive(Default)]
pub(super) struct Draft {
    pub text: String,
    images: Vec<crate::mentions::ImageInput>,
    sent: Option<SentInput>,
}
impl Draft {
    pub(super) fn restore_before(mut self, newer: &mut Draft) {
        if !self.text.is_empty() && !newer.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(&newer.text);
        self.images.append(&mut newer.images);
        newer.text = self.text;
        newer.images = self.images;
    }
}
enum SentInput {
    Draft(String, Vec<crate::mentions::ImageInput>),
    PaletteCommand,
}
impl SentInput {
    fn text(&self) -> Option<&str> {
        match self {
            Self::Draft(text, _) => Some(text),
            Self::PaletteCommand => None,
        }
    }
}
#[derive(Debug)]
pub(super) struct Submit {
    options: Option<super::routes_thread::Options>,
    owner: Option<String>,
    expected_turn: Option<String>,
    selection: u64,
    command: Option<ThreadCommand>,
    /// Handoff items injected after thread/start and before the first turn.
    inject: Vec<serde_json::Value>,
}
#[derive(Debug)]
pub(super) enum Operation {
    New(Submit),
    Account(Submit),
    Send(Submit),
    Inject(Submit),
    Stop(String),
}
impl Operation {
    pub(super) fn thread(&self) -> Option<&str> {
        match self {
            Self::New(s) | Self::Account(s) | Self::Send(s) | Self::Inject(s) => s.owner.as_deref(),
            Self::Stop(id) => Some(id),
        }
    }
}
impl Store {
    pub fn new_session(&mut self) {
        if self.remote().is_some() {
            if let Err(e) = self.dispatch_remote(crate::actions::Action::NewDraft) {
                self.notice = Some(e.to_string());
            }
            return;
        }
        self.selection += 1;
        self.selected = None;
        if self.is_session_scoped()
            && let Err(error) = self.prepare_claude_draft()
        {
            self.notice = Some(error.to_string());
        }
    }
    pub fn draft(&self, id: Option<&str>) -> &str {
        self.drafts
            .get(&id.map(str::to_owned))
            .map_or("", |d| d.text.as_str())
    }
    pub fn image_model_warning(&self, id: Option<&str>) -> Option<&'static str> {
        if self.draft_images(id).is_empty() {
            return None;
        }
        let chosen = self.options(id);
        self.command_caps(id).models.get(&chosen.model)?.input_modalities.as_ref()
            .filter(|modalities| !modalities.iter().any(|m| m=="image"))
            .map(|_| "This model accepts text only. Choose an image-capable model or remove the attachment.")
    }
    pub fn draft_images(&self, id: Option<&str>) -> &[crate::mentions::ImageInput] {
        self.drafts
            .get(&id.map(str::to_owned))
            .map_or(&[], |d| d.images.as_slice())
    }
    pub fn attach_image(&mut self, id: Option<&str>, image: crate::mentions::ImageInput) {
        self.drafts
            .entry(id.map(str::to_owned))
            .or_default()
            .images
            .push(image);
    }
    pub fn remove_image(&mut self, id: Option<&str>, index: usize) {
        if let Some(d) = self.drafts.get_mut(&id.map(str::to_owned))
            && index < d.images.len()
        {
            d.images.remove(index);
        }
    }
    pub fn edit(&mut self, id: Option<&str>, text: String) {
        self.drafts.entry(id.map(str::to_owned)).or_default().text = text;
    }
    pub fn sending(&self, id: Option<&str>) -> bool {
        if let Some(remote) = self.remote() {
            return remote.sending.contains(id.unwrap_or("")) || self.pending_input(id);
        }
        id.is_some_and(|id| self.resume_sends.contains(id))
            || self
                .drafts
                .get(&id.map(str::to_owned))
                .is_some_and(|d| d.sent.is_some())
    }
    pub fn can_send(&self, id: Option<&str>) -> bool {
        if let Some(remote) = self.remote() {
            return match id {
                None => remote.scope_caps.iter().any(|s| s == "start"),
                Some(id) => remote.can(id, "send") || remote.can(id, "steer"),
            };
        }
        self.state == Connection::Ready
            && (!self.is_session_scoped() || self.claude_can_submit(id))
            && !self.sending(id)
            && id.is_none_or(|thread| !self.changing_options(thread))
            && id.is_none_or(|id| {
                !self.in_terminal(id)
                    && self
                        .transcripts
                        .get(id)
                        .is_some_and(|t| t.ready && t.verified)
            })
    }
    /// The composer is usable: the thread can take a message now, or it is ended and a
    /// send reopens it first. Ending or reaping a process never disables the composer.
    pub fn can_compose(&self, id: Option<&str>) -> bool {
        self.can_send(id) || id.is_some_and(|id| self.can_resume_send(id))
    }
    /// An ended thread (no live process) that a send can reopen. Local stores only; a
    /// remote client reads the host's `send` capability instead.
    pub(super) fn can_resume_send(&self, id: &str) -> bool {
        self.remote().is_none()
            && self.ended_threads.contains(id)
            && matches!(self.state, Connection::Ready | Connection::Connecting)
            && !self.has_live_session(id)
            && !self.in_terminal(id)
            && !self.sending(Some(id))
            && self.catalog.get(id).is_some()
            && !(self.is_session_scoped() && (id.contains('/') || self.state != Connection::Ready))
    }
    /// Reopen an ended thread and keep the draft to send once it is ready. Claude resumes
    /// with the message as the process's first input; Codex resumes, then `flush_resume_sends`
    /// submits the draft when the thread is verified.
    fn resume_and_send(&mut self, id: &str) -> io::Result<()> {
        let draft = self.drafts.get(&Some(id.to_owned()));
        let text = draft.map_or("", |d| d.text.as_str());
        let has_images = draft.is_some_and(|d| !d.images.is_empty());
        if text.trim().is_empty() && !has_images {
            return Err(io::Error::other("Enter a message"));
        }
        if self.is_session_scoped() {
            if has_images {
                return Err(io::Error::other(
                    "Remove the attachment before submitting to Claude Code",
                ));
            }
            return self.claude_resume_send(id);
        }
        if text.trim_start().starts_with('/') {
            return Err(io::Error::other("Open this command in stock Codex"));
        }
        self.ended_threads.remove(id);
        self.resume_sends.insert(id.into());
        let opened = if self.state == Connection::Ready {
            self.open_catalog_id(id)
        } else {
            // The runtime is still connecting; `connected` reopens what is listed here.
            self.selection += 1;
            self.selected = Some(id.into());
            self.reopen.push(id.into());
            self.mark_loading(id);
            Ok(())
        };
        if let Err(error) = opened {
            self.resume_sends.remove(id);
            self.ended_threads.insert(id.into());
            return Err(error);
        }
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }
    /// Submit drafts waiting on a reopened thread once it can take them. A reopen that
    /// failed drops the wait and leaves the draft in the composer.
    pub(super) fn flush_resume_sends(&mut self) {
        if self.resume_sends.is_empty() {
            return;
        }
        let waiting: Vec<String> = self.resume_sends.iter().cloned().collect();
        for id in waiting {
            self.resume_sends.remove(&id);
            if self.can_send(Some(&id)) {
                if let Err(error) = self.submit(Some(&id)) {
                    self.notice = Some(error.to_string());
                    self.changed.mark(Changed::NOTICE);
                }
                continue;
            }
            let t = self.transcripts.get(&id);
            let opening = matches!(self.state, Connection::Connecting)
                || t.is_some_and(|t| t.loading || (t.ready && !t.verified));
            if opening && t.is_none_or(|t| t.error.is_none()) {
                self.resume_sends.insert(id);
            } else {
                self.ended_threads.insert(id.clone());
                self.notice = Some(
                    "The thread did not resume. Your message is still in the composer.".into(),
                );
                self.changed.mark(Changed::NOTICE);
                self.changed.transcript(&id);
            }
        }
    }
    pub(crate) fn send_client_message(
        &mut self,
        owner: Option<String>,
        text: String,
        images: Vec<crate::mentions::ImageInput>,
    ) -> io::Result<()> {
        if self.sending(owner.as_deref()) {
            return Err(io::Error::other("A message is already being submitted"));
        }
        let previous = self.drafts.insert(
            owner.clone(),
            Draft {
                text,
                images,
                sent: None,
            },
        );
        match self.submit(owner.as_deref()) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.drafts.remove(&owner);
                if let Some(d) = previous {
                    self.drafts.insert(owner, d);
                }
                Err(error)
            }
        }
    }
    /// Open a new session that continues another one. Codex receives `items`
    /// through thread/inject_items before the first turn; a Claude `text`
    /// already carries its context block, so `items` must be empty there.
    pub fn start_handoff(&mut self, text: String, items: Vec<serde_json::Value>) -> io::Result<()> {
        if self.is_session_scoped() && !items.is_empty() {
            return Err(io::Error::other("This runtime has no history injection"));
        }
        self.handoff_items = (!items.is_empty()).then_some(items);
        self.claude_handoff = self.is_session_scoped();
        let result = self.send_client_message(None, text, vec![]);
        self.handoff_items = None;
        self.claude_handoff = false;
        result
    }
    pub fn submit(&mut self, id: Option<&str>) -> io::Result<()> {
        if self.remote().is_some() {
            return self
                .dispatch_remote(crate::actions::Action::Submit(id.map(str::to_owned)))
                .map(|_| ());
        }
        if let Some(thread) = id
            && self.can_resume_send(thread)
        {
            return self.resume_and_send(thread);
        }
        if self.is_session_scoped() {
            return self.claude_submit(id, None);
        }
        self.submit_operation(id, None, false)
    }
    pub(super) fn submit_operation(
        &mut self,
        id: Option<&str>,
        command: Option<ThreadCommand>,
        from_palette: bool,
    ) -> io::Result<()> {
        if !self.can_send(id) {
            return Err(io::Error::other("Wait for the thread to be ready"));
        }
        if command.is_none()
            && !from_palette
            && let Some(warning) = self.image_model_warning(id)
        {
            return Err(io::Error::other(warning));
        }
        let owner = id.map(str::to_owned);
        let draft = self.drafts.entry(owner.clone()).or_default();
        if !from_palette && draft.text.trim().is_empty() && draft.images.is_empty() {
            return Err(io::Error::other("Enter a message"));
        }
        if command.is_none() && draft.text.trim_start().starts_with('/') {
            return Err(io::Error::other("Open this command in stock Codex"));
        }
        draft.sent = Some(if from_palette {
            SentInput::PaletteCommand
        } else {
            SentInput::Draft(
                std::mem::take(&mut draft.text),
                if command.is_none() {
                    std::mem::take(&mut draft.images)
                } else {
                    vec![]
                },
            )
        });
        let submit = Submit {
            options: if let Some(id) = id {
                self.transcripts[id]
                    .send_options
                    .then(|| self.options(Some(id)))
            } else {
                self.next_options.clone()
            },
            owner,
            expected_turn: id.and_then(|id| self.transcripts[id].active_turn.clone()),
            selection: self.selection,
            command,
            inject: if id.is_none() {
                self.handoff_items.take().unwrap_or_default()
            } else {
                vec![]
            },
        };
        if id.is_none() {
            let model = submit
                .options
                .as_ref()
                .map(|s| s.model.clone())
                .or_else(|| match &self.profile.route.billing {
                    Billing::Api(alias) => Some(alias.clone()),
                    Billing::Subscription => None,
                });
            let preset = self.next_preset;
            self.request(
                Call::New {
                    preset,
                    project: self.profile.project.clone(),
                    model,
                },
                Pending::Compose(Operation::New(submit)),
            )
        } else {
            self.authorize_send(submit)
        }
    }

    fn authorize_send(&mut self, submit: Submit) -> io::Result<()> {
        if self.profile.route == Route::CODEX_SUBSCRIPTION {
            self.request(Call::Account, Pending::Compose(Operation::Account(submit)))
        } else {
            self.transmit(submit)
        }
    }
    fn transmit(&mut self, mut submit: Submit) -> io::Result<()> {
        let id = submit.owner.as_deref().expect("opened thread");
        let t = &self.transcripts[id];
        if !t.ready
            || !t.verified
            || match &submit.command {
                Some(ThreadCommand::Goal(_) | ThreadCommand::Tier(_)) => false,
                Some(_) => t.active_turn.is_some(),
                None => t.active_turn != submit.expected_turn,
            }
            || self.state != Connection::Ready
        {
            let message = "The active turn or billing route changed before sending";
            self.compose_failed(Operation::Send(submit), message.into());
            return Err(io::Error::other(message));
        }
        if submit.command.is_none() && submit.expected_turn.is_none() && !submit.inject.is_empty() {
            let items = std::mem::take(&mut submit.inject);
            return self.request(
                Call::Inject {
                    thread: id.into(),
                    items,
                },
                Pending::Compose(Operation::Inject(submit)),
            );
        }
        if let Some(command) = &submit.command {
            return self.request(
                Call::Command {
                    thread: id.into(),
                    command: command.clone(),
                },
                Pending::Compose(Operation::Send(submit)),
            );
        }
        let text = self.drafts[&submit.owner]
            .sent
            .as_ref()
            .and_then(SentInput::text)
            .expect("pending draft")
            .to_owned();
        let images = match &self.drafts[&submit.owner].sent {
            Some(SentInput::Draft(_, images)) => images.clone(),
            _ => vec![],
        };
        let call = match &submit.expected_turn {
            Some(turn) => Call::Steer {
                images,
                thread: id.into(),
                turn: turn.clone(),
                text,
            },
            None => Call::Start {
                options: submit.options.clone(),
                images,
                thread: id.into(),
                text,
            },
        };
        self.request(call, Pending::Compose(Operation::Send(submit)))
    }
    pub fn stop(&mut self, id: &str) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::Stop(id.into()));
        }
        if self.is_session_scoped() {
            return self.claude_stop(id);
        }
        let turn = self
            .transcripts
            .get(id)
            .and_then(|t| t.active_turn())
            .ok_or_else(|| io::Error::other("This thread has no active turn"))?;
        self.request(
            Call::Stop {
                thread: id.into(),
                turn: turn.into(),
            },
            Pending::Compose(Operation::Stop(id.into())),
        )
    }

    /// End this client's live process for a thread. The thread stays in the
    /// catalog and opens again (Claude `--resume`, Codex `thread/resume`) on the
    /// next select. A Claude process is dropped, which kills its process group.
    /// A Codex thread is unsubscribed; the host drops the app-server once no
    /// thread in the scope is left.
    pub fn end_session(&mut self, id: &str) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::EndSession(id.into()));
        }
        if self.is_session_scoped() {
            self.claude_end(id)?;
            self.ended_threads.insert(id.into());
            return Ok(());
        }
        if self.state != Connection::Ready {
            return Err(io::Error::other("Reconnect to end this session"));
        }
        if !self
            .transcripts
            .get(id)
            .is_some_and(|t| t.ready || t.loading)
        {
            return Err(io::Error::other("This thread has no live session"));
        }
        if self.in_terminal(id) {
            return Err(io::Error::other("This thread is open in a terminal"));
        }
        if self
            .transcripts
            .get(id)
            .is_some_and(|t| t.active_turn().is_some())
        {
            // Ending a running turn: interrupt it; the unsubscribe follows anyway.
            let _ = self.stop(id);
        }
        let t = self.transcripts.get_mut(id).expect("checked above");
        t.ready = false;
        t.verified = false;
        t.loading = false;
        t.active_turn = None;
        t.error = None;
        self.ended_threads.insert(id.into());
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
        self.request(Call::Unsubscribe(id.into()), Pending::Unsubscribe)
    }
    /// Threads with a live process or subscription in this local store. A
    /// Claude process is one session; the blank draft process is not listed.
    pub fn live_threads(&self) -> Vec<String> {
        if self.remote().is_some() {
            return vec![];
        }
        if self.is_session_scoped() {
            return self.claude_live();
        }
        self.transcripts
            .iter()
            .filter(|(id, t)| (t.ready || t.loading) && !self.terminal_threads.contains(*id))
            .map(|(id, _)| id.clone())
            .collect()
    }
    /// Whether this client can end the thread's live process (the `end` capability).
    pub fn can_end_session(&self, id: &str) -> bool {
        if let Some(remote) = self.remote() {
            return remote.can(id, "end");
        }
        self.has_live_session(id)
    }
    /// Mark a thread as having no live process (the host seeds this for a route it reopened
    /// after reaping it), so a send to it resumes it.
    pub fn mark_ended(&mut self, id: &str) {
        if self.remote().is_none() {
            self.ended_threads.insert(id.into());
        }
    }
    /// The live process was ended and nothing has started it again. The row stays
    /// in the catalog; selecting it resumes the thread. A remote client reads the
    /// host's `ended` capability.
    pub fn is_ended(&self, id: &str) -> bool {
        if let Some(remote) = self.remote() {
            return remote.can(id, "ended");
        }
        self.ended_threads.contains(id) && !self.has_live_session(id)
    }
    /// This thread has a live process or subscription to end.
    pub fn has_live_session(&self, id: &str) -> bool {
        if self.remote().is_some() {
            return false;
        }
        if self.is_session_scoped() {
            return self.claude_has_live(id);
        }
        self.transcripts
            .get(id)
            .is_some_and(|t| (t.ready || t.loading) && !self.terminal_threads.contains(id))
    }
    /// Something in this thread would be lost by ending it now.
    pub fn thread_busy(&self, id: &str) -> bool {
        self.transcript(id)
            .is_some_and(|t| t.active_turn().is_some())
            || self.sending(Some(id))
            || self.asks.iter().any(|a| a.thread.as_deref() == Some(id))
    }

    pub(super) fn compose_failed(&mut self, op: Operation, reason: String) {
        let owner = match op {
            Operation::New(s)
            | Operation::Account(s)
            | Operation::Send(s)
            | Operation::Inject(s) => {
                if let Some(d) = self.drafts.get_mut(&s.owner)
                    && let Some(SentInput::Draft(mut sent, mut images)) = d.sent.take()
                {
                    images.append(&mut d.images);
                    d.images = images;
                    if !d.text.is_empty() {
                        if !sent.is_empty() {
                            sent.push('\n');
                        }
                        sent.push_str(&d.text);
                    }
                    d.text = sent;
                }
                s.owner
            }
            Operation::Stop(id) => Some(id),
        };
        if let Some(t) = owner.and_then(|id| self.transcripts.get_mut(&id)) {
            t.error = Some(reason);
        } else {
            self.notice = Some(reason);
        }
        self.changed.mark(Changed::NOTICE);
    }
    pub(super) fn compose_reply(&mut self, op: Operation, reply: Reply) -> io::Result<()> {
        match (op, reply) {
            (Operation::New(s), Reply::Opened(reply)) => self.first_send(s, *reply),
            (Operation::Inject(s), Reply::Acknowledged) => self.transmit(s),
            (Operation::Account(s), Reply::Account(account)) if account.chatgpt() => {
                self.transmit(s)
            }
            (Operation::Account(s), Reply::Account(_)) => {
                self.state = Connection::SignInRequired;
                self.compose_failed(
                    Operation::Account(s),
                    "Sign in to ChatGPT before sending".into(),
                );
                Ok(())
            }
            (Operation::Send(s), reply) if s.command.is_some() => {
                let id = s.owner.as_deref().expect("command thread");
                let cmd = s.command.as_ref().unwrap();
                let notice = match (cmd, reply) {
                    (ThreadCommand::Goal(crate::slash::Goal::Clear), Reply::Cleared(v)) => {
                        if v.cleared {
                            "Goal cleared.".into()
                        } else {
                            "This session has no goal.".into()
                        }
                    }
                    (ThreadCommand::Goal(_), Reply::Goal(v)) => v.goal.map_or_else(
                        || "This session has no goal.".into(),
                        |g| format!("Goal ({}): {}", g.status, g.objective),
                    ),
                    (ThreadCommand::Tier(tier), Reply::Acknowledged) => {
                        self.transcripts.get_mut(id).unwrap().tier.clone_from(tier);
                        format!(
                            "Session speed set to {}.",
                            tier.as_deref().unwrap_or("default")
                        )
                    }
                    (ThreadCommand::Compact, Reply::Acknowledged) => {
                        "Codex started compacting the session.".into()
                    }
                    (ThreadCommand::Rename(name), Reply::Acknowledged) => {
                        if let Some(row) = self.catalog.get_mut(id) {
                            row.name = Some(name.clone());
                        }
                        format!("Session renamed to {name}.")
                    }
                    _ => {
                        self.compose_failed(
                            Operation::Send(s),
                            "Unexpected reply to thread command".into(),
                        );
                        return Ok(());
                    }
                };
                self.transcripts.get_mut(id).unwrap().command_notice = Some(notice);
                self.drafts.get_mut(&s.owner).unwrap().sent = None;
                Ok(())
            }
            (Operation::Send(s), Reply::Turn(turn)) if s.expected_turn.is_none() => {
                let t = self
                    .transcripts
                    .get_mut(s.owner.as_deref().unwrap())
                    .unwrap();
                if turn.status == "inProgress" && t.last_finished.as_ref() != Some(&turn.id) {
                    t.active_turn = Some(turn.id);
                }
                self.drafts.get_mut(&s.owner).unwrap().sent = None;
                Ok(())
            }
            (Operation::Send(s), Reply::Steered(turn))
                if s.expected_turn.as_ref() == Some(&turn) =>
            {
                self.drafts.get_mut(&s.owner).unwrap().sent = None;
                Ok(())
            }
            (Operation::Stop(_), Reply::Acknowledged) => Ok(()),
            (op, _) => {
                self.compose_failed(op, "Unexpected response to the turn request".into());
                Ok(())
            }
        }
    }
    fn first_send(&mut self, mut s: Submit, reply: ResumedThread) -> io::Result<()> {
        let id = reply.thread.id.clone();
        if let Some(o) = &s.options
            && self.profile.route.is_api()
        {
            self.authorized_models.insert(id.clone(), o.model.clone());
        }
        if let Err(e) = self.verify_opened(&id, &reply).and_then(|()| {
            if s.options
                .as_ref()
                .is_some_and(|wanted| wanted.model != reply.model)
            {
                Err(io::Error::other(
                    "The runtime opened a different model. Choose the intended billing route.",
                ))
            } else {
                Ok(())
            }
        }) {
            self.authorized_models.remove(&id);
            self.compose_failed(Operation::New(s), e.to_string());
            return Ok(());
        }
        if self.profile.route.is_api() {
            self.authorized_models
                .insert(id.clone(), reply.model.clone());
        }
        self.host_opened = Some(id.clone());
        let mut draft = self.drafts.remove(&None).unwrap_or_default();
        if self.selection == s.selection && self.selected.is_none() {
            self.selected = Some(id.clone());
            self.next_preset = None;
            self.next_options = None;
        } else {
            // The composer moved on. Its text and images stay together with the new chat.
            let composer = self.drafts.entry(None).or_default();
            composer.text = std::mem::take(&mut draft.text);
            composer.images = std::mem::take(&mut draft.images);
        }
        let mut row = reply.thread;
        row.model = Some(reply.model.clone());
        row.updated_at = row.updated_at.max(super::catalog::now_secs());
        self.record_row(row);
        if s.command.is_none() {
            self.prompt_preview(&id, super::catalog::PromptSource::FirstTurn, || {
                draft
                    .sent
                    .as_ref()
                    .and_then(SentInput::text)
                    .and_then(crate::codex::history::first_line)
            });
        }
        if let Some(text) = draft.sent.as_ref().and_then(SentInput::text) {
            self.first_title_prompt(&id, text);
        }
        let t = self.transcripts.entry(id.clone()).or_default();
        t.model = Some(reply.model);
        t.tier = reply.service_tier;
        t.effort = reply.reasoning_effort;
        if let Some(o) = &s.options {
            t.send_options = true;
            t.effort = o.effort.clone();
            t.tier = o.tier.clone();
        }
        t.access = Some(reply.access);
        t.verified = true;
        t.ready = true;
        s.owner = Some(id);
        self.drafts.insert(s.owner.clone(), draft);
        self.authorize_send(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::frame::{Frame, RequestId, RpcError};
    use crate::links::codex::{Launch, Link, LinkEvent};
    use crate::mentions::ImageInput;
    use crate::store::Profile;

    fn ready() -> Store {
        let directory = std::env::temp_dir();
        let link = Link::spawn(
            Launch {
                executable: "/bin/cat".into(),
                arguments: vec![],
                directory: directory.clone(),
                path: "/usr/bin:/bin".into(),
            },
            |_| {},
        )
        .unwrap();
        let mut store = Store::open(
            Profile {
                project: directory,
                route: Route::CODEX_SUBSCRIPTION,
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        store.state = Connection::Ready;
        store
    }

    #[test]
    fn ending_a_ready_codex_thread_unsubscribes_and_marks_it_not_ready() {
        let mut store = ready();
        let id = "thread-ready".to_owned();
        let t = store.transcripts.entry(id.clone()).or_default();
        t.ready = true;
        t.verified = true;
        t.active_turn = Some("turn-1".into());
        assert!(store.can_end_session(&id));
        store.end_session(&id).unwrap();
        let t = store.transcripts.get(&id).unwrap();
        assert!(!t.ready && !t.verified && !t.loading);
        assert!(t.active_turn().is_none(), "an ended turn is not running");
        assert!(
            store
                .pending
                .values()
                .any(|(_, pending)| matches!(pending, Pending::Unsubscribe)),
            "the host must receive the unsubscribe"
        );
        assert!(store.is_ended(&id));
        assert!(!store.can_end_session(&id));
        assert!(store.end_session(&id).is_err());
    }

    #[test]
    fn text_only_model_preserves_image_for_the_next_choice() {
        let mut store = ready();
        let mut caps = crate::slash::Capabilities::default();
        caps.add_model(
            serde_json::from_value(serde_json::json!({
                "id":"fixture/plain","isDefault":true,"inputModalities":["text"]
            }))
            .unwrap(),
        );
        store.install_command_schema(caps).unwrap();
        store.attach_image(None, ImageInput::File("/tmp/art.png".into()));
        let error = store.submit(None).unwrap_err();
        assert!(error.to_string().contains("accepts text only"));
        assert_eq!(store.draft_images(None)[0].label(), "art.png");
        assert!(!store.sending(None));
    }

    #[test]
    fn new_chat_images_stay_with_its_text_when_the_thread_opens_elsewhere() {
        let mut store = ready();
        let project = store.profile.project.clone();
        let submit = Submit {
            options: None,
            owner: None,
            expected_turn: None,
            selection: store.selection,
            command: None,
            inject: vec![],
        };
        // While the thread opens, the new-chat composer gets text and an image; the operator
        // then selects another thread, so the opened thread must not take either.
        store.edit(None, "typed while pending".into());
        store.attach_image(None, ImageInput::File("/tmp/next.png".into()));
        store.selection += 1;
        let reply: ResumedThread = serde_json::from_value(serde_json::json!({
            "thread": {"id":"new-thread","cwd":project,"modelProvider":"openai"},
            "modelProvider":"openai",
            "model":"fixture/model",
            "serviceTier":null,
            "reasoningEffort":null,
            "initialTurnsPage":null
        }))
        .unwrap();
        store.first_send(submit, reply).unwrap();
        assert_eq!(store.draft(None), "typed while pending");
        assert_eq!(
            store
                .draft_images(None)
                .iter()
                .map(ImageInput::label)
                .collect::<Vec<_>>(),
            ["next.png"]
        );
        assert_eq!(store.draft(Some("new-thread")), "");
        assert!(store.draft_images(Some("new-thread")).is_empty());
    }

    #[test]
    fn image_only_failure_restores_sent_files_before_edits_made_in_flight() {
        let mut store = ready();
        store.attach_image(None, ImageInput::File("/tmp/first.png".into()));
        store.submit(None).unwrap();
        assert!(store.draft_images(None).is_empty());
        store.edit(None, "in flight".into());
        store.attach_image(None, ImageInput::File("/tmp/second.png".into()));
        store.apply(LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(store.seq),
            result: Err(RpcError {
                code: -1,
                message: "fixture".into(),
                data: None,
            }),
        }));
        assert_eq!(store.draft(None), "in flight");
        assert_eq!(
            store
                .draft_images(None)
                .iter()
                .map(ImageInput::label)
                .collect::<Vec<_>>(),
            ["first.png", "second.png"]
        );
        store.remove_image(None, 0);
        assert_eq!(store.draft_images(None)[0].label(), "second.png");
        assert!(store.draft_images(Some("another-thread")).is_empty());
    }

    #[test]
    fn sending_to_an_ended_codex_thread_reopens_it_then_delivers_the_message() {
        let mut store = ready();
        let id = "thread-ended".to_owned();
        let row: crate::codex::history::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": id, "cwd": store.profile.project, "modelProvider": "openai", "preview": "hi"
        }))
        .unwrap();
        store.catalog.insert(row);
        let t = store.transcripts.entry(id.clone()).or_default();
        t.ready = true;
        t.verified = true;
        store.end_session(&id).unwrap();
        // The composer stays usable, and a plain send is accepted rather than refused.
        assert!(store.is_ended(&id) && !store.can_send(Some(&id)));
        assert!(store.can_compose(Some(&id)));
        store.edit(Some(&id), "back again".into());
        store.submit(Some(&id)).unwrap();
        assert!(store.sending(Some(&id)), "the message waits for the resume");
        assert!(!store.is_ended(&id), "live again, so no ended marker");
        assert_eq!(store.draft(Some(&id)), "back again");
        assert!(
            store
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::Read(t) if t == &id)),
            "the thread is read and resumed"
        );
        // Still resuming: nothing is sent yet.
        store.drain(vec![], std::time::Instant::now());
        assert!(store.sending(Some(&id)));
        // Once the thread is verified and ready the queued message is submitted.
        let t = store.transcripts.get_mut(&id).unwrap();
        t.loading = false;
        t.ready = true;
        t.verified = true;
        store.drain(vec![], std::time::Instant::now());
        assert!(
            store.pending.values().any(|(_, p)| matches!(
                p,
                Pending::Compose(Operation::Account(s)) if s.owner.as_deref() == Some(id.as_str())
            )),
            "the draft is submitted"
        );
        assert_eq!(store.draft(Some(&id)), "");
    }

    #[test]
    fn a_failed_resume_keeps_the_message_in_the_composer() {
        let mut store = ready();
        let id = "thread-gone".to_owned();
        let row: crate::codex::history::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": id, "cwd": store.profile.project, "modelProvider": "openai"
        }))
        .unwrap();
        store.catalog.insert(row);
        store.mark_ended(&id);
        store.edit(Some(&id), "keep me".into());
        store.submit(Some(&id)).unwrap();
        let t = store.transcripts.get_mut(&id).unwrap();
        t.loading = false;
        t.error = Some("gone".into());
        store.drain(vec![], std::time::Instant::now());
        assert!(!store.sending(Some(&id)));
        assert_eq!(store.draft(Some(&id)), "keep me");
        assert!(store.is_ended(&id));
    }
}
