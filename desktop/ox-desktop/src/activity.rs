use crate::appearance::Palette;
use gpui::*;
use std::time::{Duration, Instant};

mod scene;
use scene::Ink;
pub use scene::Scene;

/// Seconds the app has been up: the slow clock that rotates the thinking sprites.
fn uptime() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs()
}

/// The seed that keeps one turn's thinking sprites stable across re-renders.
pub fn seed(thread: &str, turn: &str) -> u64 {
    scene::seed(thread, turn)
}

impl Scene {
    pub fn sprite(self, p: Palette, frame: u8) -> AnyElement {
        self.sprite_for(p, 0, frame)
    }

    /// The motionless key frame, for reduced motion and surfaces without a runner.
    pub fn still(self, p: Palette, seed: u64) -> AnyElement {
        self.sprite_for(p, seed, self.key_frame())
    }

    fn sprite_for(self, p: Palette, seed: u64, frame: u8) -> AnyElement {
        match self {
            Self::Fire => flame(p, frame).into_any_element(),
            Self::Loading => ox(p, frame, 2.).into_any_element(),
            _ => {
                let variant = scene::thinking_variant(seed, uptime());
                cabinet(p, self, variant, frame, false).into_any_element()
            }
        }
    }
}

/// A one-shot scene has played through; its owner clears the slot.
pub struct Finished;

/// Schedule one tick after a rendered frame. A covered surface cannot spin its own timer.
pub struct Runner {
    pub scene: Scene,
    pub seed: u64,
    frame: u8,
    left: Option<u8>,
    tick: Option<Task<()>>,
    bonus_frames: u8,
}

impl EventEmitter<Finished> for Runner {}

impl Runner {
    pub fn new(scene: Scene, seed: u64) -> Self {
        Self {
            scene,
            seed,
            frame: 0,
            left: scene.one_shot(),
            tick: None,
            bonus_frames: 0,
        }
    }

    pub fn burning(_: &mut Context<Self>) -> Self {
        Self::new(Scene::Fire, 0)
    }
}

impl Render for Runner {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::guard(cx, |cx| {
            let p = Palette::get(cx);
            if self.left == Some(0) {
                return div().into_any_element();
            }
            // With motion off only a one-shot still wakes, once, to leave the slot.
            let frames = match self.left {
                _ if p.motion => Some(1),
                Some(left) => Some(u64::from(left)),
                None => None,
            };
            if let Some(frames) = frames.filter(|_| self.tick.is_none() && window.is_visible()) {
                let wait = Duration::from_millis(self.scene.tick_ms() * frames);
                self.tick = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(wait).await;
                    let _ = this.update(cx, |s, cx| {
                        s.frame = (s.frame + 1) % 8;
                        s.bonus_frames = s.bonus_frames.saturating_sub(1);
                        s.tick = None;
                        if let Some(left) = &mut s.left {
                            *left = left.saturating_sub(frames as u8);
                            if *left == 0 {
                                cx.emit(Finished);
                            }
                        }
                        cx.notify();
                    });
                }));
            }
            let sprite = if !p.motion {
                self.scene.still(p, self.seed)
            } else if self.bonus_frames > 0 {
                cabinet(p, self.scene, 0, self.frame, true).into_any_element()
            } else {
                self.scene.sprite_for(p, self.seed, self.frame)
            };
            if self.scene == Scene::Fire {
                return sprite;
            }
            div()
                .id("arcade-sprite")
                .cursor_pointer()
                .child(sprite)
                .on_click(cx.listener(|s, _, _, cx| {
                    s.bonus_frames = 16;
                    cx.notify();
                }))
                .into_any_element()
        })
    }
}

