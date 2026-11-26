//! The "Start worker" and "Continue in…" sheets. They dispatch the same `StartWorker` and
//! `StartHandoff` actions as the `/worker` and `/handoff` commands. The commands stay as
//! shortcuts; these sheets show the same choices as fields.
use super::handoff_sheet::{self, HandoffDraft};
use super::model_picker::{ModelPicker, ModelPickerEvent};
use super::workbench::Workbench;
use crate::appearance::Palette;
use crate::appearance::Step;
use crate::kit::{button, col, icon_button, row, small, toolbar_button};
use gpui::*;
use gpui_component::input::{Textarea, TextareaState};
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::modes::{Level, ModeRequest};
use ox_desk_core::origin::{WorkerMode, WorkerStart};
use ox_desk_core::runtime::{Billing, Route, Runtime};
use ox_desk_core::store::Connection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LaunchKind {
    Worker,
    Handoff,
}

/// The fields both sheets share: the session launched from, the target runtime and billing,
/// the permission ceiling, and the last validation message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Choice {
    pub owner: String,
    pub runtime: Runtime,
    /// An OX API alias, or none for the runtime's subscription.
    pub model: Option<String>,
    pub permission: ModeRequest,
    pub error: Option<String>,
}

impl Choice {
    /// Starts from the route the session is on, so the common case needs no edits.
    fn new(owner: String, route: &Route) -> Self {
        Self {
            owner,
            runtime: route.runtime,
            model: match &route.billing {
                Billing::Api(alias) => Some(alias.clone()),
                Billing::Subscription => None,
            },
            permission: ModeRequest::Inherit,
            error: None,
        }
    }
}

pub(super) struct WorkerDraft {
    pub choice: Choice,
    pub mode: WorkerMode,
    pub task: Entity<TextareaState>,
}

pub(super) enum Launch {
    Worker(WorkerDraft),
    Handoff(HandoffDraft),
}

impl Launch {
    pub(super) fn kind(&self) -> LaunchKind {
        match self {
            Self::Worker(_) => LaunchKind::Worker,
            Self::Handoff(_) => LaunchKind::Handoff,
        }
    }
    pub(super) fn choice(&self) -> &Choice {
        match self {
            Self::Worker(draft) => &draft.choice,
            Self::Handoff(draft) => &draft.choice,
        }
    }
    fn choice_mut(&mut self) -> &mut Choice {
        match self {
            Self::Worker(draft) => &mut draft.choice,
            Self::Handoff(draft) => &mut draft.choice,
        }
    }
}

/// Modes a runtime can run: Goal is Codex's, Workflow is Claude Code's, Session runs anywhere.
pub(super) fn mode_allowed(runtime: Runtime, mode: WorkerMode) -> bool {
    match mode {
        WorkerMode::Session => true,
        WorkerMode::Goal => runtime == Runtime::Codex,
        WorkerMode::Workflow => runtime == Runtime::Claude,
    }
}

/// Inherit is sent as an absent field, the same as a command without `--mode`.
pub(super) fn permission_field(request: ModeRequest) -> Option<ModeRequest> {
    (request != ModeRequest::Inherit).then_some(request)
}

/// The one-line consequence shown under the permission choices.
pub(super) fn permission_note(request: ModeRequest, runtime: Runtime) -> String {
    match request {
        ModeRequest::Inherit => {
            "Uses the session's own permission mode and cannot exceed it.".into()
        }
        ModeRequest::Level(Level::Yolo) => {
            format!(
                "{} runs with no review of its actions. Only choose it for work you trust.",
                runtime.name()
            )
        }
        ModeRequest::Level(level) => {
            format!("The worker cannot go above {}.", level.label(runtime))
        }
    }
}

/// Trims a model alias and treats an empty one as the subscription.
pub(super) fn model_or_none(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
}

