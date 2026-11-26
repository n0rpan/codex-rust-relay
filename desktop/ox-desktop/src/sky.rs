//! The living horizon: time of day, sun and moon, and weather for the header.
//!
//! The clock-derived part (phase, sun or moon position, moon phase) is computed here with the
//! NOAA solar approximation and needs no network. The weather part comes from the sibling
//! `ox weather --json`, which resolves a city-level place from the system timezone and talks to
//! MET Norway (CC BY 4.0). Without a place the sky follows the local clock with a fixed
//! 06:00 to 18:00 day; without weather it is simply clear. Nothing here asks for a location.

use crate::appearance::{Palette, mix};
use gpui::{prelude::*, *};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Night,
    Dawn,
    Day,
    Dusk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    Clear,
    PartlyCloudy,
    Cloudy,
    Rain,
    Snow,
    Sleet,
    Fog,
    Thunder,
}

impl Condition {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "clear" => Self::Clear,
            "partly_cloudy" => Self::PartlyCloudy,
            "cloudy" => Self::Cloudy,
            "rain" => Self::Rain,
            "snow" => Self::Snow,
            "sleet" => Self::Sleet,
            "fog" => Self::Fog,
            "thunder" => Self::Thunder,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::Clear => "Clear",
            Self::PartlyCloudy => "Partly cloudy",
            Self::Cloudy => "Cloudy",
            Self::Rain => "Rain",
            Self::Snow => "Snow",
            Self::Sleet => "Sleet",
            Self::Fog => "Fog",
            Self::Thunder => "Thunderstorm",
        }
    }

    fn overcast(self) -> bool {
        !matches!(self, Self::Clear | Self::PartlyCloudy)
    }

    /// Conditions with moving pixels. Clear skies are static.
    fn animated(self) -> bool {
        self != Self::Clear
    }
}

/// Everything the horizon draws. `x` runs 0 to 1 along the arc; `height` is the body's
/// elevation as a share of the day's maximum (negative while it is behind the hills).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sky {
    pub phase: Phase,
    pub moon: bool,
    pub x: f32,
    pub height: f32,
    /// 0 new, 0.5 full, back to 1.
    pub moon_phase: f32,
    pub condition: Condition,
    /// True only while real forecast data drives `condition`.
    pub live: bool,
    pub temperature: Option<i16>,
    /// Animation counter for the weather particles. Zero when motion is reduced.
    pub frame: u32,
}

impl Default for Sky {
    fn default() -> Self {
        Self {
            phase: Phase::Day,
            moon: false,
            x: 0.7,
            height: 0.5,
            moon_phase: 0.5,
            condition: Condition::Clear,
            live: false,
            temperature: None,
            frame: 0,
        }
    }
}

// ------------------------------------------------------------------ astronomy

/// Solar quantities for one instant (NOAA approximation, good to about a minute).
#[derive(Clone, Copy, Debug)]
struct Sun {
    /// Degrees above the horizon, without refraction.
    elevation: f64,
    /// Degrees from solar noon, negative in the morning.
    hour_angle: f64,
    /// Highest elevation today.
    peak: f64,
    /// Hour angle at sunrise and sunset, or None under a midnight sun or polar night.
    rise: Option<f64>,
}

fn julian_day(unix: f64) -> f64 {
    unix / 86_400.0 + 2_440_587.5
}

