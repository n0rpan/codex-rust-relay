//! Desktop-owned display and input preferences.
use super::Preferences;
use crate::appearance::Palette;
use crate::appearance::Step;
use gpui::{prelude::*, *};
pub use ox_desk_core::desktop_behavior::{Behavior, ThemeSource};
pub struct Active(pub Behavior);
impl Global for Active {}
impl std::ops::Deref for Active {
    type Target = Behavior;
    fn deref(&self) -> &Behavior {
        &self.0
    }
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
            controls = controls.child(crate::kit::button(p, caption, caption, next == current, true, cx.listener(move |s, _, _, cx| {
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
                        "Ctrl / Cmd + Enter",
                        Behavior {
                            enter_sends: false,
                            ..b
                        },
                    ),
                    (
                        "Enter",
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
                        "System (Omarchy)",
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
