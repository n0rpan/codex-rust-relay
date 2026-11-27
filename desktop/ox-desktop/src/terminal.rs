//! A user-operated local PTY. No host protocol, conversation or telemetry uses its input/output.
use crate::appearance::Step;
mod display;
mod remote;
use ox_pty as pty;
pub(crate) use remote::open as open_remote;

use crate::{
    appearance::Palette,
    kit::{button, col, row, small},
};
use gpui::*;
use gpui_component::Root;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

pub use ox_pty::TerminalCommand;
type OnExit = Box<dyn FnOnce(Option<i32>, &mut App)>;
pub fn open(
    project: PathBuf,
    command: Option<TerminalCommand>,
    on_exit: impl FnOnce(Option<i32>, &mut App) + 'static,
    cx: &mut App,
) -> Result<(), String> {
    open_review(project, command, Box::new(|| Ok(())), Box::new(on_exit), cx)
}
pub fn open_checked(
    project: PathBuf,
    command: TerminalCommand,
    prepare: impl FnOnce() -> Result<(), String> + Send + 'static,
    on_exit: impl FnOnce(Option<i32>, &mut App) + 'static,
    cx: &mut App,
) -> Result<(), String> {
    open_review(
        project,
        Some(command),
        Box::new(prepare),
        Box::new(on_exit),
        cx,
    )
}
fn open_review(
    project: PathBuf,
    command: Option<TerminalCommand>,
    prepare: pty::Check,
    on_exit: OnExit,
    cx: &mut App,
) -> Result<(), String> {
    if !project.is_absolute() {
        return Err("The terminal needs an absolute project path.".into());
    }
    let shell = command.is_none();
    let mut command = command.unwrap_or_else(TerminalCommand::shell);
    command.resolve()?;
    let preview = command.preview()?;
    let bounds = Bounds::centered(None, size(px(800.), px(490.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(360.), px(230.))),
            titlebar: Some(TitlebarOptions {
                title: Some("OX Terminal".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| {
            let view = cx.new(|cx| {
                let focus = cx.focus_handle();
                focus.focus(window, cx);
                cx.on_release(|s: &mut Panel, cx| {
                    s.terminal.take();
                    if let Some(callback) = s.on_exit.take() {
                        callback(None, cx);
                    }
                })
                .detach();
                Panel {
                    project,
                    command,
                    preview,
                    shell,
                    prepare: Some(prepare),
                    on_exit: Some(on_exit),
                    focus,
                    screen: None,
                    terminal: None,
                    status: None,
                }
            });
            cx.new(|cx| Root::new(view, window, cx))
        },
    )
    .map_err(|e| e.to_string())?;
    cx.activate(true);
    Ok(())
}

struct Panel {
    project: PathBuf,
    command: TerminalCommand,
    preview: String,
    shell: bool,
    prepare: Option<pty::Check>,
    on_exit: Option<OnExit>,
    focus: FocusHandle,
    screen: Option<Arc<Mutex<pty::Screen>>>,
    terminal: Option<pty::Terminal>,
    status: Option<String>,
}

const LINE: f32 = 18.;
const FONT: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else {
    "monospace"
};

fn dimensions(window: &Window, cx: &App) -> pty::Size {
    let width = cx
        .text_system()
        .advance(cx.text_system().resolve_font(&font(FONT)), px(13.), 'M')
        .map_or(8., |s| f32::from(s.width).max(1.));
    let viewport = window.viewport_size();
    (
        ((f32::from(viewport.height) - 69.) / LINE).clamp(2., 100.) as u16,
        ((f32::from(viewport.width) - 24.) / width).clamp(8., 240.) as u16,
    )
}

impl Panel {
    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prepare) = self.prepare.take() else {
            return;
        };
        match pty::Terminal::start(
            self.project.clone(),
            self.command.clone(),
            prepare,
            dimensions(window, cx),
        ) {
            Ok((terminal, screen, updates)) => {
                if crate::platform::layout_probe::active() {
                    // Lets a window test type a password only once echo is off.
                    terminal.observe_echo();
                }
                self.terminal = Some(terminal);
                self.screen = Some(screen.clone());
                self.focus.focus(window, cx);
                let callback = self.on_exit.take();
                cx.spawn(async move |view, cx| {
                    while updates.recv_async().await.is_ok() {
                        let outcome = screen.lock().unwrap().outcome.clone();
                        if let Some(outcome) = outcome {
                            let code = outcome.clone().ok().flatten();
                            let _ = view.update(cx, |s, cx| {
                                s.terminal = None;
                                s.status = Some(match outcome {
                                    Ok(Some(c)) => format!("Exited with status {c}"),
                                    Ok(None) => "Terminal closed".into(),
                                    Err(e) => e,
                                });
                                cx.notify();
                            });
                            if let Some(callback) = callback {
                                cx.update(|cx| callback(code, cx));
                            }
                            break;
                        }
                        let _ = view.update(cx, |_, cx| cx.notify());
                        cx.background_executor()
                            .timer(Duration::from_millis(16))
                            .await;
                    }
                })
                .detach();
            }
            Err(e) => {
                self.status = Some(e.to_string());
                if let Some(callback) = self.on_exit.take() {
                    callback(None, cx);
                }
            }
        }
        cx.notify();
    }

    fn key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(tty), Some(screen)) = (&self.terminal, &self.screen) else {
            return;
        };
        let key = &event.keystroke;
        let m = key.modifiers;
        let mut locked = screen.lock().unwrap();
        let screen = locked.parser.screen_mut();
        let paste = key.key == "v" && (m.platform || (m.control && m.shift));
        let bytes = if paste {
            cx.read_from_clipboard().and_then(|c| c.text()).map(|s| {
                let clean = s.replace('\x1b', "");
                if screen.bracketed_paste() {
                    format!("\x1b[200~{clean}\x1b[201~").into_bytes()
                } else {
                    clean.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
                }
            })
        } else {
            key_bytes(key, screen.application_cursor())
        };
        let Some(bytes) = bytes else {
            return;
        };
        screen.set_scrollback(0);
        drop(locked);
        if !tty.send(bytes) {
            self.status = Some("Terminal input is full. Try again.".into());
            cx.notify();
        }
        cx.stop_propagation();
    }
}

