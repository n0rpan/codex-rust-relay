//! Resolve the documented shortcuts against GPUI's primary modifier for this platform.
use gpui::{App, Keystroke, Modifiers};
use ox_desk_core::keys::{BINDINGS, Command};

pub fn bindings(cx: &App) -> Vec<(Keystroke, Command)> {
    let primary = if Modifiers::secondary_key(cx).platform {
        "cmd-"
    } else {
        "ctrl-"
    };
    BINDINGS
        .iter()
        .map(|b| {
            (
                Keystroke::parse_with_os(&b.chord.replace("mod-", primary), cx.platform_info().os)
                    .expect("valid documented binding"),
                b.command,
            )
        })
        .collect()
}