/// Declination (radians) and equation of time (minutes).
fn sun_terms(jd: f64) -> (f64, f64) {
    let t = (jd - 2_451_545.0) / 36_525.0;
    let l0 = (280.46646 + t * (36000.76983 + t * 0.0003032)).rem_euclid(360.0);
    let m = (357.52911 + t * (35999.05029 - 0.0001537 * t)).to_radians();
    let e = 0.016708634 - t * (0.000042037 + 0.0000001267 * t);
    let c = m.sin() * (1.914602 - t * (0.004817 + 0.000014 * t))
        + (2.0 * m).sin() * (0.019993 - 0.000101 * t)
        + (3.0 * m).sin() * 0.000289;
    let omega = (125.04 - 1934.136 * t).to_radians();
    let apparent = (l0 + c - 0.00569 - 0.00478 * omega.sin()).to_radians();
    let obliquity = 23.0
        + (26.0 + (21.448 - t * (46.815 + t * (0.00059 - t * 0.001813))) / 60.0) / 60.0
        + 0.00256 * omega.cos();
    let obliquity = obliquity.to_radians();
    let declination = (obliquity.sin() * apparent.sin()).asin();
    let y = (obliquity / 2.0).tan().powi(2);
    let l0 = l0.to_radians();
    let equation = 4.0
        * (y * (2.0 * l0).sin() - 2.0 * e * m.sin() + 4.0 * e * y * m.sin() * (2.0 * l0).cos()
            - 0.5 * y * y * (4.0 * l0).sin()
            - 1.25 * e * e * (2.0 * m).sin())
        .to_degrees();
    (declination, equation)
}

/// The sun at `unix` seconds for a place (degrees, east and north positive).
fn sun_at(unix: f64, lat: f64, lon: f64) -> Sun {
    // Day-level quantities use that day's UTC noon, as the NOAA worksheet does.
    let noon_unix = (unix / 86_400.0).floor() * 86_400.0 + 43_200.0;
    let (decl, _) = sun_terms(julian_day(noon_unix));
    let (_, equation) = sun_terms(julian_day(unix));
    let minutes = (unix / 60.0).rem_euclid(1440.0);
    let solar_time = minutes + equation + 4.0 * lon;
    let hour_angle = (solar_time / 4.0 - 180.0 + 180.0).rem_euclid(360.0) - 180.0;
    let (phi, ha) = (lat.to_radians(), hour_angle.to_radians());
    let elevation = (phi.sin() * decl.sin() + phi.cos() * decl.cos() * ha.cos())
        .asin()
        .to_degrees();
    let cos_rise =
        (90.833f64.to_radians().cos() - phi.sin() * decl.sin()) / (phi.cos() * decl.cos());
    let rise = (cos_rise.abs() <= 1.0).then(|| cos_rise.acos().to_degrees());
    Sun {
        elevation,
        hour_angle,
        peak: 90.0 - (lat - decl.to_degrees()).abs(),
        rise,
    }
}

/// Sunrise and sunset in minutes after 00:00 UTC for the day containing `unix`.
#[cfg(test)]
fn sun_times(unix: f64, lat: f64, lon: f64) -> (Option<f64>, Option<f64>) {
    let sun = sun_at(unix, lat, lon);
    let noon_unix = (unix / 86_400.0).floor() * 86_400.0 + 43_200.0;
    let noon = 720.0 - 4.0 * lon - sun_terms(julian_day(noon_unix)).1;
    match sun.rise {
        Some(ha) => (Some(noon - 4.0 * ha), Some(noon + 4.0 * ha)),
        None => (None, None),
    }
}

/// Moon phase as a fraction of the synodic month: 0 new, 0.5 full.
pub fn moon_phase(unix: f64) -> f32 {
    const SYNODIC: f64 = 29.530_588_853;
    // A new moon: 2000-01-06 18:14 UTC.
    let age = (julian_day(unix) - 2_451_550.26).rem_euclid(SYNODIC);
    (age / SYNODIC) as f32
}

