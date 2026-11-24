use super::workbench::Workbench;
use crate::kit::{DropdownMenu, PopupMenuItem, toolbar_button};
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
        if let Some(id) = owner
            && store.job(id).is_some()
        {
            let text = store.draft(owner).to_owned();
            self.action(
                cx,
                DeskAction::WorkerControl(
                    id.to_owned(),
                    ox_desk_core::worker::WorkerAction::Steer(text),
                ),
            );
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
        let job = key.and_then(|id| store.job(id)).map(|j| j.view());
        let busy = store.sending(key);
        let running = key
            .and_then(|id| store.transcript(id))
            .is_some_and(|t| t.active_turn().is_some());
        let enabled = store.can_send(key)
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
            let placeholder = if job.is_some() {
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
        if let Some(job) = job {
            let id = selected.as_deref().unwrap();
            return super::workers::controls(job, id, editor.clone(), draft, p, cx);
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
        let label = match (key, mode) {
            (None, Some(m)) => format!("Next thread · {}", m.label()),
            (_, Some(m)) => m.label().to_owned(),
            _ => "Permissions".into(),
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
        let access = toolbar_button(
            p,
            "access-preset",
            label,
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
            menu
        });
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
                            .when(running, |r| {
                                let id = selected.clone().unwrap();
                                r.child(toolbar_button(
                                    p,
                                    "stop-turn",
                                    "Stop",
                                    true,
                                    cx.listener(move |s, _, _, cx| {
                                        s.action(cx, DeskAction::Stop(id.clone()))
                                    }),
                                ))
                            })
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
