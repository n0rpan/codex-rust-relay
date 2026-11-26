//! Desktop-owned display and input preferences.
use super::Preferences;
use crate::appearance::Palette;
use crate::appearance::Step;
use gpui::{prelude::*, *};
pub use ox_desk_core::desktop_behavior::{Behavior, ThemeSource, WorkerApprovals};
use ox_desk_core::runtime::{Access, Runtime};
pub struct Active(pub Behavior);
impl Global for Active {}
impl std::ops::Deref for Active {
    type Target = Behavior;
    fn deref(&self) -> &Behavior {
        &self.0
    }
}
/// "Same as terminal" plus every native preset of `runtime`, as start-mode choices.
fn start_choices(runtime: Runtime, b: Behavior) -> Vec<(&'static str, Behavior)> {
    let with = |start: Option<Access>| match runtime {
        Runtime::Claude => Behavior {
            claude_start: start,
            ..b
        },
        Runtime::Codex => Behavior {
            codex_start: start,
            ..b
        },
    };
    std::iter::once(("Same as terminal", with(None)))
        .chain(Access::choices(runtime).iter().map(|&mode| {
            let label = match mode {
                Access::Default => "Ask before edits",
                other => other.label(),
            };
            (label, with(Some(mode)))
        }))
        .collect()
}
impl Preferences {
    fn preference_choices(
        &self,
        label: &'static str,
        options: &[(&'static str, Behavior)],
        current: Behavior,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut controls = div().flex().gap_1().flex_wrap();
        for &(caption, next) in options {
            controls = controls.child(crate::kit::button(p, SharedString::from(format!("{label}:{caption}")), caption, next == current, true, cx.listener(move |s, _, _, cx| {
                    if current == next || s.busy {
                        return;
                    }
                    s.busy = true;
                    cx.notify();
                    let write = cx.background_spawn(async move {
                        ox_desk_core::desktop_behavior::write(&ox_desk_core::desktop_behavior::revision(current),next).and_then(|result|match result{ox_desk_core::desktop_behavior::Outcome::Applied=>Ok(()),_=>Err(std::io::Error::other("The preference changed on another device. Review the current setting."))})
                    });
                    cx.spawn(async move |view, cx| {
                        let result = write.await;
                        let _ = view.update(cx, |s, cx| {
                            s.busy = false;
                            match result {
                                Ok(()) => {
                                    cx.set_global(Active(next));
                                    crate::appearance::refresh(cx);
                                    crate::sky::reload(cx);
                                    s.report(Ok(()), "Preference saved.", cx);
                                }
                                Err(e) => s.report(
                                    Err(format!("Could not save desktop preference: {e}")),
                                    "",
                                    cx,
                                ),
                            }
                        });
                    })
                    .detach();
                })));
        }
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .w(px(162.))
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(p.muted))
                    .child(label),
            )
            .child(controls)
    }
    /// How agents start and who answers their workers, on the Accounts page.
    pub(super) fn agents_card(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let b = cx.global::<Active>().0;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .border_1()
            .border_color(rgb(p.line))
            .rounded_lg()
            .bg(rgb(p.panel))
            .child(div().text_size(p.text(Step::Title)).child("Agents"))
            .child(self.preference_choices(
                "New Claude sessions",
                &start_choices(Runtime::Claude, b),
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "New Codex sessions",
                &start_choices(Runtime::Codex, b),
                b,
                p,
                cx,
            ))
            .child(
                div()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.muted))
                    .child("Same as terminal starts a new session the way `claude` or `codex` starts in your terminal, using your own Claude Code and Codex settings. A mode you pick in the composer still applies to that one session."),
            )
            .child(self.preference_choices(
                "Worker requests",
                &[
                    (
                        "Parent agent answers",
                        Behavior {
                            worker_approvals: WorkerApprovals::Parent,
                            ..b
                        },
                    ),
                    (
                        "I answer",
                        Behavior {
                            worker_approvals: WorkerApprovals::Operator,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(
                div()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.muted))
                    .child("A parent agent answers its workers' questions, and approves their actions when its own mode runs without asking you (Auto, Auto-review or YOLO). Read-only workers and mode changes always ask you. You can still answer any request yourself."),
            )
    }
    pub(super) fn behavior_page(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let b = cx.global::<Active>().0;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.preference_choices(
                "Send messages",
                &[
                    (
                        "Ctrl/Cmd+Enter sends · Enter for newline",
                        Behavior {
                            enter_sends: false,
                            ..b
                        },
                    ),
                    (
                        "Enter sends · Shift+Enter for newline (default)",
                        Behavior {
                            enter_sends: true,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "Theme",
                &[
                    (
                        "Follow system",
                        Behavior {
                            theme: ThemeSource::System,
                            ..b
                        },
                    ),
                    (
                        "OX Arcade light",
                        Behavior {
                            theme: ThemeSource::Light,
                            ..b
                        },
                    ),
                    (
                        "OX Arcade dark",
                        Behavior {
                            theme: ThemeSource::Dark,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "Text size",
                &[
                    ("Small", Behavior { font_size: 14, ..b }),
                    ("Default", Behavior { font_size: 16, ..b }),
                    ("Large", Behavior { font_size: 18, ..b }),
                    ("Extra large", Behavior { font_size: 20, ..b }),
                ],
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "Animation",
                &[
                    (
                        "Follow system",
                        Behavior {
                            reduce_motion: false,
                            ..b
                        },
                    ),
                    (
                        "Reduce motion",
                        Behavior {
                            reduce_motion: true,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "OX updates",
                &[
                    ("Download automatically", Behavior { background_updates: true, ..b }),
                    ("Check manually", Behavior { background_updates: false, ..b }),
                ], b, p, cx,
            ))
            .child(self.preference_choices(
                "Live weather (MET Norway)",
                &[
                    (
                        "Weather on",
                        Behavior {
                            live_weather: true,
                            ..b
                        },
                    ),
                    (
                        "Weather off",
                        Behavior {
                            live_weather: false,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(
                div()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.muted))
                    .child("Only a city-level location derived from your timezone is sent to api.met.no. Time of day works without it."),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::Behavior;

    #[test]
    fn enter_sends_by_default_and_saved_newline_preference_wins() {
        assert!(Behavior::default().enter_sends);
        assert!(serde_json::from_str::<Behavior>("{}").unwrap().enter_sends);
        assert!(
            !serde_json::from_str::<Behavior>(r#"{"enter_sends":false}"#)
                .unwrap()
                .enter_sends
        );
    }
}