// Low-resolution sprites share the terminal palette and one eight-frame clock.
fn cabinet(p: Palette, scene: Scene, variant: u8, frame: u8, bonus: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let mut rect = |x: i32, y: i32, w: i32, h: i32, color| {
                let at = bounds.origin + point(px(x as f32 * 2.), px(y as f32 * 2.));
                window.paint_quad(fill(
                    Bounds::new(at, size(px(w as f32 * 2.), px(h as f32 * 2.))),
                    rgb(color),
                ));
            };
            if bonus {
                // The easter egg: a saucer hovers in and drops a shot.
                let beat = i32::from(frame);
                let lift = i32::from((2..6).contains(&frame));
                rect(8, 1 + lift, 4, 1, p.fg);
                rect(6, 2 + lift, 8, 2, p.accent);
                rect(3, 4 + lift, 14, 2, p.fg);
                rect(5, 6 + lift, 10, 1, p.accent);
                for i in 0..3 {
                    rect(
                        6 + i * 3,
                        5 + lift,
                        1,
                        1,
                        if (beat / 2) % 3 == i {
                            p.warning
                        } else {
                            p.line
                        },
                    );
                }
                rect(7 + beat % 6, 8, 1, 2, p.warning);
                return;
            }
            scene::draw(scene, variant, frame, &mut |x, y, w, h, ink| {
                let color = match ink {
                    Ink::Fg => p.fg,
                    Ink::Accent => p.accent,
                    Ink::OnAccent => p.on_accent,
                    Ink::Warning => p.warning,
                    Ink::Success => p.success,
                    Ink::Error => p.error,
                    Ink::Line => p.line,
                    Ink::Panel => p.panel,
                };
                rect(x, y, w, h, color);
            });
        },
    )
    .w(px(scene::GRID.0 as f32 * 2.))
    .h(px(scene::GRID.1 as f32 * 2.))
    .flex_shrink_0()
}

pub fn ox(p: Palette, frame: u8, unit: f32) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let mut rect = |x: i32, y: i32, w: i32, h: i32, color| {
                let origin = bounds.origin + point(px(x as f32 * unit), px(y as f32 * unit));
                window.paint_quad(fill(
                    Bounds::new(origin, size(px(w as f32 * unit), px(h as f32 * unit))),
                    rgb(color),
                ));
            };
            let bounce = i32::from(matches!(frame, 2 | 3 | 6 | 7));
            let step = [0, 1, 1, 0, 0, -1, -1, 0][usize::from(frame)];
            // Tail, back, muzzle and the two upward horns.
            rect(3, 4 + bounce, 2, 1, p.accent);
            rect(2, 3 + bounce, 1, 2, p.accent);
            rect(5, 3 + bounce, 9, 4, p.accent);
            rect(13, 2 + bounce, 4, 4, p.accent);
            rect(16, 4 + bounce, 3, 2, p.accent);
            rect(12, bounce, 1, 2, p.fg);
            rect(13, 1 + bounce, 1, 1, p.fg);
            rect(16, 1 + bounce, 2, 1, p.fg);
            rect(17, bounce, 1, 1, p.fg);
            rect(15, 3 + bounce, 1, 1, p.on_accent);
            rect(6, 7 + bounce, 1, 1, p.accent);
            rect(6 + step, 8 + bounce, 2, 1, p.accent);
            rect(12, 7 + bounce, 1, 1, p.accent);
            rect(12 - step, 8 + bounce, 2, 1, p.accent);
            rect(2 + (7 - i32::from(frame)) % 4, 10, 2, 1, p.line);
            rect(13 + (7 - i32::from(frame)) % 5, 10, 3, 1, p.line);
        },
    )
    .w(px(unit * 20.))
    .h(px(unit * 11.))
    .flex_shrink_0()
}

/// A two-tone campfire with rising embers behind the running ox.
pub fn flame(p: Palette, frame: u8) -> impl IntoElement {
    let f = usize::from(frame);
    let hot = Palette {
        accent: p.warning,
        ..p
    };
    div()
        .relative()
        .w(px(40.))
        .h(px(31.))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let mut pixel = |x: usize, y: usize, w: usize, h: usize, color: u32| {
                        let origin = bounds.origin + point(px((x * 2) as f32), px((y * 2) as f32));
                        window.paint_quad(fill(
                            Bounds::new(origin, size(px((w * 2) as f32), px((h * 2) as f32))),
                            rgb(color),
                        ));
                    };
                    let tips = [2, 3, 4, 3, 4, 6, 5, 3, 2, 4, 5, 3];
                    for i in 0..12 {
                        let h = tips[(i + f) % tips.len()];
                        pixel(4 + i, 10 - h, 1, h, p.error);
                        pixel(4 + i, 10 - h / 2, 1, h / 2, p.warning);
                    }
                    pixel(7 + (f / 3), 3 - (f % 4), 1, 1, p.error);
                    pixel(14 - (f / 4), 3 - (f + 2) % 4, 1, 1, p.warning);
                },
            )
            .absolute()
            .size_full(),
        )
        .child(div().absolute().bottom_0().child(ox(hot, frame, 2.)))
}