impl Sky {
    /// The clock-derived sky. `place` is (lat, lon); `local_minutes` is minutes after local
    /// midnight, used only when there is no place.
    pub fn at(
        unix: i64,
        place: Option<(f64, f64)>,
        local_minutes: f64,
        condition: Option<(Condition, Option<i16>)>,
    ) -> Self {
        let now = unix as f64;
        let sun = match place {
            Some((lat, lon)) => sun_at(now, lat, lon),
            None => {
                // A fixed equinox day at the equator: sunrise 06:00, sunset 18:00 local time.
                let ha = local_minutes / 4.0 - 180.0;
                Sun {
                    elevation: ha.to_radians().cos().asin().to_degrees(),
                    hour_angle: ha,
                    peak: 90.0,
                    rise: Some(90.0),
                }
            }
        };
        let phase = if sun.elevation < -6.0 {
            Phase::Night
        } else if sun.elevation < 3.0 {
            if sun.hour_angle < 0.0 {
                Phase::Dawn
            } else {
                Phase::Dusk
            }
        } else {
            Phase::Day
        };
        let rise = sun.rise.unwrap_or(90.0);
        let (moon, x, height) = if phase == Phase::Night {
            // The moon crosses the same arc during the night, sunset to sunrise.
            let since_noon = sun.hour_angle.rem_euclid(360.0);
            let night = ((since_noon - rise) / (360.0 - 2.0 * rise)).clamp(0.04, 0.96);
            (
                true,
                night as f32,
                0.25 + 0.6 * (std::f64::consts::PI * night).sin() as f32,
            )
        } else {
            (
                false,
                (0.5 + sun.hour_angle / (2.0 * rise)).clamp(0.03, 0.97) as f32,
                (sun.elevation / sun.peak.max(5.0)).clamp(-0.3, 1.0) as f32,
            )
        };
        Self {
            phase,
            moon,
            x,
            height,
            moon_phase: moon_phase(now),
            condition: condition.map_or(Condition::Clear, |c| c.0),
            live: condition.is_some(),
            temperature: condition.and_then(|c| c.1),
            frame: 0,
        }
    }
}

// ------------------------------------------------------------------- provider

/// What the last `ox weather --json` said.
#[derive(Clone, Copy, Default)]
struct Live {
    place: Option<(f64, f64)>,
    weather: Option<(Condition, Option<i16>)>,
}

#[derive(Clone, Copy, Default)]
struct Frame(u32);

struct Current(Sky);
impl Global for Current {}
impl Global for Live {}
impl Global for Frame {}

/// The sky the header should draw now. Time-only defaults before the provider has run.
pub fn now(cx: &App) -> Sky {
    cx.try_global::<Current>()
        .map_or_else(Sky::default, |c| c.0)
}

fn local_minutes() -> f64 {
    use chrono::Timelike;
    let t = chrono::Local::now();
    f64::from(t.hour() * 60 + t.minute())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn any_visible(cx: &mut App) -> bool {
    cx.windows().into_iter().any(|w| {
        w.update(cx, |_, window, _| window.is_visible())
            .unwrap_or(false)
    })
}

/// Recompute the sky; true when something on screen is still moving.
fn tick(cx: &mut App) -> bool {
    if !any_visible(cx) {
        return false;
    }
    let live = cx.try_global::<Live>().copied().unwrap_or_default();
    let weather = if cx
        .global::<crate::preferences::behavior::Active>()
        .0
        .live_weather
    {
        live.weather
    } else {
        None
    };
    let motion = Palette::get(cx).motion;
    let mut sky = Sky::at(unix_now(), live.place, local_minutes(), weather);
    let animating = motion && sky.condition.animated();
    let frame = cx.try_global::<Frame>().map_or(0, |f| f.0);
    let frame = if animating {
        frame.wrapping_add(1)
    } else {
        frame
    };
    cx.set_global(Frame(frame));
    sky.frame = if motion { frame } else { 0 };
    if cx.try_global::<Current>().map(|c| c.0) != Some(sky) {
        cx.set_global(Current(sky));
        cx.refresh_windows();
    }
    animating
}

/// Start the clock and weather refreshers. The clock part wakes once a minute (every 400 ms
/// only while weather particles are moving in a visible window); it does nothing while hidden.
pub fn start(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            let animating = cx.update(tick);
            let wait = if animating { 400 } else { 60_000 };
            cx.background_executor()
                .timer(Duration::from_millis(wait))
                .await;
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        loop {
            let wait = fetch(cx).await;
            cx.background_executor().timer(wait).await;
        }
    })
    .detach();
}

/// Fetch again now, for example after the preference changed.
pub fn reload(cx: &mut App) {
    cx.spawn(async move |cx| {
        fetch(cx).await;
        cx.update(|cx| {
            tick(cx);
        });
    })
    .detach();
}