/// Whether a start counts as YOLO-confirmed. A human choosing YOLO in the clearly labelled
/// control is the consent (the consequence line stays under it); only an agent asking for YOLO
/// needs a human tap, and that path does not come through these sheets.
pub(super) fn yolo_confirmed(permission: ModeRequest) -> bool {
    permission == ModeRequest::Level(Level::Yolo)
}

/// Sets the permission and clears any validation message.
pub(super) fn set_permission(choice: &mut Choice, permission: ModeRequest) {
    choice.permission = permission;
    choice.error = None;
}

/// Builds the start the worker sheet sends. It applies the same checks as `/worker`.
pub(super) fn worker_start(
    owner: &str,
    runtime: Runtime,
    mode: WorkerMode,
    permission: ModeRequest,
    model: Option<&str>,
    task: &str,
) -> Result<WorkerStart, String> {
    let task = task.trim();
    if task.is_empty() {
        return Err("Write the task for the worker.".into());
    }
    let yolo = yolo_confirmed(permission);
    let start = WorkerStart {
        host: None,
        parent: owner.to_owned(),
        runtime,
        model: model_or_none(model),
        mode,
        task: task.to_owned(),
        write: None,
        permission: permission_field(permission),
        checkout: None,
        yolo_confirmed: yolo,
        max_minutes: None,
        subscription_model: None,
    };
    start.validate()?;
    Ok(start)
}

fn mode_label(mode: WorkerMode) -> &'static str {
    match mode {
        WorkerMode::Session => "Session",
        WorkerMode::Goal => "Goal",
        WorkerMode::Workflow => "Workflow",
    }
}

fn mode_note(mode: WorkerMode) -> &'static str {
    match mode {
        WorkerMode::Session => "The task starts a new session under this one.",
        WorkerMode::Goal => "Codex keeps working toward the task as an objective.",
        WorkerMode::Workflow => "Claude Code runs the task with its Workflow tool.",
    }
}

fn permission_label(request: ModeRequest, runtime: Runtime) -> String {
    match request {
        ModeRequest::Inherit => "Inherit".into(),
        ModeRequest::Level(level) => level.label(runtime).to_owned(),
    }
}

/// "Claude subscription", with the runtime's default model when this session's runtime reports it.
fn subscription_label(runtime: Runtime, default: Option<String>) -> String {
    let account = format!("{} subscription", runtime.account());
    match default {
        Some(name) => format!("{account} · {name}"),
        None => account,
    }
}

fn field(p: Palette, label: &'static str, control: impl IntoElement) -> Div {
    col().gap_1().child(small(p, label)).child(control)
}

impl Workbench {
    pub(super) fn request_launch(&mut self, kind: LaunchKind, cx: &mut Context<Self>) {
        self.launch_request = Some(kind);
        cx.notify();
    }

