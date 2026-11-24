use crate::{
    appearance::Palette,
    kit::button,
    platform::remote::{self, Operation, Report},
};
use gpui::{prelude::*, *};
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
    qr: Option<Arc<Image>>,
    expiry: Option<Task<()>>,
}
impl Remote {
    pub fn new(project: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            project,
            report: None,
            message: None,
            busy: false,
            qr: None,
            expiry: None,
        };
        view.submit(Operation::Status, cx);
        view
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
            r.host.pairing = None;
        }
        let result = cx.background_spawn(async move { work() });
        cx.spawn(async move |this, cx| {
            let result = result.await;
            let _ = this.update(cx, |s, cx| {
                s.busy = false;
                match result {
                    Ok(mut r) => {
                        s.message = r.notice.take();
                        if let Some(pair) = &r.host.pairing {
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
                                        r.host.pairing = None;
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
            help.command(),
            move || {
                *checked.lock().unwrap() = Some(before_run.prepare()?);
                Ok(())
            },
            move |status, app| {
                let lease = lease.lock().unwrap().take();
                owner.update(app, |s, cx| {
                    s.busy = false;
                    s.perform(move || help.finish(status, lease), cx);
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let mut view = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_xl().child("Remote access"));
        let permission = self.report.as_ref().and_then(|r| r.denied.as_ref());
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
            if let Ok(node) = &report.network {
                view = view.child(format!("Tailscale account · {}", node.login));
                if let Some(reason) = node.reason() {
                    view = view.child(reason);
                }
                let ready = report.host.enabled
                    && node.ready()
                    && report.host.origin.as_ref() == Some(&node.origin);
                if ready {
                    view = view.child(node.origin.clone());
                    buttons = buttons.child(self.action(
                        p,
                        "remote-pair",
                        "Pair a device",
                        Operation::Pair,
                        cx,
                    ));
                } else if node.available() {
                    buttons = buttons.child(self.action(
                        p,
                        "remote-enable",
                        "Enable Tailscale Serve",
                        Operation::Enable,
                        cx,
                    ));
                }
            } else if let Err(msg) = &report.network {
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
            if report.host.enabled || report.network.as_ref().is_ok_and(|n| n.ready()) {
                buttons = buttons.child(self.action(
                    p,
                    "remote-disable",
                    "Disable remote access",
                    Operation::Disable,
                    cx,
                ));
            }
            if let (Some(image), Some(pair)) = (&self.qr, &report.host.pairing) {
                let url = pair.url.clone();
                view = view
                    .child("Scan with your phone while connected to your tailnet.")
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
            if !report.host.devices.is_empty() {
                view = view.child(div().text_lg().child("Paired devices"));
            }
            for device in &report.host.devices {
                let id = device.id.clone();
                let grant = id.clone();
                let enable = !device.terminal;
                view = view.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(format!("{} · {}", device.label, device.login))
                        .child(button(
                            p,
                            format!("terminal-grant-{}", device.id),
                            if enable {
                                "Allow terminal access"
                            } else {
                                "Disable terminal access"
                            },
                            false,
                            !self.busy,
                            cx.listener(move |s, _, _, cx| {
                                s.submit(Operation::TerminalGrant(grant.clone(), enable), cx)
                            }),
                        ))
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
            }
        }
        view.child(buttons)
    }
}