struct Answer {
    place: Option<(f64, f64)>,
    weather: Option<(Condition, Option<i16>)>,
    expires: Option<i64>,
}

fn parse_answer(text: &str) -> Option<Answer> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let place = match (value["lat"].as_f64(), value["lon"].as_f64()) {
        (Some(lat), Some(lon)) => Some((lat, lon)),
        _ => None,
    };
    let weather = value["condition"]
        .as_str()
        .and_then(Condition::parse)
        .map(|c| (c, value["temperature"].as_f64().map(|t| t.round() as i16)));
    Some(Answer {
        place,
        weather,
        expires: value["expires"].as_i64(),
    })
}

/// How long to wait before the next `ox weather` run: until the forecast expires, but never
/// sooner than five minutes or later than thirty (`ox weather` itself keeps MET's schedule).
fn next_wait(expires: Option<i64>, now: i64) -> Duration {
    let secs = expires.map_or(1_800, |e| (e - now).clamp(300, 1_800));
    Duration::from_secs(secs as u64)
}

async fn fetch(cx: &AsyncApp) -> Duration {
    let output = cx
        .background_spawn(async {
            std::process::Command::new(crate::platform::ox())
                .args(["weather", "--json"])
                .env("PATH", crate::platform::child_path())
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
        })
        .await;
    let Some(answer) = output.as_deref().and_then(parse_answer) else {
        return Duration::from_secs(600);
    };
    let wait = next_wait(answer.expires, unix_now());
    cx.update(|cx| {
        cx.set_global(Live {
            place: answer.place,
            weather: answer.weather,
        });
        tick(cx);
    });
    wait
}

// ------------------------------------------------------------------- painting

const STARS: [(f32, f32); 12] = [
    (24., 9.),
    (62., 4.),
    (104., 15.),
    (132., 7.),
    (40., 21.),
    (150., 12.),
    (88., 8.),
    (12., 14.),
    (118., 25.),
    (70., 23.),
    (160., 5.),
    (141., 23.),
];

const HILLS: [f32; 22] = [
    0., 2., 4., 6., 8., 6., 4., 2., 4., 6., 8., 10., 12., 10., 8., 6., 4., 2., 4., 6., 4., 2.,
];

const CLOUD_SMALL: [(f32, f32, f32); 3] = [(6., 0., 10.), (2., 2., 18.), (0., 4., 26.)];
const CLOUD_LARGE: [(f32, f32, f32); 4] =
    [(8., 0., 12.), (4., 2., 20.), (0., 4., 30.), (2., 6., 26.)];

/// An inset, stepped sky for the cabinet header: tinted by the time of day, with the sun or
/// moon on an arc and the weather in front of a stepped ridge. Drawn in the active theme's inks
/// in two-pixel rows. Nothing animates unless the sky says so.
pub fn horizon(p: Palette, sky: &Sky) -> impl IntoElement {
    let sky = *sky;
    let tooltip = sky.live.then(|| {
        let temperature = sky
            .temperature
            .map(|t| format!(", {t}\u{b0}C"))
            .unwrap_or_default();
        format!(
            "{}{temperature}. Weather data from MET Norway",
            sky.condition.label()
        )
    });
    div()
        .id("horizon")
        .absolute()
        .inset_0()
        .when_some(tooltip, |el, text| {
            el.tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(text.clone()).build(window, cx)
            })
        })
        .child(
            canvas(
                |_, _, _| (),
                move |b, _, window, _| paint(p, sky, b, window),
            )
            .size_full(),
        )
}