    /// Opens a sheet for the selected session. It needs the window, so the request is taken in draw.
    pub(super) fn open_launch(
        &mut self,
        kind: LaunchKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.launch_request = None;
        let Some(store) = &self.store else { return };
        let Some(owner) = store.selected().map(str::to_owned) else {
            return;
        };
        if store.state() != &Connection::Ready || store.in_terminal(&owner) {
            return;
        }
        // The sheet starts from the route this session is on, not the window's route.
        let route = store.thread_route(Some(&owner));
        let choice = Choice::new(owner, &route);
        let launch = match kind {
            LaunchKind::Worker => {
                let task = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(3, 8)
                        .placeholder("What should the worker do?")
                });
                task.read(cx).focus_handle(cx).focus(window, cx);
                Launch::Worker(WorkerDraft {
                    choice,
                    mode: WorkerMode::Session,
                    task,
                })
            }
            LaunchKind::Handoff => {
                let request = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(3, 8)
                        .placeholder("What should the new session do?")
                });
                request.read(cx).focus_handle(cx).focus(window, cx);
                Launch::Handoff(HandoffDraft { choice, request })
            }
        };
        self.launch = Some(launch);
        self.launch_picker = None;
        self.model_picker = None;
        cx.notify();
    }

    pub(super) fn close_launch(&mut self, cx: &mut Context<Self>) {
        self.launch = None;
        self.launch_picker = None;
        cx.notify();
    }

    fn set_launch_choice(&mut self, edit: impl FnOnce(&mut Choice), cx: &mut Context<Self>) {
        if let Some(launch) = self.launch.as_mut() {
            let choice = launch.choice_mut();
            edit(choice);
            choice.error = None;
        }
        cx.notify();
    }

    /// Point the open sheet at a runtime and API alias (none: its subscription).
    pub(super) fn set_launch_route(
        &mut self,
        runtime: Runtime,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.set_launch_runtime(runtime, cx);
        self.set_launch_choice(|c| c.model = model, cx);
    }

    pub(super) fn set_launch_runtime(&mut self, runtime: Runtime, cx: &mut Context<Self>) {
        self.set_launch_choice(|c| c.runtime = runtime, cx);
        if let Some(Launch::Worker(draft)) = self.launch.as_mut()
            && !mode_allowed(runtime, draft.mode)
        {
            draft.mode = WorkerMode::Session;
        }
    }

    pub(super) fn set_launch_permission(&mut self, request: ModeRequest, cx: &mut Context<Self>) {
        self.set_launch_choice(|c| set_permission(c, request), cx);
    }

    pub(super) fn set_launch_mode(&mut self, mode: WorkerMode, cx: &mut Context<Self>) {
        if let Some(Launch::Worker(draft)) = self.launch.as_mut()
            && mode_allowed(draft.choice.runtime, mode)
        {
            draft.mode = mode;
            draft.choice.error = None;
        }
        cx.notify();
    }

    /// `None` chooses the runtime's subscription.
    pub(super) fn choose_launch_model(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        self.set_launch_choice(|c| c.model = model, cx);
        self.launch_picker = None;
        cx.notify();
    }

    pub(super) fn toggle_launch_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(launch) = &self.launch else { return };
        let kind = launch.kind();
        if self
            .launch_picker
            .as_ref()
            .is_some_and(|(open, _, _)| *open == kind)
        {
            self.launch_picker = None;
            cx.notify();
            return;
        }
        // The target is an OX API alias, so the source session's own native command models
        // (subscription names like "default") are not candidates. Any catalog model may be
        // chosen, starred or not: the picker lists settings and the discovered catalog.
        let picker =
            cx.new(|cx| ModelPicker::new(self.link.clone(), vec![], window, cx).with_catalog(cx));
        let subscription = cx.subscribe_in(
            &picker,
            window,
            |s: &mut Workbench, _, event: &ModelPickerEvent, _, cx| match event {
                ModelPickerEvent::Choose(id) => s.choose_launch_model(Some(id.clone()), cx),
                ModelPickerEvent::Close => {
                    s.launch_picker = None;
                    cx.notify();
                }
                ModelPickerEvent::Starred(_) | ModelPickerEvent::HandOff(_) => {}
                ModelPickerEvent::AddProvider => cx.emit(super::route_chip::OpenProviders),
            },
        );
        self.launch_picker = Some((kind, picker, subscription));
        cx.notify();
    }

    /// Dispatches the same action the command would, or shows why the fields cannot start it.
    pub(super) fn submit_launch(&mut self, cx: &mut Context<Self>) {
        let result = match self.launch.as_ref() {
            Some(Launch::Worker(draft)) => {
                let task = draft.task.read(cx).value().to_string();
                let c = &draft.choice;
                worker_start(
                    &c.owner,
                    c.runtime,
                    draft.mode,
                    c.permission,
                    c.model.as_deref(),
                    &task,
                )
                .map(DeskAction::StartWorker)
            }
            Some(Launch::Handoff(draft)) => {
                let request = draft.request.read(cx).value().to_string();
                handoff_sheet::handoff_start(&draft.choice, &request).map(DeskAction::StartHandoff)
            }
            None => return,
        };
        match result {
            Ok(action) => {
                self.close_launch(cx);
                self.action(cx, action);
            }
            Err(message) => {
                if let Some(launch) = self.launch.as_mut() {
                    launch.choice_mut().error = Some(message);
                }
                cx.notify();
            }
        }
    }

    fn runtime_row(&self, p: Palette, choice: &Choice, cx: &mut Context<Self>) -> Div {
        let mut chips = row().gap_2().flex_wrap();
        for (id, runtime) in [
            ("launch-runtime-codex", Runtime::Codex),
            ("launch-runtime-claude", Runtime::Claude),
        ] {
            chips = chips.child(button(
                p,
                id,
                runtime.name(),
                choice.runtime == runtime,
                true,
                cx.listener(move |s, _, _, cx| s.set_launch_runtime(runtime, cx)),
            ));
        }
        field(p, "Runtime", chips)
    }

    fn model_row(
        &self,
        p: Palette,
        kind: LaunchKind,
        choice: &Choice,
        cx: &mut Context<Self>,
    ) -> Div {
        // A Codex worker needs an OX API model; a handoff may use either subscription.
        let subscription_ok = kind == LaunchKind::Handoff || choice.runtime == Runtime::Claude;
        let model_label = match &choice.model {
            Some(model) => format!(
                "{} · {}",
                choice.runtime.name(),
                ox_desk_core::slash::readable_model(model)
            ),
            None => "Choose a model…".into(),
        };
        let default = self
            .store
            .as_ref()
            .filter(|s| s.profile().route.runtime == choice.runtime)
            .and_then(|store| {
                let caps = store.command_caps(Some(&choice.owner));
                let id = caps.default_model()?.to_owned();
                Some(caps.model_display(&id))
            });
        let chips = row()
            .gap_2()
            .flex_wrap()
            .child(button(
                p,
                "launch-subscription",
                subscription_label(choice.runtime, default),
                choice.model.is_none(),
                subscription_ok,
                cx.listener(|s, _, _, cx| s.choose_launch_model(None, cx)),
            ))
            .child(button(
                p,
                "launch-model",
                model_label,
                choice.model.is_some(),
                true,
                cx.listener(|s, _, window, cx| s.toggle_launch_picker(window, cx)),
            ));
        let mut control = col().gap_1().child(chips);
        if !subscription_ok {
            control = control.child(small(p, "Codex workers need an OX API model."));
        }
        field(p, "Model", control)
    }

    fn permission_row(&self, p: Palette, choice: &Choice, cx: &mut Context<Self>) -> Div {
        let runtime = choice.runtime;
        let mut chips = row().gap_2().flex_wrap().child(button(
            p,
            "launch-permission-inherit",
            permission_label(ModeRequest::Inherit, runtime),
            choice.permission == ModeRequest::Inherit,
            true,
            cx.listener(|s, _, _, cx| s.set_launch_permission(ModeRequest::Inherit, cx)),
        ));
        for (i, level) in Level::ALL.into_iter().enumerate() {
            let request = ModeRequest::Level(level);
            chips = chips.child(button(
                p,
                ("launch-permission", i),
                permission_label(request, runtime),
                choice.permission == request,
                true,
                cx.listener(move |s, _, _, cx| s.set_launch_permission(request, cx)),
            ));
        }
        let note_color = if choice.permission == ModeRequest::Level(Level::Yolo) {
            p.warning
        } else {
            p.muted
        };
        field(
            p,
            "Permission ceiling",
            col().gap_1().child(chips).child(
                div()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(note_color))
                    .child(permission_note(choice.permission, runtime)),
            ),
        )
    }

    fn mode_row(&self, p: Palette, draft: &WorkerDraft, cx: &mut Context<Self>) -> Div {
        let mut chips = row().gap_2().flex_wrap();
        for (i, mode) in [WorkerMode::Session, WorkerMode::Goal, WorkerMode::Workflow]
            .into_iter()
            .enumerate()
        {
            chips = chips.child(button(
                p,
                ("launch-mode", i),
                mode_label(mode),
                draft.mode == mode,
                mode_allowed(draft.choice.runtime, mode),
                cx.listener(move |s, _, _, cx| s.set_launch_mode(mode, cx)),
            ));
        }
        field(
            p,
            "Mode",
            col()
                .gap_1()
                .child(chips)
                .child(small(p, mode_note(draft.mode))),
        )
    }

    /// The sheet above the composer, for whichever launch is open on the selected session.
    pub(super) fn launch_sheet(
        &self,
        max_height: Pixels,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = Palette::get(cx);
        let launch = self.launch.as_ref()?;
        let kind = launch.kind();
        let choice = launch.choice().clone();
        let picker = self
            .launch_picker
            .as_ref()
            .filter(|(open, _, _)| *open == kind)
            .map(|(_, picker, _)| picker.clone());
        let parent = self
            .store
            .as_ref()
            .and_then(|store| store.catalog_row(&choice.owner))
            .map(|row| row.title().to_owned())
            .unwrap_or_else(|| "this session".into());
        let (heading, submit, context) = match kind {
            LaunchKind::Worker => (
                "Start worker",
                "Start worker",
                format!("Runs as a child of {parent}."),
            ),
            LaunchKind::Handoff => (
                "Continue in…",
                "Continue",
                format!("Starts a new session from {parent} on another runtime or model."),
            ),
        };
        let header = row()
            .justify_between()
            .gap_2()
            .child(
                col()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .text_size(p.text(Step::Body))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(p.fg))
                            .child(heading),
                    )
                    .child(small(p, context)),
            )
            .child(icon_button(
                p,
                "launch-close",
                "×",
                "Close",
                cx.listener(|s, _, _, cx| s.close_launch(cx)),
            ));

        let mut body = col()
            .gap_3()
            .child(header)
            .child(self.runtime_row(p, &choice, cx));
        match launch {
            Launch::Worker(draft) => {
                body = body
                    .child(self.mode_row(p, draft, cx))
                    .child(self.model_row(p, kind, &choice, cx));
            }
            Launch::Handoff(_) => {
                body = body.child(self.model_row(p, kind, &choice, cx));
            }
        }
        if let Some(picker) = picker {
            body = body.child(
                col()
                    .p_2()
                    .border_1()
                    .border_color(rgb(p.line))
                    .bg(rgb(p.bg))
                    .child(picker),
            );
        }
        body = body.child(self.permission_row(p, &choice, cx));
        match launch {
            Launch::Worker(draft) => {
                body = body.child(field(p, "Task", Textarea::new(&draft.task)));
            }
            Launch::Handoff(draft) => {
                body = body.child(field(p, "Request", Textarea::new(&draft.request)));
            }
        }
        if let Some(error) = &choice.error {
            body = body.child(
                div()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.error))
                    .child(error.clone()),
            );
        }
        let footer = row()
            .justify_end()
            .gap_2()
            .child(toolbar_button(
                p,
                "launch-cancel",
                "Cancel",
                true,
                cx.listener(|s, _, _, cx| s.close_launch(cx)),
            ))
            .child(button(
                p,
                "launch-submit",
                submit,
                true,
                true,
                cx.listener(|s, _, _, cx| s.submit_launch(cx)),
            ));
        let frame = col()
            .id("launch-sheet")
            .w_full()
            .flex_shrink_0()
            .max_h(max_height)
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .border_t_2()
            .border_color(rgb(p.line_strong))
            .bg(rgb(p.panel))
            .on_key_down(cx.listener(|s, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    s.close_launch(cx);
                    cx.stop_propagation();
                }
            }));
        Some(frame.child(body).child(footer).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Choice, Level, ModeRequest, WorkerMode, mode_allowed, permission_field, permission_note,
        set_permission, worker_start, yolo_confirmed,
    };
    use ox_desk_core::runtime::Runtime;

    #[test]
    fn goal_is_codex_only_and_workflow_is_claude_only() {
        assert!(mode_allowed(Runtime::Codex, WorkerMode::Goal));
        assert!(!mode_allowed(Runtime::Claude, WorkerMode::Goal));
        assert!(mode_allowed(Runtime::Claude, WorkerMode::Workflow));
        assert!(!mode_allowed(Runtime::Codex, WorkerMode::Workflow));
        for runtime in [Runtime::Codex, Runtime::Claude] {
            assert!(mode_allowed(runtime, WorkerMode::Session));
        }
    }

    #[test]
    fn inherit_is_an_absent_permission_field() {
        assert_eq!(permission_field(ModeRequest::Inherit), None);
        assert_eq!(
            permission_field(ModeRequest::Level(Level::Read)),
            Some(ModeRequest::Level(Level::Read))
        );
    }

    #[test]
    fn empty_task_is_refused_before_anything_is_sent() {
        for task in ["", "   ", "\n\t"] {
            let error = worker_start(
                "s1",
                Runtime::Claude,
                WorkerMode::Session,
                ModeRequest::Inherit,
                None,
                task,
            )
            .unwrap_err();
            assert_eq!(error, "Write the task for the worker.");
        }
    }

    #[test]
    fn task_is_trimmed_and_choosing_yolo_is_the_confirmation() {
        let start = worker_start(
            "s1",
            Runtime::Claude,
            WorkerMode::Session,
            ModeRequest::Level(Level::Yolo),
            Some("  "),
            "  fix the build \n",
        )
        .unwrap();
        assert_eq!(start.task, "fix the build");
        assert_eq!(start.model, None);
        assert!(start.yolo_confirmed);
        assert_eq!(start.permission, Some(ModeRequest::Level(Level::Yolo)));
        assert_eq!(start.parent, "s1");
    }

    #[test]
    fn yolo_is_confirmed_only_when_yolo_is_chosen() {
        assert!(yolo_confirmed(ModeRequest::Level(Level::Yolo)));
        assert!(!yolo_confirmed(ModeRequest::Level(Level::Read)));
        assert!(!yolo_confirmed(ModeRequest::Inherit));
    }

    #[test]
    fn changing_permission_clears_the_error() {
        let mut choice = Choice {
            owner: "s1".into(),
            runtime: Runtime::Claude,
            model: None,
            permission: ModeRequest::Level(Level::Yolo),
            error: Some("old".into()),
        };
        set_permission(&mut choice, ModeRequest::Level(Level::Classifier));
        assert_eq!(choice.error, None);
    }

    #[test]
    fn codex_worker_needs_a_model_and_goal_needs_codex() {
        let needs_model = worker_start(
            "s1",
            Runtime::Codex,
            WorkerMode::Session,
            ModeRequest::Inherit,
            None,
            "task",
        );
        assert!(needs_model.is_err());
        let goal_on_claude = worker_start(
            "s1",
            Runtime::Claude,
            WorkerMode::Goal,
            ModeRequest::Inherit,
            None,
            "task",
        );
        assert!(goal_on_claude.is_err());
        let goal_on_codex = worker_start(
            "s1",
            Runtime::Codex,
            WorkerMode::Goal,
            ModeRequest::Inherit,
            Some("gpt-x"),
            "task",
        )
        .unwrap();
        assert_eq!(goal_on_codex.mode, WorkerMode::Goal);
        assert_eq!(goal_on_codex.model.as_deref(), Some("gpt-x"));
    }

    #[test]
    fn yolo_note_names_the_consequence() {
        assert!(
            permission_note(ModeRequest::Level(Level::Yolo), Runtime::Claude).contains("no review")
        );
    }
}