fn key_bytes(k: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = k.modifiers;
    if m.platform {
        return None;
    }
    if m.control {
        let ch = match k.key.as_str() {
            "space" => b' ',
            s if s.len() == 1 => s.as_bytes()[0].to_ascii_uppercase(),
            _ => return None,
        };
        return Some(vec![match ch {
            b' ' | b'@'..=b'_' => ch & 31,
            b'?' => 127,
            _ => return None,
        }]);
    }
    let seq = match k.key.as_str() {
        "up" | "down" | "right" | "left" | "home" | "end" => {
            let c = match k.key.as_str() {
                "up" => "A",
                "down" => "B",
                "right" => "C",
                "left" => "D",
                "home" => "H",
                _ => "F",
            };
            return Some(format!("\x1b{}{c}", if app_cursor { "O" } else { "[" }).into_bytes());
        }
        "enter" | "return" => "\r",
        "tab" if m.shift => "\x1b[Z",
        "tab" => "\t",
        "escape" => "\x1b",
        "backspace" => "\x7f",
        "delete" => "\x1b[3~",
        "insert" => "\x1b[2~",
        "pageup" => "\x1b[5~",
        "pagedown" => "\x1b[6~",
        "f1" => "\x1bOP",
        "f2" => "\x1bOQ",
        "f3" => "\x1bOR",
        "f4" => "\x1bOS",
        "f5" => "\x1b[15~",
        "f6" => "\x1b[17~",
        "f7" => "\x1b[18~",
        "f8" => "\x1b[19~",
        "f9" => "\x1b[20~",
        "f10" => "\x1b[21~",
        "f11" => "\x1b[23~",
        "f12" => "\x1b[24~",
        "space" => " ",
        _ => k.key_char.as_deref()?,
    };
    let mut bytes = Vec::new();
    if m.alt {
        bytes.push(27);
    }
    bytes.extend_from_slice(seq.as_bytes());
    Some(bytes)
}

