//! Local fixture for the native move flow. All control and probe wiring is test-only.
//! The production host must publish the fenced state and verify destination activation
//! before this presentation can be used outside the fixture.
use super::workbench::Workbench;
use crate::{
    appearance::Palette,
    kit::{button, col, row, small, toolbar_button},
};
use gpui::{prelude::FluentBuilder, *};
use ox_desk_core::{host_protocol::Scope, store::Connection};
use uuid::Uuid;

#[derive(Clone)]
struct Target {
    host_id: String,
    name: String,
    scope: Scope,
    registration: Option<EntityId>,
}
struct Receipt {
    operation: Uuid,
    host_id: String,
    scope: Scope,
    session: String,
}
enum Phase {
    Waiting,
    Staging,
    Releasing,
    Ready(Receipt),
    Failed { source_fenced: bool },
    Opening,
}
struct Attempt {
    id: Uuid,
    target: Target,
    phase: Phase,
}
impl Attempt {
    fn confirm(&mut self, receipt: Receipt) -> Result<(), &'static str> {
        if matches!(self.phase, Phase::Releasing)
            && self.id == receipt.operation
            && self.target.host_id == receipt.host_id
            && self.target.scope == receipt.scope
            && !receipt.session.trim().is_empty()
        {
            self.phase = Phase::Ready(receipt);
            Ok(())
        } else {
            Err("The receipt does not confirm ownership on the chosen host")
        }
    }
    fn fenced(&self) -> bool {
        matches!(
            self.phase,
            Phase::Releasing
                | Phase::Ready(_)
                | Phase::Opening
                | Phase::Failed {
                    source_fenced: true
                }
        )
    }
}

#[derive(Default)]
pub(super) struct Preview {
    owner: Option<String>,
    chosen: Option<Target>,
    attempt: Option<Attempt>,
}
impl Preview {
    pub(super) fn enabled() -> bool {
        std::env::var_os("OX_TEST_MOVE_PREVIEW").is_some()
    }
}

impl Workbench {
    fn move_owner(&self) -> Option<&str> {
        let store = self.store.as_ref()?;
        let owner = store.selected()?;
        (store.state() == &Connection::Ready
            && !store.in_terminal(owner)
            && store.transcript(owner).is_some_and(|t| t.is_ready()))
        .then_some(owner)
    }

    fn move_source_name(&self) -> &str {
        self.host_caption
            .as_deref()
            .or_else(|| self.link.remote().map(|host| host.name.as_str()))
            .unwrap_or("Laptop")
    }

    fn move_targets(&self, owner: &str, cx: &App) -> Vec<Target> {
        let route = self
            .store
            .as_ref()
            .map(|store| store.thread_route(Some(owner)))
            .unwrap_or_else(|| self.route.clone());
        let actual = self
            .handoff_destinations(cx)
            .into_iter()
            .filter(|target| target.routes.contains(&route))
            .map(|target| Target {
                host_id: target.id.clone(),
                name: target.name.clone(),
                scope: Scope {
                    project: target.project.clone(),
                    route: route.clone(),
                },
                registration: Some(target.registration),
            })
            .collect::<Vec<_>>();
        if !actual.is_empty() || std::env::var_os("OX_TEST_MOVE_FIXTURE_HOST").is_none() {
            return actual;
        }
        vec![Target {
            host_id: "fixture-studio".into(),
            name: "Studio".into(),
            scope: Scope {
                project: self.project.with_file_name("studio-checkout"),
                route,
            },
            registration: None,
        }]
    }

    fn showing_move(&self) -> bool {
        self.move_preview.owner.as_deref().is_some_and(|owner| {
            self.store.as_ref().and_then(|store| store.selected()) == Some(owner)
        })
    }

    pub(super) fn move_source_reserved(&self) -> bool {
        self.showing_move()
            && self
                .move_preview
                .attempt
                .as_ref()
                .is_some_and(|attempt| attempt.fenced() || matches!(attempt.phase, Phase::Staging))
    }

