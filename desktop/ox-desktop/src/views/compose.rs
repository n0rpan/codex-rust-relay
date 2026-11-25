use super::workbench::Workbench;
use crate::appearance::Step;
use crate::kit::{DropdownMenu, PopupMenuItem};
use crate::{appearance::Palette, col, kit::button, row};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::input::{InputEvent, TextareaState};
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::runtime::Access;

impl Workbench {
    fn send_input(&mut self, owner: Option<&str>, cx: &mut Context<Self>) {
        let Some(store) = &self.store else {
            return;
        };
        if self.image_work.contains_key(&owner.map(str::to_owned)) || store.selected() != owner {
            return;
        }
        if !store.draft_images(owner).is_empty()
            && !store.draft(owner).trim_start().starts_with('/')
            && (!store.has_command_schema()
                || store.commands_loading()
                || store.image_model_warning(owner).is_some())
        {
            return;
        }
        let id = owner.map(str::to_owned);
        if let Some(source) = owner
            && let Some(start) =
                ox_desk_core::handoff::HandoffStart::parse_command(source, store.draft(owner))
        {
            match start {
                Ok(mut start) => {
                    // Typing `--mode yolo` is the explicit YOLO confirmation on the desktop.
                    start.yolo_confirmed = start.mode
                        == ox_desk_core::modes::ModeRequest::Level(
                            ox_desk_core::modes::Level::Yolo,
                        );
                    self.action(cx, DeskAction::StartHandoff(start));
                    self.action(cx, DeskAction::DraftEdit(id, String::new()));
                }
                Err(message) => {
                    self.launch_error = Some(message);
                    cx.notify();
                }
            }
            return;
        }
        if let Some(parent) = owner
            && let Some(start) =
                ox_desk_core::origin::WorkerStart::parse_command(parent, store.draft(owner))
        {
            match start {
                Ok(mut start) => {
                    start.yolo_confirmed = start.permission
                        == Some(ox_desk_core::modes::ModeRequest::Level(
                            ox_desk_core::modes::Level::Yolo,
                        ));
                    self.action(cx, DeskAction::StartWorker(start));
                    self.action(cx, DeskAction::DraftEdit(id, String::new()));
                }
                Err(message) => {
                    self.launch_error = Some(message);
                    cx.notify();
                }
            }
            return;
        }
        let action = if store.draft(owner).trim_start().starts_with('/') {
            if !store.has_command_schema() || store.commands_loading() {
                self.pending_command = Some((id, store.draft(owner).to_owned()));
                cx.notify();
                return;
            }
            DeskAction::ExecuteCommand(id)
        } else {
            DeskAction::Submit(id)
        };
        self.action(cx, action);
    }

    pub(super) fn finish_command_discovery(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.store else { return };
        if !store.has_command_schema() || store.commands_loading() {
            return;
        }
        if let Some((owner, text)) = self.pending_command.take()
            && self.front
            && store.selected() == owner.as_deref()
            && store.draft(owner.as_deref()) == text
        {
            self.action(cx, DeskAction::ExecuteCommand(owner));
        }
    }

