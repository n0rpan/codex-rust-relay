use crate::appearance::Palette;
use gpui::*;
use std::time::Duration;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Scene {
    Loading,
    Thinking,
    Typing,
    Tool,
    Worker,
    Waiting,
    Fire,
}

impl Scene {
    pub fn label(self) -> &'static str {
        match self {
            Self::Loading => "Loading history…",
            Self::Thinking => "Thinking",
            Self::Typing => "Writing",
            Self::Tool => "Using a tool",
            Self::Worker => "Worker active",
            Self::Waiting => "Waiting for you",
            Self::Fire => "YOLO",
        }
    }

    pub fn sprite(self, p: Palette, frame: u8) -> AnyElement {
        match self {
            Self::Fire => flame(p, frame).into_any_element(),
            Self::Loading => ox(p, frame, 2.).into_any_element(),
            _ => cabinet(p, self, frame, false).into_any_element(),
        }
    }
}

/// Schedule one tick after a rendered frame. A covered surface cannot spin its own timer.
pub struct Runner {
    pub scene: Scene,
    frame: u8,
    tick: Option<Task<()>>,
    bonus_frames: u8,
}

impl Runner {
    pub fn new(scene: Scene) -> Self {
        Self {
            scene,
            frame: 0,
            tick: None,
            bonus_frames: 0,
        }
    }

    pub fn burning(_: &mut Context<Self>) -> Self {
        Self::new(Scene::Fire)
    }
}

impl Render for Runner {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::guard(cx, |cx| {
            let p = Palette::get(cx);
            if self.tick.is_none() && p.motion && window.is_visible() {
                self.tick = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(125))
                        .await;
                    let _ = this.update(cx, |s, cx| {
                        s.frame = (s.frame + 1) % 8;
                        s.bonus_frames = s.bonus_frames.saturating_sub(1);
                        s.tick = None;
                        cx.notify();
                    });
                }));
            }
            let sprite = if self.bonus_frames > 0 {
                cabinet(p, self.scene, self.frame, true).into_any_element()
            } else {
                self.scene.sprite(p, self.frame)
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
fn cabinet(p: Palette, scene: Scene, frame: u8, bonus: bool) -> impl IntoElement {
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
            let beat = i32::from(frame);
            if bonus {
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
            } else {
                match scene {
                    Scene::Typing | Scene::Tool => {
                        rect(2, 1, 15, 8, p.accent);
                        rect(3, 2, 13, 6, p.panel);
                        rect(4, 3, 1, 1, p.fg);
                        rect(5, 4, 1, 1, p.fg);
                        rect(4, 5, 1, 1, p.fg);
                        if scene == Scene::Typing {
                            rect(7, 5, 1 + beat, 1, p.accent);
                        } else {
                            rect(7 + beat / 2, 6, 3, 1, p.warning);
                        }
                        rect(6, 9, 7, 1, p.line);
                    }
                    Scene::Thinking => {
                        rect(3, 2, 13, 5, p.accent);
                        rect(2, 3, 1, 3, p.accent);
                        rect(16, 3, 1, 3, p.accent);
                        rect(5, 7, 3, 1, p.accent);
                        rect(4, 8, 1, 1, p.accent);
                        for i in 0..3 {
                            rect(
                                5 + i * 4,
                                4,
                                2,
                                1,
                                if beat / 2 % 3 == i { p.fg } else { p.on_accent },
                            );
                        }
                    }
                    Scene::Worker => {
                        for i in 0..2 {
                            let x = 1 + 10 * i;
                            let y = 3 + (beat / 2 + i) % 2;
                            rect(x, y, 7, 4, p.accent);
                            rect(x + 1, y - 2, 1, 2, p.fg);
                            rect(x + 5, y - 2, 1, 2, p.fg);
                            rect(x + 1, y + 1, 1, 1, p.on_accent);
                            rect(x + 5, y + 1, 1, 1, p.on_accent);
                            rect(x + 1, y + 4, 1, 1, p.accent);
                            rect(x + 5, y + 4, 1, 1, p.accent);
                        }
                    }
                    Scene::Waiting => {
                        rect(4, 1, 12, 8, p.warning);
                        rect(6, 3, 2, 4, p.on_accent);
                        rect(12, 3, 2, 4, p.on_accent);
                    }
                    _ => {}
                }
            }
        },
    )
    .w(px(40.))
    .h(px(22.))
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

pub fn cartridge(p: Palette, kind: Cartridge, selected: bool) -> impl IntoElement {
    let pattern = kind.pixels();
    let color = match kind {
        Cartridge::Codex => p.accent,
        Cartridge::Claude => p.warning,
        Cartridge::Api => crate::appearance::mix(p.fg, p.accent, 48),
        Cartridge::Worker => p.muted,
    };
    let ink = if selected {
        color
    } else {
        crate::appearance::mix(color, p.muted, 32)
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

/// An inset, stepped sunset for the cabinet header. Drawn once per normal paint,
/// in the active theme's inks. There is no animation timer on the resting screen.
pub fn horizon(p: Palette) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let w = f32::from(b.size.width);
            let h = f32::from(b.size.height);
            let mix = crate::appearance::mix;
            let mut rect = |x: f32, y: f32, width: f32, height: f32, color: u32| {
                let left = x.max(0.);
                let width = (x + width).min(w) - left;
                if width > 0. && y < h {
                    window.paint_quad(fill(
                        Bounds::new(
                            b.origin + point(px(left), px(y)),
                            size(px(width), px(height.min(h - y))),
                        ),
                        rgb(color),
                    ));
                }
            };
            // Two-pixel rows give the sun and distant ridge the same pitch as the OX lettering.
            let sun = w - 55.;
            for (i, half) in [
                5., 10., 13., 15., 17., 17., 18., 18., 17., 16., 14., 11., 7.,
            ]
            .iter()
            .enumerate()
            {
                let y = 6. + i as f32 * 2.;
                if i != 8 && i != 10 && i != 12 {
                    rect(
                        sun - half,
                        y,
                        half * 2.,
                        2.,
                        mix(p.nav, p.warning, if p.light { 57 } else { 76 }),
                    );
                }
            }
            for (x, y) in [(24., 9.), (62., 4.), (104., 15.), (132., 7.)] {
                rect(w - x, y, 2., 2., mix(p.nav, p.accent, 47));
            }
            let hills = [
                0., 2., 4., 6., 8., 6., 4., 2., 4., 6., 8., 10., 12., 10., 8., 6., 4., 2., 4., 6.,
                4., 2.,
            ];
            for (i, rise) in hills.iter().enumerate() {
                let x = w - 132. + i as f32 * 6.;
                rect(
                    x,
                    h - 10. - rise,
                    6.,
                    rise + 10.,
                    mix(p.nav, p.accent, if p.light { 11 } else { 19 }),
                );
                rect(x, h - 10. - rise, 6., 2., mix(p.nav, p.accent, 40));
            }
            rect(0., h - 2., w, 2., mix(p.nav, p.accent, 43));
        },
    )
    .absolute()
    .inset_0()
}