    pub(super) fn move_preview_button(
        &self,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Option<gpui_component::button::Button> {
        let owner = self.move_owner()?;
        (Preview::enabled() && !self.showing_move() && self.move_preview.attempt.is_none()).then(
            || {
                let owner = owner.to_owned();
                toolbar_button(
                    p,
                    "move-to-host",
                    "Move to host…",
                    true,
                    cx.listener(move |s, _, _, cx| {
                        s.move_preview = Preview {
                            owner: Some(owner.clone()),
                            ..Preview::default()
                        };
                        cx.notify();
                    }),
                )
            },
        )
    }

    pub(super) fn move_preview_panel(&self, p: Palette, cx: &mut Context<Self>) -> Option<Div> {
        if !Preview::enabled() || !self.showing_move() {
            return None;
        }
        let source = self.move_source_name();
        let owner = self.move_preview.owner.as_deref()?;
        let running = self
            .store
            .as_ref()
            .and_then(|store| store.transcript(owner))
            .is_some_and(|t| t.active_turn().is_some());
        let mut panel = col()
            .w_full()
            .p_3()
            .gap_2()
            .border_t_2()
            .border_color(rgb(p.line_strong))
            .bg(rgb(p.panel));
        if let Some(attempt) = &self.move_preview.attempt {
            let target = &attempt.target.name;
            let (heading, explanation) = match &attempt.phase {
                Phase::Waiting => (
                    "Waiting for this turn to finish".into(),
                    format!(
                        "The current turn and its approvals stay on {source}. Keep {source} on until {target} is ready."
                    ),
                ),
                Phase::Staging => (
                    format!("Preparing {target}"),
                    format!("Copying the conversation and working files. Keep {source} on."),
                ),
                Phase::Releasing => (
                    format!("Closing on {source}"),
                    format!(
                        "The source is read-only while {target} takes over. Keep both hosts on."
                    ),
                ),
                Phase::Ready(_) => (
                    format!("Safe to shut down {source}"),
                    format!(
                        "{target} has confirmed it owns the session. Continue from your last turn."
                    ),
                ),
                Phase::Failed {
                    source_fenced: false,
                } => (
                    "Couldn’t prepare the destination".into(),
                    format!(
                        "The checkout transfer failed. Your session is still on {source}. Try again or choose another host."
                    ),
                ),
                Phase::Failed {
                    source_fenced: true,
                } => (
                    "Waiting for confirmation".into(),
                    format!(
                        "OX couldn’t confirm that {target} took over. Keep {source} on and check the move’s status."
                    ),
                ),
                Phase::Opening => (
                    format!("Opening on {target}"),
                    format!("The destination was confirmed. You can shut down {source}."),
                ),
            };
            panel = panel
                .child(status_title(p, heading))
                .child(small(p, explanation));
            match &attempt.phase {
                Phase::Waiting | Phase::Staging => {
                    panel = panel.child(
                        toolbar_button(
                            p,
                            "cancel-move",
                            "Cancel move",
                            true,
                            cx.listener(|s, _, _, cx| {
                                if let Some(attempt) = &s.move_preview.attempt
                                    && matches!(attempt.phase, Phase::Waiting | Phase::Staging)
                                {
                                    s.move_preview = Preview::default();
                                    cx.notify();
                                }
                            }),
                        )
                        .self_start(),
                    )
                }
                Phase::Ready(_) => {
                    panel = panel.child(
                        button(
                            p,
                            "open-moved-session",
                            format!("Open on {target}"),
                            true,
                            true,
                            cx.listener(|s, _, _, cx| s.open_preview_destination(cx)),
                        )
                        .self_start(),
                    )
                }
                Phase::Failed {
                    source_fenced: false,
                } => {
                    panel = panel.child(
                        toolbar_button(
                            p,
                            "retry-move",
                            "Choose destination again…",
                            true,
                            cx.listener(|s, _, _, cx| {
                                s.move_preview.attempt = None;
                                cx.notify();
                            }),
                        )
                        .self_start(),
                    )
                }
                Phase::Failed {
                    source_fenced: true,
                } => {
                    panel = panel.child(
                        toolbar_button(
                            p,
                            "check-move-status",
                            "Check status",
                            true,
                            cx.listener(|s, _, _, cx| {
                                if let Some(attempt) = &mut s.move_preview.attempt {
                                    attempt.phase = Phase::Releasing;
                                }
                                cx.notify();
                            }),
                        )
                        .self_start(),
                    )
                }
                _ => {}
            }
            return Some(panel);
        }

        panel = panel.child(status_title(p, "Move session to another host".into()))
            .child(small(p, if running {
                format!("Finish this turn on {source}, then continue on the destination. Keep {source} on until the move is ready.")
            } else {
                format!("Continue this conversation and checkout on another host. Keep {source} on until the move is ready.")
            }))
            .child(small(p, "Destination"));
        let targets = self.move_targets(owner, cx);
        let mut choices = row().flex_wrap().gap_2();
        for (i, target) in targets.iter().cloned().enumerate() {
            let selected = self
                .move_preview
                .chosen
                .as_ref()
                .is_some_and(|current| same_target(current, &target));
            let label = target.name.clone();
            choices = choices.child(
                button(
                    p,
                    ("move-target", i),
                    label,
                    selected,
                    true,
                    cx.listener(move |s, _, _, cx| {
                        s.move_preview.chosen = Some(target.clone());
                        cx.notify();
                    }),
                )
                .toggled(selected)
                .tooltip(targets[i].scope.project.display().to_string()),
            );
        }
        panel = if targets.is_empty() {
            panel.child(small(
                p,
                "No connected host has a matching checkout and billing route.",
            ))
        } else {
            panel.child(choices)
        };
        let selected = self
            .move_preview
            .chosen
            .as_ref()
            .filter(|chosen| targets.iter().any(|current| same_target(current, chosen)));
        panel = panel.when_some(selected, |panel, target| {
            panel
                .child(small(
                    p,
                    format!("Checkout  {}", target.scope.project.display()),
                ))
                .child(small(p, format!("Billing  {}", target.scope.route.label())))
        });
        let submit = if running {
            "Move after this turn".to_owned()
        } else {
            selected.map_or_else(
                || "Move session".into(),
                |target| format!("Move to {}", target.name),
            )
        };
        Some(
            panel
                .child(
                    row()
                        .gap_2()
                        .justify_end()
                        .child(toolbar_button(
                            p,
                            "close-move",
                            "Cancel",
                            true,
                            cx.listener(|s, _, _, cx| {
                                s.move_preview = Preview::default();
                                cx.notify();
                            }),
                        ))
                        .child(button(
                            p,
                            "queue-move",
                            submit,
                            true,
                            selected.is_some(),
                            cx.listener(move |s, _, _, cx| {
                                if s.move_preview.attempt.is_some() {
                                    return;
                                }
                                let Some(target) = s.move_preview.chosen.clone() else {
                                    return;
                                };
                                let Some(owner) = s.move_preview.owner.as_deref() else {
                                    return;
                                };
                                if s.move_owner() != Some(owner)
                                    || !s
                                        .move_targets(owner, cx)
                                        .iter()
                                        .any(|current| same_target(current, &target))
                                {
                                    s.move_preview.chosen = None;
                                    cx.notify();
                                    return;
                                }
                                let running = s
                                    .store
                                    .as_ref()
                                    .and_then(|store| store.transcript(owner))
                                    .is_some_and(|t| t.active_turn().is_some());
                                s.move_preview.attempt = Some(Attempt {
                                    id: Uuid::new_v4(),
                                    target,
                                    phase: if running {
                                        Phase::Waiting
                                    } else {
                                        Phase::Staging
                                    },
                                });
                                cx.notify();
                            }),
                        )),
                )
                .on_key_down(cx.listener(|s, e: &KeyDownEvent, _, cx| {
                    if e.keystroke.key == "escape" {
                        s.move_preview = Preview::default();
                        cx.notify();
                        cx.stop_propagation();
                    }
                })),
        )
    }

    fn open_preview_destination(&mut self, cx: &mut Context<Self>) {
        let Some(attempt) = self.move_preview.attempt.as_mut() else {
            return;
        };
        let Phase::Ready(receipt) = &attempt.phase else {
            return;
        };
        if receipt.session.is_empty() {
            return;
        }
        // The fixture records the requested navigation. No test receipt calls a host.
        attempt.phase = Phase::Opening;
        cx.notify();
    }

    /// Drives test progress as if it had arrived from the source host.
    pub(super) fn move_preview_progress(
        &mut self,
        incoming: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), &'static str> {
        if !Preview::enabled() {
            return Err("Move preview is disabled");
        }
        let attempt = self
            .move_preview
            .attempt
            .as_mut()
            .ok_or("No move is queued")?;
        match incoming {
            "staging" if matches!(attempt.phase, Phase::Waiting) => attempt.phase = Phase::Staging,
            "fenced" if matches!(attempt.phase, Phase::Staging) => attempt.phase = Phase::Releasing,
            "fail"
                if matches!(
                    attempt.phase,
                    Phase::Waiting | Phase::Staging | Phase::Releasing
                ) =>
            {
                attempt.phase = Phase::Failed {
                    source_fenced: attempt.fenced(),
                };
            }
            "ready" | "wrong-host" => attempt.confirm(Receipt {
                operation: attempt.id,
                host_id: if incoming == "wrong-host" {
                    "other-host".into()
                } else {
                    attempt.target.host_id.clone()
                },
                scope: attempt.target.scope.clone(),
                session: "fixture-moved-thread".into(),
            })?,
            _ => return Err("The current move does not accept that transition"),
        }
        cx.notify();
        Ok(())
    }
}

fn same_target(a: &Target, b: &Target) -> bool {
    a.host_id == b.host_id && a.scope == b.scope && a.registration == b.registration
}

fn status_title(p: Palette, text: String) -> Div {
    crate::platform::layout_probe::mark_element(
        div()
            .text_size(p.text(crate::appearance::Step::Ui))
            .font_weight(FontWeight::SEMIBOLD)
            .child(text.clone()),
        &"move-status".into(),
        &text.into(),
        false,
    )
}
