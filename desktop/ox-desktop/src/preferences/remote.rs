use crate::appearance::Step;
use crate::{
    appearance::Palette,
    kit::{button, field, input},
    platform::remote::{self, Operation, Report},
};
use gpui::{prelude::*, *};
use gpui_component::input::InputState;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) struct Remote {
    project: PathBuf,
    report: Option<Report>,
    message: Option<String>,
    busy: bool,
    /// Completed status operations; lets a window test wait for the one it started.
    finished: u64,
    qr: Option<Arc<Image>>,
    expiry: Option<Task<()>>,
    /// Device whose terminal grant awaits a second click.
    confirm_grant: Option<String>,
    port_field: Option<Entity<InputState>>,
}
impl Remote {
    pub fn new(project: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            project,
            report: None,
            message: None,
            busy: false,
            finished: 0,
            qr: None,
            expiry: None,
            confirm_grant: None,
            port_field: None,
        };
        view.submit(Operation::Status, cx);
        view
    }
    fn enable(&mut self, cx: &mut Context<Self>) {
        if let Some(field) = &self.port_field {
            let text = field.read(cx).value();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                let Ok(port) = trimmed.parse::<u16>() else {
                    self.message = Some("Enter a Tailscale HTTPS port from 1024 to 65535.".into());
                    cx.notify();
                    return;
                };
                if !ox_host_web::serve::valid_https_port(port) {
                    self.message = Some("Enter a Tailscale HTTPS port from 1024 to 65535.".into());
                    cx.notify();
                    return;
                }
                self.submit(
                    Operation::EnableOn {
                        https_port: Some(port),
                        loopback_port: None,
                    },
                    cx,
                );
                return;
            }
        }
        self.submit(Operation::Enable, cx);
    }
    fn submit(&mut self, op: Operation, cx: &mut Context<Self>) {
        self.perform(move || remote::run(op), cx);
    }
    fn perform(
        &mut self,
        work: impl FnOnce() -> Result<Report, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.message = None;
        self.qr = None;
        self.expiry = None;
        if let Some(r) = &mut self.report {
            r.access.host.pairing = None;
        }
        let result = cx.background_spawn(async move { work() });
        cx.spawn(async move |this, cx| {
            let result = result.await;
            let _ = this.update(cx, |s, cx| {
                s.busy = false;
                s.finished += 1;
                match result {
                    Ok(mut r) => {
                        s.message = r.access.notice.take();
                        if let Some(pair) = &r.access.host.pairing {
                            s.qr = Some(Arc::new(Image::from_bytes(
                                ImageFormat::Svg,
                                pair.qr_svg.as_bytes().to_vec(),
                            )));
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let duration = Duration::from_secs(
                                pair.expires_at.saturating_sub(now).max(0) as u64,
                            );
                            s.expiry = Some(cx.spawn(async move |this, cx| {
                                cx.background_executor().timer(duration).await;
                                let _ = this.update(cx, |s, cx| {
                                    s.qr = None;
                                    if let Some(r) = &mut s.report {
                                        r.access.host.pairing = None;
                                    }
                                    cx.notify();
                                });
                            }));
                        }
                        s.report = Some(r);
                    }
                    Err(e) => s.message = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn terminal(&mut self, help: remote::PermissionHelp, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let owner = cx.entity();
        let lease = Arc::new(Mutex::new(None));
        let checked = lease.clone();
        let before_run = help.clone();
        let result = crate::terminal::open_checked(
            self.project.clone(),
            crate::terminal::TerminalCommand {
                program: "/usr/bin/sudo".into(),
                args: help.args(),
            },
            move || {
                *checked.lock().unwrap() = Some(before_run.prepare()?);
                Ok(())
            },
            move |status, app| {
                let lease = lease.lock().unwrap().take();
                owner.update(app, |s, cx| {
                    s.busy = false;
                    s.perform(move || help.finish(status, lease).map(Report::from), cx);
                });
            },
            cx,
        );
        if let Err(e) = result {
            self.busy = false;
            self.message = Some(e);
        }
        cx.notify();
    }
    /// What Settings → Remote access shows, for the desk stats probe.
    pub fn probe_state(&self) -> crate::platform::layout_probe::RemoteProbe {
        let report = self.report.as_ref();
        let permission = report.and_then(|r| r.access.denied.as_ref());
        crate::platform::layout_probe::RemoteProbe {
            checking: self.busy,
            finished: self.finished,
            notice: self.message.clone().or(permission.map(|p| p.error.clone())),
            permission: permission.map(|p| p.suggested()),
            enabled: report.is_some_and(|r| r.access.host.enabled),
            origin: report.and_then(|r| r.access.host.origin.clone()),
        }
    }
    fn action(
        &self,
        p: Palette,
        id: &'static str,
        title: &'static str,
        op: Operation,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        button(
            p,
            id,
            title,
            false,
            !self.busy,
            cx.listener(move |s, _, _, cx| s.submit(op.clone(), cx)),
        )
    }
}
impl Render for Remote {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let mut view = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_size(p.text(Step::Title)).child("Remote access"));
        let permission = self.report.as_ref().and_then(|r| r.access.denied.as_ref());
        if let Some(msg) = self
            .message
            .as_ref()
            .or_else(|| permission.map(|p| &p.error))
        {
            let text = msg.clone();
            view = view
                .child(
                    div()
                        .text_color(rgb(p.warning))
                        .child(msg.chars().take(600).collect::<String>()),
                )
                .child(button(
                    p,
                    "remote-copy-error",
                    "Copy error",
                    false,
                    true,
                    move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())),
                ));
        }
        if let Some(help) = permission {
            if help.pending_removal() {
                view = view
                    .child("Remote access is still enabled. Complete the command to disable it.");
            }
            let suggestion = help.suggested();
            let help = help.clone();
            view = view.child(div().child(suggestion.clone())).child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(button(
                        p,
                        "remote-copy-command",
                        "Copy command",
                        false,
                        true,
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(suggestion.clone()))
                        },
                    ))
                    .child(button(
                        p,
                        "remote-privileged-terminal",
                        "Run in OX terminal",
                        false,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| s.terminal(help.clone(), cx)),
                    )),
            );
        }
        if self.busy {
            view = view.child("Checking remote access…");
        }
        let mut buttons = div().flex().flex_wrap().gap_2();
        buttons = buttons.child(self.action(p, "remote-refresh", "Refresh", Operation::Status, cx));
        if let Some(report) = &self.report {
            if let Ok(node) = &report.access.network {
                view = match &node.login {
                    Some(login) => view.child(format!("Tailscale account · {login}")),
                    None => view
                        .child(format!("Tagged machine · {}", node.tags.join(", ")))
                        .child(
                            "This machine belongs to a Tailscale tag, not a user. Anyone your tailnet lets reach it can redeem a pairing invitation.",
                        ),
                };
                if let Some(reason) = node.reason() {
                    view = view.child(reason);
                }
                let ready = report.access.host.enabled
                    && node.ready()
                    && report.access.host.origin.as_ref() == Some(&node.origin);
                if ready {
                    view = view.child(node.origin.clone());
                    buttons = buttons.child(self.action(
                        p,
                        "remote-pair",
                        "Pair a device",
                        Operation::Pair {
                            terminal: false,
                            login: None,
                        },
                        cx,
                    ));
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs() as i64);
                    match report
                        .access
                        .host
                        .window
                        .as_ref()
                        .filter(|w| w.expires_at > now)
                    {
                        Some(window) => {
                            let minutes = (window.expires_at - now + 59) / 60;
                            let who = window.login.as_deref().unwrap_or("a device");
                            view = view.child(format!(
                                "Waiting for {who} to pair · {minutes} min left · {} paired. On the other machine, choose this host under Add host.",
                                window.paired
                            ));
                            buttons = buttons.child(self.action(
                                p,
                                "remote-window-close",
                                "Stop waiting",
                                Operation::CloseWindow,
                                cx,
                            ));
                        }
                        None => {
                            view = view.child(
                                "Waiting lets a device pair from its own Add host with no code to carry. It lasts five minutes.",
                            );
                            buttons = buttons.child(self.action(
                                p,
                                "remote-window-open",
                                "Wait for a device to pair",
                                Operation::OpenWindow { login: None },
                                cx,
                            ));
                        }
                    }
                } else {
                    let port_input = self
                        .port_field
                        .get_or_insert_with(|| {
                            let initial =
                                if node.https_port == ox_host_web::access::DEFAULT_HTTPS_PORT {
                                    String::new()
                                } else {
                                    node.https_port.to_string()
                                };
                            let entity = field(&initial, "HTTPS port (default 8443)", false, w, cx);
                            cx.subscribe(
                                &entity,
                                |_, _, _: &gpui_component::input::InputEvent, cx| cx.notify(),
                            )
                            .detach();
                            entity
                        })
                        .clone();
                    let has_override = !port_input.read(cx).value().trim().is_empty();
                    if !node.available() || has_override {
                        view = view.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .text_size(p.text(Step::Ui))
                                        .text_color(rgb(p.muted))
                                        .child("Tailscale HTTPS port"),
                                )
                                .child(div().w(px(220.)).child(input(&port_input))),
                        );
                    }
                    if node.available() || has_override {
                        buttons = buttons.child(button(
                            p,
                            "remote-enable",
                            "Enable Tailscale Serve",
                            false,
                            !self.busy,
                            cx.listener(|s, _, _, cx| s.enable(cx)),
                        ));
                    }
                }
            } else if let Err(msg) = &report.access.network {
                view = view.child(msg.clone());
                let mut setup = div().flex().flex_wrap().gap_2().child(button(
                    p,
                    "tailscale-install",
                    "Open Tailscale downloads",
                    false,
                    true,
                    |_, _, cx| cx.open_url("https://tailscale.com/download"),
                ));
                for runtime in &report.guide {
                    let runtime = *runtime;
                    setup = setup.child(button(
                        p,
                        format!("tailnet-guide-{}", runtime.executable()),
                        format!("Ask {} for setup help", runtime.name()),
                        false,
                        true,
                        cx.listener(move |s, _, _, cx| {
                            s.message = remote::guidance(&s.project, runtime).err();
                            cx.notify();
                        }),
                    ));
                }
                view = view.child(setup);
            }
            if report.access.host.enabled || report.access.network.as_ref().is_ok_and(|n| n.ready())
            {
                buttons = buttons.child(self.action(
                    p,
                    "remote-disable",
                    "Disable remote access",
                    Operation::Disable,
                    cx,
                ));
            }
            if let (Some(image), Some(pair)) = (&self.qr, &report.access.host.pairing) {
                let url = pair.url.clone();
                if let Some(code) = &pair.short_code {
                    view = view.child(format!("Desktop pairing code · {code}"));
                }
                view = view
                    .child(
                        "Scan on your phone or paste the pairing link in Add host on your desktop.",
                    )
                    .child(img(image.clone()).size(px(224.)).bg(white()))
                    .child(button(
                        p,
                        "remote-copy-link",
                        "Copy pairing link",
                        false,
                        true,
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
                        },
                    ));
            }
            if !report.access.host.devices.is_empty() {
                view = view.child(div().text_size(p.text(Step::Title)).child("Paired devices"));
            }
            for device in &report.access.host.devices {
                let id = device.id.clone();
                let grant = id.clone();
                let enable = !device.terminal;
                let confirming =
                    enable && self.confirm_grant.as_deref() == Some(device.id.as_str());
                let ask = device.id.clone();
                let label = if !enable {
                    "Disable terminal access"
                } else if confirming {
                    "Yes, allow commands as me"
                } else {
                    "Allow terminal access"
                };
                let grant_button = if enable {
                    crate::kit::danger_button(
                        p,
                        format!("terminal-grant-{}", device.id),
                        label,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| {
                            if s.confirm_grant.as_deref() == Some(grant.as_str()) {
                                s.confirm_grant = None;
                                s.submit(Operation::TerminalGrant(grant.clone(), true), cx)
                            } else {
                                s.confirm_grant = Some(ask.clone());
                                cx.notify();
                            }
                        }),
                    )
                } else {
                    button(
                        p,
                        format!("terminal-grant-{}", device.id),
                        label,
                        false,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| {
                            s.submit(Operation::TerminalGrant(grant.clone(), false), cx)
                        }),
                    )
                };
                let mut line = div().flex().flex_col().gap_1().child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(format!("{} · {}", device.label, device.login))
                        .child(grant_button)
                        .child(button(
                            p,
                            format!("revoke-{}", device.id),
                            "Revoke",
                            false,
                            !self.busy,
                            cx.listener(move |s, _, _, cx| {
                                s.submit(Operation::Revoke(id.clone()), cx)
                            }),
                        )),
                );
                if enable {
                    line = line.child(
                        div()
                            .text_size(p.text(Step::Label))
                            .text_color(rgb(if confirming { p.error } else { p.muted }))
                            .child(if confirming {
                                "Click again to confirm. This device will run commands as you on this computer."
                            } else {
                                "Lets this device run commands as you on this computer."
                            }),
                    );
                }
                view = view.child(line);
            }
        }
        view.child(buttons)
    }
}