/// A hand-lettered OX cabinet marquee. Three-pixel strokes stay crisp at desktop scale.
pub fn marquee(p: Palette, hot: bool, compact: bool) -> impl IntoElement {
    let scale = if compact { 0.75 } else { 1.0 };
    const LETTERS: [&[u8]; 7] = [
        b".####..##...##",
        b"##..##.##...##",
        b"##..##..##.##.",
        b"##..##...###..",
        b"##..##..##.##.",
        b"##..##.##...##",
        b".####..##...##",
    ];
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let mut block = |x: f32, y: f32, w: f32, h: f32, color: u32| {
                let origin =
                    bounds.origin + point(px((x * scale).round()), px((y * scale).round()));
                window.paint_quad(fill(
                    Bounds::new(
                        origin,
                        size(
                            px((w * scale).round().max(1.)),
                            px((h * scale).round().max(1.)),
                        ),
                    ),
                    rgb(color),
                ));
            };
            let ink = if hot { p.warning } else { p.accent };
            let edge = if hot {
                p.error
            } else {
                crate::appearance::mix(p.accent, p.fg, 50)
            };
            // The clipped corners and offset shadow follow the silhouette of an arcade marquee.
            for (x, y, w, h) in [(4., 1., 55., 37.), (1., 4., 61., 31.)] {
                block(x, y, w, h, p.line);
            }
            for (x, y, w, h) in [(5., 3., 53., 33.), (3., 6., 57., 27.)] {
                block(x, y, w, h, p.bg);
            }
            block(7., 5., 13., 1., edge);
            block(45., 33., 10., 1., ink);
            for (dx, dy, shadow) in [(2., 2., true), (0., 0., false)] {
                for (y, row) in LETTERS.iter().enumerate() {
                    for (x, &bit) in row.iter().enumerate() {
                        if bit == b'#' {
                            let color = if shadow {
                                crate::appearance::mix(p.bg, ink, 32)
                            } else if y < 2 {
                                edge
                            } else {
                                ink
                            };
                            block(
                                10. + x as f32 * 3. + dx,
                                8. + y as f32 * 3. + dy,
                                3.,
                                3.,
                                color,
                            );
                        }
                    }
                }
            }
        },
    )
    .w(px(64. * scale))
    .h(px(40. * scale))
    .flex_shrink_0()
}

pub use ox_desk_core::cartridge::Cartridge;

/// A provider glyph: full colour while `vivid` (the active or streaming thread), otherwise
/// a quiet monochrome tint so the list reads calm.
pub fn cartridge(p: Palette, kind: Cartridge, vivid: bool) -> impl IntoElement {
    let pattern = kind.pixels();
    let color = match kind {
        Cartridge::Codex => p.accent,
        Cartridge::Claude => p.warning,
        Cartridge::Antigravity => crate::appearance::mix(p.accent, p.fg, 32),
        Cartridge::Api => crate::appearance::mix(p.fg, p.accent, 48),
        Cartridge::Worker => p.muted,
    };
    let ink = if vivid {
        color
    } else {
        crate::appearance::mix(crate::appearance::mix(color, p.muted, 60), p.nav, 35)
    };
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for (y, line) in pattern.iter().enumerate() {
                for x in 0..7 {
                    if line & (1 << (6 - x)) != 0 {
                        window.paint_quad(fill(
                            Bounds::new(
                                bounds.origin + point(px((x * 2) as f32), px((y * 2) as f32)),
                                size(px(2.), px(2.)),
                            ),
                            rgb(ink),
                        ));
                    }
                }
            }
        },
    )
    .w(px(14.))
    .h(px(14.))
    .flex_shrink_0()
}

/// The cabinet header's living sky; see `sky.rs` for the drawing and the weather.
pub fn horizon(p: Palette, sky: &crate::sky::Sky) -> impl IntoElement {
    crate::sky::horizon(p, sky)
}