fn paint(p: Palette, sky: Sky, b: Bounds<Pixels>, window: &mut Window) {
    let w = f32::from(b.size.width);
    let h = f32::from(b.size.height);
    let frame = sky.frame as f32;
    let mut rect = |x: f32, y: f32, width: f32, height: f32, color: u32| {
        let left = x.max(0.);
        let width = (x + width).min(w) - left;
        let top = y.max(0.);
        let height = (y + height).min(h) - top;
        if width > 0. && height > 0. {
            window.paint_quad(fill(
                Bounds::new(
                    b.origin + point(px(left), px(top)),
                    size(px(width), px(height)),
                ),
                rgb(color),
            ));
        }
    };
    let dark_ink = |dark: u32, light: u32| if p.light { light } else { dark };
    let overcast = sky.condition.overcast();

    // Time-of-day tint over the whole header, kept within a few percent of the nav token.
    let tint = match sky.phase {
        Phase::Night => Some((p.accent, dark_ink(9, 13))),
        Phase::Dawn => Some((p.warning, dark_ink(8, 10))),
        Phase::Dusk => Some((p.error, dark_ink(8, 9))),
        Phase::Day => None,
    };
    if let Some((color, amount)) = tint {
        rect(0., 0., w, h, mix(p.nav, color, amount));
    }

    // Stars: only when the sky is open, and a slow twinkle while motion is allowed.
    let stars = match (sky.phase, sky.condition) {
        (Phase::Night, Condition::Clear) => 12,
        (Phase::Night, Condition::PartlyCloudy) => 6,
        (Phase::Dawn | Phase::Dusk, Condition::Clear | Condition::PartlyCloudy) => 3,
        _ => 0,
    };
    for (i, (x, y)) in STARS.iter().take(stars).enumerate() {
        let dim = sky.frame > 0 && (sky.frame / 6 + i as u32).is_multiple_of(9);
        let amount = if dim { 20 } else { dark_ink(52, 46) };
        rect(w - x, *y, 2., 2., mix(p.nav, p.fg, amount));
    }

    // The body travels the arc from left to right; hills in front hide it near the horizon.
    let (x0, x1) = (w - 150., w - 20.);
    let r = if sky.moon { 8. } else { 12. };
    let cx = x0 + sky.x * (x1 - x0);
    let cy = (h - 14.) - sky.height * (h - 14. - r - 2.);
    if sky.moon {
        let ink = mix(p.nav, p.fg, dark_ink(82, 58));
        let earthshine = mix(p.nav, p.fg, dark_ink(14, 12));
        let k = (std::f32::consts::TAU * sky.moon_phase).cos();
        let mut dy = -r;
        while dy < r {
            let mid = dy + 1.;
            let half = (r * r - mid * mid).max(0.).sqrt();
            let half = (half / 2.).round() * 2.;
            rect(cx - half, cy + dy, half * 2., 2., earthshine);
            let (lit_from, lit_to) = if sky.moon_phase <= 0.5 {
                (half * k, half)
            } else {
                (-half, -half * k)
            };
            let (a, c) = ((lit_from / 2.).round() * 2., (lit_to / 2.).round() * 2.);
            if c > a {
                rect(cx + a, cy + dy, c - a, 2., ink);
            }
            dy += 2.;
        }
    } else {
        let base = match sky.phase {
            Phase::Dawn | Phase::Dusk => mix(p.warning, p.error, 38),
            _ => p.warning,
        };
        let mut ink = mix(p.nav, base, dark_ink(76, 57));
        if overcast {
            ink = mix(ink, p.nav, 55);
        }
        let mut dy = -r;
        let mut row = 0;
        while dy < r {
            let mid = dy + 1.;
            let half = ((r * r - mid * mid).max(0.).sqrt() / 2.).round() * 2.;
            // Retro slits across the lower third, as on the cabinet sun.
            if !(dy > r * 0.15 && row % 2 == 1) {
                rect(cx - half, cy + dy, half * 2., 2., ink);
            }
            dy += 2.;
            row += 1;
        }
    }

    // Clouds drift left to right behind the ridge.
    let (clouds, cloud_amount): (&[(f32, f32)], u32) = match sky.condition {
        Condition::Clear | Condition::Fog => (&[], 0),
        Condition::PartlyCloudy => (&[(40., 8.)], 26),
        Condition::Cloudy => (&[(10., 6.), (70., 16.), (130., 4.)], 36),
        _ => (&[(10., 4.), (60., 10.), (110., 3.), (150., 12.)], 46),
    };
    let span = x1 - x0 + 60.;
    for (i, (seed, y)) in clouds.iter().enumerate() {
        let shape: &[(f32, f32, f32)] = if i % 2 == 0 {
            &CLOUD_SMALL
        } else {
            &CLOUD_LARGE
        };
        let x = x0 - 30. + (seed + frame).rem_euclid(span);
        let x = (x / 2.).floor() * 2.;
        for (dx, dy, width) in shape {
            rect(
                x + dx,
                y + dy,
                *width,
                2.,
                mix(p.nav, p.muted, cloud_amount),
            );
        }
    }

    // The stepped ridge. Night and the cloudy dusk keep the theme's inks.
    for (i, rise) in HILLS.iter().enumerate() {
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

    // Weather in front of the ridge.
    let fall = (h - 26.).max(8.);
    match sky.condition {
        Condition::Rain | Condition::Thunder | Condition::Sleet => {
            for i in 0..14u32 {
                if sky.condition == Condition::Sleet && i % 2 == 1 {
                    continue;
                }
                let x = w - 150. + i as f32 * 10. + ((i * 7) % 4) as f32 * 2.;
                let y = 14. + ((frame * 3. + i as f32 * 13.) % fall);
                rect(x, (y / 2.).floor() * 2., 2., 4., mix(p.nav, p.accent, 62));
            }
        }
        _ => {}
    }
    if matches!(sky.condition, Condition::Snow | Condition::Sleet) {
        for i in 0..12u32 {
            if sky.condition == Condition::Sleet && i % 2 == 0 {
                continue;
            }
            let wobble = (((sky.frame / 4 + i) % 3) as f32 - 1.) * 2.;
            let x = w - 146. + i as f32 * 11. + wobble;
            let y = 14. + ((frame + i as f32 * 11.) % fall);
            rect(
                x,
                (y / 2.).floor() * 2.,
                2.,
                2.,
                mix(p.nav, p.fg, dark_ink(78, 60)),
            );
        }
    }
    if sky.condition == Condition::Fog {
        for (k, y) in [h - 26., h - 18., h - 11.].iter().enumerate() {
            let x = w - 170. + (k as f32 * 53. + frame * 0.5).rem_euclid(200.) - 20.;
            rect((x / 2.).floor() * 2., *y, 54., 2., mix(p.nav, p.muted, 34));
            rect(
                (x / 2.).floor() * 2. - 70.,
                *y + 2.,
                40.,
                2.,
                mix(p.nav, p.muted, 24),
            );
        }
    }
    if sky.condition == Condition::Thunder && sky.frame % 53 >= 50 {
        // One bright frame and a stepped bolt; never drawn while motion is reduced.
        rect(0., 0., w, h, mix(p.nav, p.fg, 12));
        let bx = w - 86.;
        rect(bx + 2., 10., 4., 6., p.warning);
        rect(bx - 2., 16., 6., 6., p.warning);
        rect(bx, 22., 4., 8., p.warning);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Condition, Duration, Phase, Sky, moon_phase, next_wait, parse_answer, sun_at, sun_times,
    };

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
            .and_utc()
            .timestamp()
    }

    const OSLO: (f64, f64) = (59.91, 10.75);

    #[test]
    fn oslo_midsummer_sun_times() {
        // 2026-06-21: sunrise about 03:54 and sunset about 22:43 local (CEST, UTC+2).
        let (rise, set) = sun_times(utc(2026, 6, 21, 12, 0) as f64, OSLO.0, OSLO.1);
        let (rise, set) = (rise.unwrap() + 120.0, set.unwrap() + 120.0);
        assert!((rise - (3.0 * 60.0 + 54.0)).abs() <= 3.0, "sunrise {rise}");
        assert!((set - (22.0 * 60.0 + 43.0)).abs() <= 3.0, "sunset {set}");
    }

    #[test]
    fn polar_day_and_night_have_no_sunrise() {
        let tromso = (69.65, 18.96);
        assert_eq!(
            sun_times(utc(2026, 6, 21, 12, 0) as f64, tromso.0, tromso.1),
            (None, None)
        );
        assert_eq!(
            sun_times(utc(2026, 12, 21, 12, 0) as f64, tromso.0, tromso.1),
            (None, None)
        );
        // The midnight sun stays above the horizon at local midnight.
        let midnight = sun_at(utc(2026, 6, 21, 22, 0) as f64, tromso.0, tromso.1);
        assert!(midnight.elevation > 0.0);
    }

    #[test]
    fn moon_phase_follows_known_syzygies() {
        let wrap = |p: f32| p.min(1.0 - p);
        assert!(wrap(moon_phase(utc(2000, 1, 6, 18, 14) as f64)) < 0.02);
        assert!((moon_phase(utc(2000, 1, 21, 4, 40) as f64) - 0.5).abs() < 0.03);
    }

    #[test]
    fn phase_selection_follows_the_sun() {
        let at = |h, m| Sky::at(utc(2026, 6, 21, h, m), Some(OSLO), 0.0, None);
        // 01:00 UTC is 03:00 CEST, before the 03:54 sunrise on midsummer: dawn. 04:00 UTC is day.
        assert_eq!(at(1, 0).phase, Phase::Dawn);
        assert_eq!(at(4, 0).phase, Phase::Day);
        assert_eq!(at(11, 0).phase, Phase::Day);
        // Midsummer night in Oslo never gets darker than twilight; December does get night.
        let winter = |h| Sky::at(utc(2026, 12, 21, h, 0), Some(OSLO), 0.0, None);
        assert_eq!(winter(0).phase, Phase::Night);
        assert!(winter(0).moon);
        assert_eq!(winter(8).phase, Phase::Dawn);
        assert_eq!(winter(11).phase, Phase::Day);
        assert!(!winter(11).moon);
        assert_eq!(winter(14).phase, Phase::Dusk);
        let noon = winter(11);
        assert!(noon.x > 0.35 && noon.x < 0.65 && noon.height > 0.9);
        assert!(winter(8).x < winter(11).x && winter(11).x < winter(14).x);
    }

    #[test]
    fn without_a_place_the_clock_decides() {
        let at = |minutes| Sky::at(0, None, minutes, None);
        assert_eq!(at(0.0).phase, Phase::Night);
        assert_eq!(at(5.0 * 60.0 + 50.0).phase, Phase::Dawn);
        assert_eq!(at(12.0 * 60.0).phase, Phase::Day);
        assert!((at(12.0 * 60.0).x - 0.5).abs() < 0.01);
        assert_eq!(at(18.0 * 60.0 + 10.0).phase, Phase::Dusk);
        assert_eq!(at(23.0 * 60.0).phase, Phase::Night);
        assert!(!at(12.0 * 60.0).live);
    }

    #[test]
    fn weather_answers_parse_and_schedule() {
        let answer = parse_answer(
            r#"{"zone":"Europe/Oslo","lat":59.92,"lon":10.75,"condition":"thunder","temperature":3.6,"expires":10000}"#,
        )
        .unwrap();
        assert_eq!(answer.place, Some((59.92, 10.75)));
        assert_eq!(answer.weather, Some((Condition::Thunder, Some(4))));
        let offline = parse_answer(r#"{"zone":"Europe/Oslo","condition":null}"#).unwrap();
        assert!(offline.place.is_none() && offline.weather.is_none());
        assert!(parse_answer("not json").is_none());
        assert_eq!(next_wait(Some(10_000), 9_900), Duration::from_secs(300));
        assert_eq!(next_wait(Some(10_900), 10_000), Duration::from_secs(900));
        assert_eq!(next_wait(Some(99_000), 10_000), Duration::from_secs(1_800));
        assert_eq!(next_wait(None, 10_000), Duration::from_secs(1_800));
        assert_eq!(
            Condition::parse("partly_cloudy"),
            Some(Condition::PartlyCloudy)
        );
        assert_eq!(Condition::parse("hail"), None);
    }

    #[test]
    fn weather_attaches_to_the_sky() {
        let sky = Sky::at(0, None, 720.0, Some((Condition::Rain, Some(7))));
        assert!(sky.live && sky.condition == Condition::Rain && sky.temperature == Some(7));
    }
}
