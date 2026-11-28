//! Choose a connected host and present the source's durable move progress.
use super::{handoff_sheet::HandoffReady, workbench::Workbench, worker_sheet::OpenWorkerHosts};
use crate::{
    appearance::Palette,
    kit::{button, col, row, small, toolbar_button},
};
use gpui::{
    AppContext, ClickEvent, Context, Div, InteractiveElement, ParentElement, Styled, div, px, rgb,
};
use gpui_component::scroll::ScrollableElement;
use ox_desk_core::{
    actions::{Action, MoveSession},
    host_protocol::Scope,
    runtime::Route,
    store::Connection,
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

pub(super) struct Picker {
    owner: String,
    choices: Option<Result<Vec<Destination>, String>>,
    selected: Option<String>,
}

struct Destination {
    id: String,
    name: String,
    unavailable: Option<&'static str>,
}

#[derive(Deserialize)]
struct Ready {
    operation: String,
    host: String,
    thread: String,
    scope: Scope,
}

impl Workbench {
    fn live_move_mark(&self) -> Option<(&str, &Value)> {
        let store = self.store.as_ref()?;
        let thread = store.selected()?;
        Some((thread, store.session_mark(thread, "move")?))
    }

    pub(super) fn live_move_locked(&self) -> bool {
        self.live_move_mark().is_some_and(|(_, mark)| {
            matches!(
                mark["phase"].as_str(),
                Some(
                    "waiting_for_workers"
                        | "preparing"
                        | "closing"
                        | "checking"
                        | "ready"
                        | "failed"
                )
            )
        })
    }

    pub(super) fn live_move_button(
        &self,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Option<gpui_component::button::Button> {
        let store = self.store.as_ref()?;
        let thread = store.selected()?;
        if !store.can_move_session(thread)
            || self.live_move_locked()
            || self.live_move_mark().is_some_and(|(_, mark)| {
                matches!(
                    mark["phase"].as_str(),
                    Some("waiting" | "waiting_for_workers")
                )
            })
            || self
                .move_picker
                .as_ref()
                .is_some_and(|picker| picker.owner == thread)
        {
            return None;
        }
        let thread = thread.to_owned();
        Some(toolbar_button(
            p,
            "move-to-host",
            "Move to host…",
            store.state() == &Connection::Ready,
            cx.listener(move |s, _, _, cx| s.load_move_destinations(thread.clone(), cx)),
        ))
    }

    fn load_move_destinations(&mut self, owner: String, cx: &mut Context<Self>) {
        let link = self.link.clone();
        let project = self.project.clone();
        let route = self
            .store
            .as_ref()
            .map_or_else(|| self.route.clone(), |s| s.thread_route(Some(&owner)));
        self.move_picker = Some(Picker {
            owner: owner.clone(),
            choices: None,
            selected: None,
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    link.worker_hosts(project)
                        .map(|result| destinations(result.as_ref(), &route))
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |s, cx| {
                if let Some(picker) = &mut s.move_picker
                    && picker.owner == owner
                {
                    picker.choices = Some(result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn live_move_panel(&self, p: Palette, cx: &mut Context<Self>) -> Option<Div> {
        let store = self.store.as_ref()?;
        let owner = store.selected()?;
        let current = self.host_caption.as_deref().unwrap_or("this computer");
        let mut panel = col()
            .w_full()
            .p_3()
            .gap_2()
            .border_t_2()
            .border_color(rgb(p.line_strong))
            .bg(rgb(p.panel));
        if let Some(picker) = self
            .move_picker
            .as_ref()
            .filter(|picker| picker.owner == owner)
        {
            let running = store
                .transcript(owner)
                .is_some_and(|t| t.active_turn().is_some());
            panel = panel.child("Move session to another host").child(small(
                p,
                if running {
                    format!(
                        "This turn will finish on {current}. Keep it on until OX confirms the move."
                    )
                } else {
                    format!("Keep {current} on until OX confirms the move.")
                },
            ));
            let targets = match &picker.choices {
                None => {
                    panel = panel.child(small(p, "Checking connected hosts…"));
                    &[][..]
                }
                Some(Err(error)) => {
                    panel = panel.child(small(p, error.clone()));
                    &[][..]
                }
                Some(Ok(targets)) if targets.is_empty() => {
                    panel = panel.child(small(p, "Link a host before moving this session."));
                    &[][..]
                }
                Some(Ok(targets)) => targets.as_slice(),
            };
            let mut choices = row().gap_2().flex_wrap();
            for (index, target) in targets.iter().enumerate() {
                let id = target.id.clone();
                let selected = picker.selected.as_ref() == Some(&id);
                let label = target.unavailable.map_or_else(
                    || target.name.clone(),
                    |reason| format!("{} · {reason}", target.name),
                );
                choices = choices.child(button(
                    p,
                    ("live-move-target", index),
                    label,
                    selected,
                    target.unavailable.is_none(),
                    cx.listener(move |s, _, _, cx| {
                        if let Some(picker) = &mut s.move_picker {
                            picker.selected = Some(id.clone());
                        }
                        cx.notify();
                    }),
                ));
            }
            let selected = targets.iter().find(|target| {
                picker.selected.as_ref() == Some(&target.id) && target.unavailable.is_none()
            });
            let label = if running {
                "Move after this turn".into()
            } else {
                selected.map_or_else(
                    || "Move session".into(),
                    |target| format!("Move to {}", target.name),
                )
            };
            let destination = selected.map(|target| target.id.clone());
            let owner = owner.to_owned();
            panel = panel
                .child(
                    div()
                        .id("move-host-choices")
                        .max_h(px(132.))
                        .overflow_y_scrollbar()
                        .child(choices),
                )
                .child(
                    row()
                        .gap_2()
                        .justify_end()
                        .child(toolbar_button(
                            p,
                            "manage-move-hosts",
                            "Manage host links",
                            true,
                            cx.listener(|_, _, _, cx| cx.emit(OpenWorkerHosts)),
                        ))
                        .child(toolbar_button(
                            p,
                            "close-move",
                            "Cancel",
                            true,
                            cx.listener(|s, _, _, cx| {
                                s.move_picker = None;
                                cx.notify();
                            }),
                        ))
                        .child(button(
                            p,
                            "queue-move",
                            label,
                            true,
                            destination.is_some(),
                            cx.listener(move |s, _, _, cx| {
                                if let Some(destination) = &destination {
                                    s.move_picker = None;
                                    s.action(
                                        cx,
                                        Action::MoveSession(MoveSession {
                                            thread: owner.clone(),
                                            destination: destination.clone(),
                                            operation: Uuid::new_v4().to_string(),
                                            after_turn: running,
                                        }),
                                    );
                                }
                            }),
                        )),
                );
            return Some(panel);
        }

        let (owner, mark) = self.live_move_mark()?;
        let host = mark["destination"]["name"].as_str()?;
        let phase = mark["phase"].as_str()?;
        let operation = mark["id"].as_str()?.to_owned();
        let ready = ready_receipt(mark);
        let (label, detail) = match phase {
            "waiting" => (
                format!("Moves to {host} after this turn"),
                format!("Keep {current} on until the move is ready."),
            ),
            "waiting_for_workers" => (
                "Waiting for workers".into(),
                format!("The move to {host} starts after the workers finish."),
            ),
            "preparing" => (
                format!("Preparing {host}"),
                format!("Copying the conversation and working files. Keep {current} on."),
            ),
            "closing" => (
                format!("Closing on {current}"),
                format!("Keep {current} on until {host} confirms the move."),
            ),
            "ready" if ready.is_some() => (
                format!("Ready on {host}"),
                format!("You can shut down {current}."),
            ),
            "ready" | "checking" => (
                "Confirming the move".into(),
                format!("Keep {current} on while OX checks whether {host} can continue."),
            ),
            "failed" => (
                format!("Couldn’t prepare {host}"),
                mark["message"]
                    .as_str()
                    .unwrap_or("Try again or cancel the move.")
                    .into(),
            ),
            "cancelled" => return None,
            _ => return None,
        };
        panel = panel.child(label).child(small(p, detail));
        let mut actions = row().gap_2();
        if matches!(
            phase,
            "waiting" | "waiting_for_workers" | "preparing" | "failed"
        ) {
            let op = operation.clone();
            actions = actions.child(toolbar_button(
                p,
                "cancel-move",
                "Cancel move",
                true,
                cx.listener(move |s, _, _, cx| s.action(cx, Action::CancelMove(op.clone()))),
            ));
        }
        if let Some(ready) = ready {
            let owner = owner.to_owned();
            let address = mark["destination"]["origin"]
                .as_str()
                .and_then(|origin| ox_host_web::serve::session_url(origin, &ready.thread));
            actions = actions.child(button(
                p,
                "open-moved-session",
                format!("Open on {host}"),
                true,
                true,
                cx.listener(move |s, _: &ClickEvent, _, cx| {
                    if let Some(destination) = s
                        .handoff_destinations(cx)
                        .into_iter()
                        .find(|host| host.id == ready.host)
                    {
                        cx.emit(HandoffReady {
                            source_session: owner.clone(),
                            registration: destination.registration,
                            host: Some(ready.host.clone()),
                            scope: ready.scope.clone(),
                            session: Some(ready.thread.clone()),
                        });
                    } else if let Some(address) = &address {
                        cx.open_url(address);
                    }
                }),
            ));
        } else if matches!(phase, "checking" | "failed" | "ready") {
            let retry = phase == "failed";
            actions = actions.child(button(
                p,
                "retry-move",
                if retry { "Try again" } else { "Check status" },
                true,
                true,
                cx.listener(move |s, _, _, cx| {
                    s.action(
                        cx,
                        if retry {
                            Action::RetryMove(operation.clone())
                        } else {
                            Action::CheckMove(operation.clone())
                        },
                    )
                }),
            ));
        }
        Some(panel.child(actions))
    }
}

fn ready_receipt(mark: &Value) -> Option<Ready> {
    if mark["phase"] != "ready" {
        return None;
    }
    let receipt: Ready = serde_json::from_value(mark["receipt"].clone()).ok()?;
    (mark["id"].as_str() == Some(receipt.operation.as_str())
        && mark["destination"]["id"].as_str() == Some(receipt.host.as_str())
        && !receipt.thread.is_empty())
    .then_some(receipt)
}

fn destinations(inventory: Option<&Value>, route: &Route) -> Vec<Destination> {
    inventory
        .and_then(|report| report["hosts"].as_array())
        .into_iter()
        .flatten()
        .filter(|row| row["parent_host"] != true)
        .filter_map(|row| {
            let unavailable = match row["status"].as_str() {
                Some("online") if row["supports_session_moves"] != true => Some("OX update needed"),
                Some("online") if row["grant"]["may_accept_moves"] != true => {
                    Some("Accept session moves is off")
                }
                Some("online")
                    if !row["routes"].as_array().is_some_and(|routes| {
                        routes.iter().any(|r| {
                            serde_json::from_value::<Route>(r.clone()).is_ok_and(|r| &r == route)
                        })
                    }) =>
                {
                    Some("Runtime or account unavailable")
                }
                Some("online") => None,
                Some("access_denied") => Some("Host link denied"),
                Some("needs_update") => Some("OX update needed"),
                _ => Some("Offline or unreachable"),
            };
            Some(Destination {
                id: row["host"]["id"].as_str()?.into(),
                name: row["host"]["name"].as_str()?.into(),
                unavailable,
            })
        })
        .collect()
}
