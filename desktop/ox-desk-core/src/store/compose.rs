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
        self.drafts
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
            self.edit(None, std::mem::take(&mut draft.text));
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
}