    pub(super) fn composer(
        &mut self,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        if !cx.has_active_drag() {
            self.file_hover = None;
        }
        let p = Palette::get(cx);
        let Some(store) = &self.store else {
            return col();
        };
        let model_controls = self.model_controls(width, cx);
        let selected = store.selected().map(str::to_owned);
        let key = selected.as_deref();
        let draft = store.draft(key);
        let busy = store.sending(key);
        let running = key
            .and_then(|id| store.transcript(id))
            .is_some_and(|t| t.active_turn().is_some());
        let enabled = store.can_compose(key)
            && !self.image_work.contains_key(&selected)
            && (store.draft_images(key).is_empty()
                || draft.trim_start().starts_with('/')
                || (store.has_command_schema()
                    && !store.commands_loading()
                    && store.image_model_warning(key).is_none()))
            && (!draft.trim().is_empty() || !store.draft_images(key).is_empty());
        let enter_sends = cx
            .global::<crate::preferences::behavior::Active>()
            .enter_sends;
        if self.enter_sends != enter_sends {
            for editor in self.editors.values() {
                editor.update(cx, |e, cx| e.set_submit_on_enter(enter_sends, cx));
            }
            self.enter_sends = enter_sends;
        }
        let editor = self.editors.entry(selected.clone()).or_insert_with(|| {
            let owner = selected.clone();
            let placeholder = if owner
                .as_deref()
                .is_some_and(|id| store.origin(id).is_some())
            {
                "Send an instruction to this worker…".to_owned()
            } else {
                format!("Ask {} about this project…", store.runtime_name())
            };
            let editor = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 6)
                    .submit_on_enter(enter_sends)
                    .placeholder(placeholder)
            });
            cx.subscribe_in(
                &editor,
                window,
                move |s, field, event, window, cx| match event {
                    InputEvent::Change => {
                        if let Some(store) = &mut s.store {
                            let text = field.read(cx).value();
                            if store.draft(owner.as_deref()) != text.as_ref() {
                                let _ = store.dispatch(DeskAction::DraftEdit(
                                    owner.clone(),
                                    text.to_string(),
                                ));
                                cx.notify();
                            }
                        }
                        s.search_mention(owner.clone(), field.clone(), cx);
                    }
                    InputEvent::PressEnter { secondary, shift }
                        if field.update(cx, |input, cx| {
                            input.marked_text_range(window, cx).is_none()
                        }) && (*secondary
                            || (!shift
                                && cx
                                    .global::<crate::preferences::behavior::Active>()
                                    .enter_sends)) =>
                    {
                        s.send_input(owner.as_deref(), cx)
                    }
                    _ => (),
                },
            )
            .detach();
            editor
        });
        if editor.read(cx).value().as_ref() != draft {
            editor.update(cx, |e, cx| {
                // The IME owns its temporary preedit until the platform commits it.
                if e.marked_text_range(window, cx).is_none() {
                    e.set_value(draft.to_owned(), window, cx);
                }
            });
        }
        let is_command = draft.trim_start().starts_with('/');
        let caps = store.command_caps(key);
        let discovering = is_command && (!store.has_command_schema() || store.commands_loading());
        let term = is_command
            && matches!(
                ox_desk_core::slash::parse(draft, &caps),
                Some(ox_desk_core::slash::Action::Terminal)
            );
        let enabled = enabled && !discovering;
        let send = if discovering {
            "Finding commands…"
        } else if term {
            "Open in terminal ↗"
        } else if is_command {
            "Run command"
        } else if busy {
            "Sending…"
        } else if running {
            "Steer"
        } else {
            "Send"
        };
        let effective = store.applied_access(key);
        let mode = if key.is_some() {
            effective
        } else {
            effective.or_else(|| store.next_access())
        };
        let granted = key
            .and_then(|id| store.session_mark(id, "yolo_grant"))
            .is_some_and(|v| v == true);
        let value = match (key, mode) {
            (Some(_), Some(m)) if granted && m.bypasses_permissions() => {
                "YOLO + workers".to_owned()
            }
            (None, Some(m)) => format!("Next thread · {}", m.label()),
            (_, Some(m)) => m.label().to_owned(),
            _ => String::new(),
        };
        if effective.is_some_and(Access::bypasses_permissions)
            && p.motion
            && self.front
            && window.is_visible()
            && window.is_window_active()
        {
            self.fire
                .get_or_insert_with(|| cx.new(crate::activity::Runner::burning));
        } else {
            self.fire = None;
        }
        let owner = selected.clone();
        let view = cx.weak_entity();
        let presets = Access::choices(self.route.runtime);
        let grant_offer = key.is_some() && effective.is_some_and(Access::bypasses_permissions);
        let access = crate::kit::chip(
            p,
            "access-preset",
            "Permissions",
            value,
            false,
            key.map_or_else(|| store.can_send(None), |id| store.can_change_access(id)),
            |_, _, _| {},
        )
        .icon(gpui_component::IconName::ChevronDown)
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
            for &preset in presets {
                let id = owner.clone();
                let view = view.clone();
                menu = menu.item(
                    PopupMenuItem::new(preset.label())
                        .checked(mode == Some(preset))
                        .on_click(move |_, _, cx| {
                            let _ = view.update(cx, |s, cx| {
                                if s.store.as_ref().and_then(|s| s.selected()) == id.as_deref() {
                                    let action = match &id {
                                        Some(id) => DeskAction::SetAccess(id.clone(), preset),
                                        None => DeskAction::SetNextAccess(preset),
                                    };
                                    s.action(cx, action);
                                }
                            });
                        }),
                );
            }
            if grant_offer {
                // The grant and its revoke share one group: the consequence is said in the menu.
                let id = owner.clone();
                let view = view.clone();
                menu = menu
                    .separator()
                    .item(PopupMenuItem::label("Workers and handoffs"));
                if granted {
                    menu = menu
                        .item(PopupMenuItem::label(
                            "YOLO is allowed for workers and handoffs started here.",
                        ))
                        .item(
                            PopupMenuItem::new("Revoke YOLO for workers and handoffs").on_click(
                                move |_, _, cx| {
                                    let _ = view.update(cx, |s, cx| {
                                        if let Some(id) = &id {
                                            s.action(cx, DeskAction::YoloGrant(id.clone(), false));
                                        }
                                    });
                                },
                            ),
                        );
                } else {
                    menu = menu
                        .item(PopupMenuItem::label(
                            "Lets workers and handoffs start in YOLO without asking.",
                        ))
                        .item(
                            PopupMenuItem::new("Allow YOLO for workers and handoffs").on_click(
                                move |_, _, cx| {
                                    let _ = view.update(cx, |s, cx| {
                                        if let Some(id) = &id {
                                            s.action(cx, DeskAction::YoloGrant(id.clone(), true));
                                        }
                                    });
                                },
                            ),
                        );
                }
            }
            menu
        });
        let handoff_notice = key
            .and_then(|id| ox_desk_core::handoff::HandoffStart::parse_command(id, draft))
            .and_then(Result::ok)
            .and_then(|start| ox_desk_core::handoff::provider_notice(&self.route, &start.route()));
        let suggestions = if is_command && !discovering {
            ox_desk_core::slash::completions(draft, &caps)
        } else {
            vec![]
        };
        let owner = selected.clone();
        let editor_for_menu = editor.clone();
        col()
            .flex_shrink_0()
            .gap_2()
            .p_3()
            .rounded_sm()
            .border_t_2()
            .border_color(rgb(crate::appearance::mix(p.line, p.accent, 46)))
            .bg(rgb(p.panel))
            .when(!suggestions.is_empty(), |panel| {
                let mut choices = row().gap_1().flex_wrap();
                for (i, (text, hint)) in suggestions.into_iter().enumerate() {
                    let input = editor_for_menu.clone();
                    choices = choices.child(
                        button(
                            p,
                            ("command-choice", i),
                            text.clone(),
                            false,
                            true,
                            cx.listener(move |_, _, window, cx| {
                                input.update(cx, |edit, cx| {
                                    edit.set_value(text.clone(), window, cx)
                                });
                                input.read(cx).focus_handle(cx).focus(window, cx);
                            }),
                        )
                        .tooltip(hint),
                    );
                }
                panel.child(choices)
            })
            .when_some(handoff_notice, |panel, notice| {
                panel.child(
                    div()
                        .id("handoff-provider-notice")
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(p.muted))
                        .child(notice),
                )
            })
            .child(super::mention_menu::composer_input(
                editor.clone(),
                selected.clone(),
                store,
                self.image_work.contains_key(&selected),
                self.mention_index,
                cx,
            ))
            .child(
                row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .when(width < px(760.), |controls| {
                        controls.flex_col().items_stretch()
                    })
                    .child(
                        row()
                            .min_w_0()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .children(model_controls)
                            .child(access)
                            .when(effective.is_some_and(Access::bypasses_permissions), |r| {
                                r.child(match &self.fire {
                                    Some(fire) => fire.clone().into_any_element(),
                                    None => crate::activity::flame(p, 0).into_any_element(),
                                })
                            }),
                    )
                    .child(
                        row()
                            .flex_shrink_0()
                            .justify_end()
                            .gap_2()
                            // Stop keeps a fixed slot and is disabled while idle, so the
                            // send button never moves when a turn starts or ends.
                            .child(div().w(px(72.)).flex().justify_end().child(button(
                                p,
                                "stop-turn",
                                "Stop",
                                false,
                                running && selected.is_some(),
                                cx.listener(move |s, _, _, cx| {
                                    if let Some(id) = selected.clone() {
                                        s.action(cx, DeskAction::Stop(id))
                                    }
                                }),
                            )))
                            .child(
                                button(
                                    p,
                                    "send-message",
                                    send,
                                    true,
                                    enabled,
                                    cx.listener(move |s, _, _, cx| {
                                        s.send_input(owner.as_deref(), cx)
                                    }),
                                )
                                .rounded(px(3.))
                                .min_w(px(68.))
                                .border_b_2()
                                .border_color(rgb(
                                    crate::appearance::mix(
                                        p.panel,
                                        p.accent,
                                        if enabled { 65 } else { 24 },
                                    ),
                                )),
                            ),
                    ),
            )
    }
}