impl Render for Panel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::platform::layout_probe::begin_frame(window, "OX Terminal", || {
            serde_json::json!({
                "reviewing": self.screen.is_none(),
                "running": self.terminal.is_some(),
                // The command ran and ended (or failed to start) and Close is shown.
                "exited": self.screen.is_some() && self.terminal.is_none(),
                "status": self.status,
                "echo": self.screen.as_ref().and_then(|s| s.lock().unwrap().echo),
            })
        });
        let p = Palette::get(cx);
        let mut body = col()
            .id("terminal-panel")
            .size_full()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .overflow_hidden();
        if let Some(screen) = &self.screen {
            if let Some(tty) = &self.terminal {
                tty.resize(dimensions(window, cx));
            }
            let screen = screen.lock().unwrap();
            let lines = display::rows(screen.parser.screen(), p);
            let status = self
                .status
                .clone()
                .unwrap_or_else(|| self.project.display().to_string());
            let header = row()
                .h(px(44.))
                .flex_shrink_0()
                .px_3()
                .gap_2()
                .bg(rgb(p.nav))
                .child(div().flex_1().truncate().child(status))
                .child(button(
                    p,
                    "terminal-close",
                    "Close",
                    false,
                    true,
                    |_, w, _| w.remove_window(),
                ));
            body = body.child(header).child(
                col()
                    .id("terminal-grid")
                    .track_focus(&self.focus)
                    .p(px(12.))
                    .flex_1()
                    .overflow_hidden()
                    .font_family(FONT)
                    .text_size(p.text(Step::Ui))
                    .line_height(px(LINE))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|s, _, w, cx| s.focus.focus(w, cx)),
                    )
                    .on_scroll(cx.listener(|s, e: &ScrollEvent, _, cx| {
                        if let Some(screen) = &s.screen {
                            let mut guard = screen.lock().unwrap();
                            let grid = guard.parser.screen_mut();
                            let delta = f32::from(e.delta.pixel_delta(px(LINE)).y) / LINE;
                            if delta.abs() > 0.01 {
                                grid.set_scrollback(
                                    grid.scrollback()
                                        .saturating_add_signed(delta.round() as isize),
                                );
                                cx.notify();
                            }
                        }
                    }))
                    .capture_key_down(cx.listener(Self::key))
                    .children(lines.into_iter().map(|line| {
                        div()
                            .flex_shrink_0()
                            .h(px(LINE))
                            .whitespace_nowrap()
                            .child(line)
                    })),
            );
        } else {
            let preview = self.preview.clone();
            body = body
                .overflow_y_scroll()
                .p_4()
                .gap_3()
                .track_focus(&self.focus)
                .child(div().text_size(p.text(Step::Title)).child(if self.shell {
                    "Open a local shell"
                } else {
                    "Review command"
                }))
                .child(small(p, self.project.display().to_string()))
                .child(
                    div()
                        .bg(rgb(p.panel))
                        .p_3()
                        .font_family(FONT)
                        .child(self.preview.clone()),
                )
                .children(self.status.clone())
                .child(
                    row()
                        .gap_2()
                        .child(button(
                            p,
                            "terminal-run",
                            if self.shell { "Open shell" } else { "Run" },
                            true,
                            self.prepare.is_some(),
                            cx.listener(|s, _, w, cx| s.run(w, cx)),
                        ))
                        .child(button(
                            p,
                            "terminal-copy",
                            "Copy command",
                            false,
                            true,
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(preview.clone()))
                            },
                        ))
                        .child(button(
                            p,
                            "terminal-cancel",
                            "Cancel",
                            false,
                            true,
                            |_, w, _| w.remove_window(),
                        )),
                );
        }
        body
    }
}
