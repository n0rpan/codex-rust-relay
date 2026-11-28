//! Paint the native cartridge bitmaps using two CSS pixels per source pixel.
use ox_desk_core::cartridge::Cartridge;
use std::{fmt::Write, sync::OnceLock};

pub(super) fn stylesheet() -> &'static [u8] {
    static CSS: OnceLock<String> = OnceLock::new();
    CSS.get_or_init(|| {
        let mut css = String::from(include_str!("../assets/style.css"));
        for (name, kind) in [
            ("codex", Cartridge::Codex),
            ("claude", Cartridge::Claude),
            ("antigravity", Cartridge::Antigravity),
            ("api", Cartridge::Api),
            ("worker", Cartridge::Worker),
        ] {
            write!(
                css,
                "\n.session[data-source=\"{name}\"]:before{{box-shadow:"
            )
            .unwrap();
            let mut pixels = Vec::new();
            for (y, row) in kind.pixels().iter().enumerate() {
                for x in 0..7 {
                    if row & (1 << (6 - x)) != 0 {
                        pixels.push(format!("{}px {}px currentColor", x * 2, y * 2));
                    }
                }
            }
            css.push_str(&pixels.join(","));
            css.push_str(";}");
        }
        css
    })
    .as_bytes()
}
