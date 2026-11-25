//! Test-only registry of the controls each window laid out in its last frame.
//!
//! Window tests read it through the desk stats socket (`OX_DESK_STATS_SOCKET`)
//! to find a button and to see whether it is enabled, instead of reading the
//! screen with OCR. Without that variable every entry point returns after one
//! atomic load: no element is added, no lock is taken and nothing allocates.
use gpui::{
    App, Bounds, ElementId, ParentElement, Pixels, Point, SharedString, Styled, Window, WindowId,
    canvas,
};
use gpui_component::button::Button;
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::{LazyLock, Mutex};

static ACTIVE: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("OX_DESK_STATS_SOCKET").is_some());
static WINDOWS: Mutex<Vec<Frame>> = Mutex::new(Vec::new());

struct Frame {
    window: WindowId,
    title: &'static str,
    x11: Option<u64>,
    /// Counts drawn frames, so a test can wait for one newer than a state it saw.
    frame: u64,
    state: Value,
    controls: Vec<Control>,
}
struct Control {
    id: String,
    label: SharedString,
    enabled: bool,
    /// Visible part in device pixels from the window's top-left corner.
    bounds: [i32; 4],
    /// Centre of the visible part in logical pixels, where `press` clicks.
    centre: Point<Pixels>,
}

pub fn active() -> bool {
    *ACTIVE
}

/// Called from a window's root render, before any control prepaints: drops the
/// controls of the previous frame and records the window's test-facing state.
/// Views are not cached in OX, so every drawn frame renders the root again.
pub fn begin_frame<S: Serialize>(window: &Window, title: &'static str, state: impl FnOnce() -> S) {
    if !active() {
        return;
    }
    let id = window.window_handle().window_id();
    let state = serde_json::to_value(state()).unwrap_or(Value::Null);
    let mut windows = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(frame) = windows.iter_mut().find(|f| f.window == id) {
        frame.controls.clear();
        frame.frame += 1;
        frame.state = state;
    } else {
        windows.push(Frame {
            window: id,
            title,
            x11: x11(window),
            frame: 1,
            state,
            controls: Vec::new(),
        });
    }
}

pub fn forget(window: WindowId) {
    if active() {
        let mut windows = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
        windows.retain(|f| f.window != window);
    }
}

/// Adds an invisible child covering the button's content box that records the
/// button's visible bounds when it prepaints.
pub fn mark(button: Button, id: &ElementId, label: &SharedString, enabled: bool) -> Button {
    if !active() {
        return button;
    }
    let id = id.to_string();
    let label = label.clone();
    button.child(
        canvas(
            move |bounds, window, _: &mut App| record(window, id, label, enabled, bounds),
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full(),
    )
}

fn record(window: &Window, id: String, label: SharedString, enabled: bool, b: Bounds<Pixels>) {
    let visible = b.intersect(&window.content_mask().bounds);
    if visible.size.width <= Pixels::ZERO || visible.size.height <= Pixels::ZERO {
        return;
    }
    let scale = window.scale_factor();
    let device = |v: Pixels| (f32::from(v) * scale).round() as i32;
    let bounds = [
        device(visible.origin.x),
        device(visible.origin.y),
        device(visible.size.width),
        device(visible.size.height),
    ];
    let window = window.window_handle().window_id();
    let mut windows = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(frame) = windows.iter_mut().find(|f| f.window == window) {
        frame.controls.push(Control {
            id,
            label,
            enabled,
            bounds,
            centre: visible.center(),
        });
    }
}

/// Resolves `press <query>`: exact id, then exact label, then case-insensitive label.
/// Several matches at the first level that matches are an error naming the candidates.
pub fn find(query: &str) -> Result<(WindowId, Point<Pixels>), String> {
    let windows = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    let all: Vec<(&Frame, &Control)> = windows
        .iter()
        .flat_map(|f| f.controls.iter().map(move |c| (f, c)))
        .collect();
    let levels: [&dyn Fn(&Control) -> bool; 3] =
        [&|c| c.id == query, &|c| c.label.as_ref() == query, &|c| {
            c.label.eq_ignore_ascii_case(query)
        }];
    for level in levels {
        let hits: Vec<_> = all.iter().filter(|(_, c)| level(c)).collect();
        match hits.as_slice() {
            [] => continue,
            [(f, c)] if c.enabled => return Ok((f.window, c.centre)),
            [(_, c)] => return Err(format!("control {:?} is disabled", c.id)),
            many => {
                let names: Vec<_> = many
                    .iter()
                    .map(|(f, c)| format!("{} [{}] in {}", c.label, c.id, f.title))
                    .collect();
                return Err(format!("ambiguous {query:?}: {}", names.join("; ")));
            }
        }
    }
    Err(format!("no control matches {query:?}"))
}

/// `{"windows": [...], "layout": [...]}` for the stats snapshot.
pub fn snapshot() -> (Value, Value) {
    let windows = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    let mut layout = Vec::new();
    let mut list = Vec::new();
    for frame in windows.iter() {
        list.push(json!({
            "title": frame.title, "x11": frame.x11, "frame": frame.frame, "state": frame.state,
        }));
        for c in &frame.controls {
            let [x, y, w, h] = c.bounds;
            layout.push(json!({
                "window": frame.title, "id": c.id, "label": c.label.as_ref(), "enabled": c.enabled,
                "x": x, "y": y, "w": w, "h": h,
            }));
        }
    }
    (Value::Array(list), Value::Array(layout))
}

/// The X11 window id lets a test address the right window without searching titles.
// `c_ulong` is `u64` on 64-bit Linux but not on every target.
#[allow(clippy::unnecessary_cast)]
fn x11(window: &Window) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Xlib(h) => Some(h.window as u64),
        RawWindowHandle::Xcb(h) => Some(u64::from(h.window.get())),
        _ => None,
    }
}

/// What Settings shows, for the stats snapshot.
#[derive(Serialize)]
pub struct SettingsProbe {
    pub page: String,
    pub remote: Option<RemoteProbe>,
}

/// What Settings → Remote access shows, for the stats snapshot. `finished`
/// counts completed status operations, so a test waits for the one it started.
#[derive(Serialize)]
pub struct RemoteProbe {
    pub checking: bool,
    pub finished: u64,
    pub notice: Option<String>,
    pub permission: Option<String>,
    pub enabled: bool,
    pub origin: Option<String>,
}
