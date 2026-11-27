//! The "Continue in…" sheet's draft and checks. The sheet itself is drawn by `worker_sheet`,
//! which shares its runtime, model and permission fields with the worker sheet.
use super::workbench::Workbench;
use super::worker_sheet::{Choice, model_or_none, yolo_confirmed};
use crate::app::fleet::HandoffDestination;
use gpui::{App, AppContext, Context, Entity, EventEmitter};
use gpui_component::input::TextareaState;
use ox_desk_core::handoff::HandoffStart;
use ox_desk_core::host_protocol::Scope;

pub(super) struct HandoffDraft {
    pub destination: Option<HandoffDestination>,
    pub choice: Choice,
    pub request: Entity<TextareaState>,
}

/// Builds the start the sheet sends. It applies the same checks as `/handoff`.
pub(super) fn handoff_start(choice: &Choice, request: &str) -> Result<HandoffStart, String> {
    let yolo = yolo_confirmed(choice.permission);
    let start = HandoffStart {
        host: None,
        source: choice.owner.clone(),
        runtime: choice.runtime,
        model: model_or_none(choice.model.as_deref()),
        mode: choice.permission,
        request: request.trim().to_owned(),
        yolo_confirmed: yolo,
    };
    start.validate()?;
    Ok(start)
}

pub(super) struct HandoffReady {
    pub registration: gpui::EntityId,
    pub host: Option<String>,
    pub scope: Scope,
    pub session: Option<String>,
}
impl EventEmitter<HandoffReady> for Workbench {}

impl Workbench {
    pub(super) fn handoff_destinations(&self, cx: &App) -> Vec<HandoffDestination> {
        self.workspace
            .as_ref()
            .and_then(|w| w.upgrade())
            .map(|w| {
                w.read(cx)
                    .handoff_destinations(&self.link, &self.project, cx)
            })
            .unwrap_or_default()
    }
    pub(super) fn select_handoff_destination(
        &self,
        selector: &str,
        cx: &App,
    ) -> Result<Option<HandoffDestination>, String> {
        let destinations = self.handoff_destinations(cx);
        let mut found = destinations.into_iter().filter(|h| {
            h.name.eq_ignore_ascii_case(selector)
                || (selector.len() >= 4 && h.id.starts_with(selector))
                || h.link
                    .remote()
                    .is_some_and(|r| r.name.eq_ignore_ascii_case(selector))
                || (selector == "local" && h.link.remote().is_none())
        });
        let current = self.link.remote();
        let local_alias = selector.eq_ignore_ascii_case("local");
        let current_matches = (local_alias && current.is_none())
            || current.is_some_and(|h| {
                h.name.eq_ignore_ascii_case(selector)
                    || (selector.len() >= 4 && h.id.starts_with(selector))
            })
            || self
                .host_caption
                .as_ref()
                .is_some_and(|name| name.eq_ignore_ascii_case(selector));
        let Some(selected) = found.next() else {
            return if current_matches {
                Ok(None)
            } else {
                Err(format!(
                    "No connected @{selector} checkout matches this repository."
                ))
            };
        };
        if current_matches || found.next().is_some() {
            return Err(format!(
                "Several checkouts match @{selector}. Choose one in Continue in…"
            ));
        }
        Ok(Some(selected))
    }
    pub(super) fn send_handoff(
        &mut self,
        start: HandoffStart,
        to: HandoffDestination,
        cx: &mut Context<Self>,
    ) {
        if self.handoff_pending {
            return;
        }
        if !self.handoff_destinations(cx).iter().any(|h| {
            h.registration == to.registration
                && h.id == to.id
                && h.project == to.project
                && h.repository == to.repository
        }) {
            self.launch_error = Some("The destination checkout is no longer connected.".into());
            cx.notify();
            return;
        }
        let source = Scope {
            project: self.project.clone(),
            route: self
                .store
                .as_ref()
                .map(|s| s.thread_route(Some(&start.source)))
                .unwrap_or_else(|| self.route.clone()),
        };
        let link = self.link.clone();
        let host = to.link.remote().map(|h| h.id.clone());
        let registration = to.registration;
        let owner = start.source.clone();
        let initial_draft = self
            .store
            .as_ref()
            .map(|s| s.draft(Some(&owner)).to_owned());
        let (notice, notices) = flume::bounded(1);
        let approval_scope = Scope {
            project: to.project.clone(),
            route: start.route(),
        };
        let approval_host = host.clone();
        cx.spawn(async move |this, cx| {
            if notices.recv_async().await.is_ok() {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(HandoffReady {
                        registration,
                        host: approval_host,
                        scope: approval_scope,
                        session: None,
                    })
                });
            }
        })
        .detach();
        self.handoff_pending = true;
        self.launch_error = Some(format!("Starting handoff on {}…", to.name));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    link.handoff(source, start, &to, || {
                        let _ = notice.try_send(());
                    })
                })
                .await;
            let _ = this.update(cx, |s, cx| {
                s.handoff_pending = false;
                match result {
                    Ok((scope, session)) => {
                        if initial_draft.as_ref().is_some_and(|draft| {
                            draft.trim_start().starts_with("/handoff ")
                                && s.store
                                    .as_ref()
                                    .is_some_and(|st| st.draft(Some(&owner)) == draft)
                        }) {
                            s.action(
                                cx,
                                ox_desk_core::actions::Action::DraftEdit(
                                    Some(owner),
                                    String::new(),
                                ),
                            );
                        }
                        s.close_launch(cx);
                        s.launch_error = None;
                        cx.emit(HandoffReady {
                            registration,
                            host,
                            scope,
                            session: Some(session),
                        });
                    }
                    Err(e) => {
                        if let Some(super::worker_sheet::Launch::Handoff(draft)) = &mut s.launch {
                            draft.choice.error = Some(e.to_string());
                        }
                        s.launch_error = Some(e.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::handoff_start;
    use crate::views::worker_sheet::Choice;
    use ox_desk_core::modes::{Level, ModeRequest};
    use ox_desk_core::runtime::Runtime;

    fn choice(runtime: Runtime, model: Option<&str>, permission: ModeRequest) -> Choice {
        Choice {
            owner: "s1".into(),
            runtime,
            model: model.map(str::to_owned),
            permission,
            error: None,
        }
    }

    #[test]
    fn empty_request_is_refused() {
        for request in ["", "  \n"] {
            let error = handoff_start(&choice(Runtime::Codex, None, ModeRequest::Inherit), request)
                .unwrap_err();
            assert_eq!(error, "Write the request for the new session.");
        }
    }

    #[test]
    fn request_is_trimmed_and_subscription_is_allowed() {
        let start = handoff_start(
            &choice(Runtime::Claude, Some(" "), ModeRequest::Level(Level::Read)),
            "  summarise the diff  ",
        )
        .unwrap();
        assert_eq!(start.request, "summarise the diff");
        assert_eq!(start.source, "s1");
        assert_eq!(start.model, None);
        assert_eq!(start.mode, ModeRequest::Level(Level::Read));
        assert!(!start.yolo_confirmed);
    }

    #[test]
    fn yolo_handoff_records_the_confirmation() {
        let start = handoff_start(
            &choice(
                Runtime::Codex,
                Some("alias"),
                ModeRequest::Level(Level::Yolo),
            ),
            "go",
        )
        .unwrap();
        assert!(start.yolo_confirmed);
        assert_eq!(start.model.as_deref(), Some("alias"));
    }
}
