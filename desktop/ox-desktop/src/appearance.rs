use crate::preferences::behavior::{Active, Behavior, ThemeSource};
use gpui::{App, Global, px, rgb};
use gpui_component::{Theme, ThemeMode};
use std::env;
#[cfg(any(target_os = "linux", test))]
use toml_edit::DocumentMut;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Palette {
    pub bg: u32,
    pub nav: u32,
    pub panel: u32,
    pub line: u32,
    pub fg: u32,
    pub muted: u32,
    pub accent: u32,
    pub hover: u32,
    pub on_accent: u32,
    pub warning: u32,
    pub error: u32,
    pub light: bool,
    pub motion: bool,
}

impl Global for Palette {}

impl Default for Palette {
    fn default() -> Self {
        Self {
            bg: 0x1b1e22,
            nav: 0x15181b,
            panel: 0x25292e,
            line: 0x363c42,
            fg: 0xeaece7,
            muted: 0xa4aaa8,
            accent: 0xc5ec88,
            hover: 0x363c42,
            on_accent: 0x15181b,
            warning: 0xe0af68,
            error: 0xf3a598,
            light: false,
            motion: true,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn lightness(color: u32) -> f32 {
    let linear = |shift| {
        let v = ((color >> shift) & 255u32) as f32 / 255.;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
}

pub fn mix(a: u32, b: u32, percent: u32) -> u32 {
    let ch = |s: u32| -> u32 {
        (((a >> s) & 255u32) * (100 - percent) + ((b >> s) & 255u32) * percent + 50) / 100
    };
    ch(16) << 16 | ch(8) << 8 | ch(0)
}

#[cfg(any(target_os = "linux", test))]
fn parse_color(value: &str) -> Option<u32> {
    let hex = value.strip_prefix('#')?;
    if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        6 => u32::from_str_radix(hex, 16).ok(),
        3 => {
            let n = u32::from_str_radix(hex, 16).ok()?;
            Some((n >> 8) * 0x110000 + ((n >> 4) & 15) * 0x1100 + (n & 15) * 0x11)
        }
        _ => None,
    }
}

impl Palette {
    pub fn get(cx: &App) -> Self {
        *cx.global::<Self>()
    }

    #[cfg(any(target_os = "linux", test))]
    fn parse(text: &str) -> Option<Self> {
        let doc = text.parse::<DocumentMut>().ok()?;
        let color = |key| parse_color(doc.get(key)?.as_str()?);
        let bg = color("background")?;
        let fg = color("foreground")?;
        let accent = color("accent").or_else(|| color("color4")).unwrap_or(fg);
        let light = match doc.get("mode").and_then(|m| m.as_str()) {
            Some("light") => true,
            Some("dark") => false,
            _ => lightness(bg) > 0.45,
        };
        Some(Self {
            bg,
            fg,
            accent,
            light,
            nav: color("dark_background").unwrap_or(mix(bg, 0, 7)),
            panel: color("lighter_background").unwrap_or(mix(bg, fg, 8)),
            line: color("muted")
                .or_else(|| color("color8"))
                .unwrap_or(mix(bg, fg, 22)),
            muted: mix(bg, fg, 78),
            hover: color("selection").unwrap_or(mix(bg, accent, 20)),
            on_accent: if lightness(accent) > 0.179 {
                0x101115
            } else {
                0xffffff
            },
            warning: color("yellow")
                .or_else(|| color("color3"))
                .unwrap_or(accent),
            error: color("red").or_else(|| color("color1")).unwrap_or(fg),
            motion: true,
        })
    }

    fn read(system_light: bool, prefs: Behavior) -> Self {
        let light = match prefs.theme {
            ThemeSource::Light => true,
            ThemeSource::Dark => false,
            ThemeSource::System => system_light,
        };
        let mut palette = if light {
            Self {
                bg: 0xf5f6f2,
                nav: 0xe8eae3,
                panel: 0xffffff,
                line: 0xc4c7bc,
                fg: 0x22261e,
                muted: 0x535b4c,
                accent: 0x526d37,
                on_accent: 0xffffff,
                hover: 0xdce5d3,
                warning: 0x915600,
                error: 0xa82f35,
                light: true,
                motion: true,
            }
        } else {
            Self::default()
        };
        #[cfg(target_os = "linux")]
        if let Some(files) = crate::platform::theme::ThemeFiles::discover() {
            for path in files
                .colors
                .into_iter()
                .filter(|_| prefs.theme == ThemeSource::System)
            {
                if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= 65536)
                    && let Some(parsed) = std::fs::read_to_string(path)
                        .ok()
                        .and_then(|s| Self::parse(&s))
                {
                    palette = parsed;
                    break;
                }
            }
            palette.motion = !files.reduced_motion.is_file();
        }
        if let Ok(setting) = env::var("OXA_REDUCED_MOTION") {
            match setting.to_ascii_lowercase().as_str() {
                "on" | "yes" | "true" | "1" => palette.motion = false,
                "off" | "no" | "false" | "0" => palette.motion = true,
                _ => {}
            }
        }
        palette.motion &= !prefs.reduce_motion;
        palette
    }

    fn apply(self, cx: &mut App) {
        Theme::change(
            if self.light {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            },
            None,
            cx,
        );
        let font_size = cx.global::<Active>().font_size;
        Theme::update(cx, |theme| {
            theme.font_size = px(font_size as f32);
            theme.mono_font_size = px(font_size.saturating_sub(3) as f32);
            let c = &mut theme.colors;
            c.background = rgb(self.bg).into();
            c.foreground = rgb(self.fg).into();
            c.border = rgb(self.line).into();
            c.input = c.border;
            c.muted = rgb(self.panel).into();
            c.muted_foreground = rgb(self.muted).into();
            c.primary = rgb(self.accent).into();
            c.primary_foreground = rgb(self.on_accent).into();
            c.link = c.primary;
            c.caret = c.primary;
            c.ring = c.primary;
            c.accent = rgb(self.hover).into();
            c.accent_foreground = c.foreground;
            c.selection = rgb(mix(self.bg, self.accent, 45)).into();
            c.popover = c.muted;
            c.popover_foreground = c.foreground;
            c.secondary = c.muted;
            c.secondary_foreground = c.foreground;
            c.scrollbar_thumb = c.border;
            c.button_primary = c.primary;
            c.button_primary_foreground = c.primary_foreground;
            c.button_primary_hover = rgb(mix(self.accent, self.on_accent, 12)).into();
            c.button_primary_active = rgb(mix(self.accent, self.on_accent, 20)).into();
            c.button_secondary = c.secondary;
            c.button_secondary_foreground = c.secondary_foreground;
            c.button_secondary_hover = c.accent;
            c.button_secondary_active = c.selection;
        });
        cx.set_global(self);
    }
}

fn system_light(cx: &App) -> bool {
    matches!(
        cx.window_appearance(),
        gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight
    )
}

pub fn observe_window(window: &mut gpui::Window) {
    window
        .observe_window_appearance(|_, cx| {
            let palette = Palette::read(system_light(cx), cx.global::<Active>().0);
            if Palette::get(cx) != palette {
                palette.apply(cx);
            }
        })
        .detach();
}

pub fn refresh(cx: &mut App) {
    Palette::read(system_light(cx), cx.global::<Active>().0).apply(cx);
    cx.refresh_windows();
}
pub fn init(cx: &mut App) {
    cx.set_global(Active(
        ox_desk_core::desktop_behavior::read()
            .map(|s| s.behavior)
            .unwrap_or_default(),
    ));
    watch_behavior(cx);
    refresh(cx);
    #[cfg(target_os = "linux")]
    if let Some(paths) = crate::platform::theme::ThemeFiles::discover() {
        use gpui::AppContext;

        let (wake, updates) = flume::bounded(1);
        cx.spawn(async move |cx| {
            loop {
                let paths = paths.clone();
                let signal = wake.clone();
                let watcher = cx
                    .background_spawn(async move {
                        paths.watch(move || {
                            let _ = signal.try_send(());
                        })
                    })
                    .await;
                let (light, prefs) = cx.update(|cx| (system_light(cx), cx.global::<Active>().0));
                let palette = cx
                    .background_spawn(async move { Palette::read(light, prefs) })
                    .await;
                cx.update(|cx| {
                    if cx.global::<Active>().0 == prefs && Palette::get(cx) != palette {
                        palette.apply(cx);
                    }
                });
                let Ok(_watcher) = watcher else {
                    break;
                };
                if updates.recv_async().await.is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}

// Include saves from the companion service. Watch the parent because both native and
// phone clients replace preferences atomically.
fn watch_behavior(cx: &mut App) {
    use gpui::AppContext as _;
    use notify::Watcher as _;
    let Ok(path) = ox_desk_core::desktop_behavior::path() else {
        return;
    };
    let Some(dir) = path.parent().map(std::path::Path::to_path_buf) else {
        return;
    };
    let (tx, rx) = flume::bounded(1);
    cx.spawn(async move |cx| {
        let watcher = cx
            .background_spawn(async move {
                std::fs::create_dir_all(&dir).ok()?;
                let mut watcher =
                    notify::recommended_watcher(move |e: notify::Result<notify::Event>| {
                        if e.is_ok_and(|ev| {
                            matches!(
                                ev.kind,
                                notify::EventKind::Create(_)
                                    | notify::EventKind::Modify(_)
                                    | notify::EventKind::Remove(_)
                            ) && ev.paths.contains(&path)
                        }) {
                            let _ = tx.try_send(());
                        }
                    })
                    .ok()?;
                watcher
                    .watch(&dir, notify::RecursiveMode::NonRecursive)
                    .ok()?;
                Some(watcher)
            })
            .await;
        let Some(_watcher) = watcher else {
            return;
        };
        let mut initial = true;
        while initial || rx.recv_async().await.is_ok() {
            initial = false;
            let next = cx
                .background_spawn(async { ox_desk_core::desktop_behavior::read() })
                .await;
            if let Ok(next) = next {
                cx.update(|cx| {
                    if cx.global::<Active>().0 != next.behavior {
                        cx.set_global(Active(next.behavior));
                        refresh(cx);
                    }
                });
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_both_generations_and_derives_readable_colors() {
        let modern = Palette::parse(
            "background='#191925'\nforeground='#bbc'\naccent='#79f'\ndark_background='#14141c'",
        )
        .unwrap();
        assert_eq!(
            (modern.nav, modern.accent, modern.fg),
            (0x14141c, 0x7799ff, 0xbbbbcc)
        );
        let legacy =
            Palette::parse("background='#fff'\nforeground='#111'\ncolor4='#202060'").unwrap();
        assert!(legacy.light);
        assert_eq!((legacy.accent, legacy.on_accent), (0x202060, 0xffffff));
        assert!(Palette::parse("background='incorrect'\nforeground='#000'").is_none());
    }
}
