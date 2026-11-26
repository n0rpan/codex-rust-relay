use crate::{
    appearance::{Palette, Step},
    kit::{button, field},
    platform::hosts::{self, KeyReview, SavedHost},
};
use gpui::{prelude::*, *};
use gpui_component::input::{Input, InputState};
use ox_host_web::host_updates::{Channel, Command, Mode, Status};
use std::io;

pub(crate) struct Hosts {
    address: Entity<InputState>,
    code: Entity<InputState>,
    hosts: Vec<SavedHost>,
    selected: Option<SavedHost>,
    status: Option<Status>,
    review: Option<KeyReview>,
    busy: bool,
    notice: String,
}
impl Hosts {
    pub fn new(w: &mut Window, cx: &mut Context<Self>) -> Self {
        let loaded = hosts::list();
        Self {
            address: field(
                "",
                "Invitation link or https://host.tailnet.ts.net:8443",
                false,
                w,
                cx,
            ),
            code: field("", "Pairing code (when entering a URL)", false, w, cx),
            notice: loaded
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default(),
            hosts: loaded.unwrap_or_default(),
            selected: None,
            status: None,
            review: None,
            busy: false,
        }
    }
    fn work<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        job: impl FnOnce() -> io::Result<T> + Send + 'static,
        apply: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.notice.clear();
        let job = cx.background_spawn(async move { job() });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |s, cx| {
                s.busy = false;
                match result {
                    Ok(value) => apply(s, value, cx),
                    Err(e) => s.notice = e.to_string(),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn select(&mut self, host: Option<SavedHost>, cx: &mut Context<Self>) {
        self.selected = host;
        self.status = None;
        self.review = None;
        self.update(Command::Status, cx);
    }
    fn update(&mut self, command: Command, cx: &mut Context<Self>) {
        let host = self.selected.clone();
        self.work(
            cx,
            move || match host {
                Some(host) => host.updates(command),
                None => {
                    let paths = ox_host_service::Paths::discover()?;
                    ox_host_web::access::status(&paths).map_err(io::Error::other)?;
                    ox_host_web::admin::request(
                        &paths.state_dir,
                        &ox_host_web::admin::Request::Update { command },
                    )?
                    .update
                    .ok_or_else(|| {
                        io::Error::other("The host needs an update to manage its settings here")
                    })
                }
            },
            |s, status, _| s.status = Some(status),
        )
    }
    fn remove(&mut self, host: SavedHost, revoke: bool, cx: &mut Context<Self>) {
        self.work(
            cx,
            move || {
                if revoke {
                    host.revoke()?;
                } else {
                    host.forget()?;
                }
                hosts::list()
            },
            move |s, rows, cx| {
                s.hosts = rows;
                s.selected = None;
                s.review = None;
                s.status = None;
                cx.set_global(HostDirectory(s.hosts.clone()));
                s.notice = if revoke {
                    "This desktop has been revoked."
                } else {
                    "Host forgotten on this desktop."
                }
                .into();
            },
        );
    }
    fn add(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let address = self.address.read(cx).value().to_string();
        let code = self.code.read(cx).value().to_string();
        let task = cx.background_spawn(async move {
            let label = hosts::device_label();
            hosts::pair(&address, &code, &label)
        });
        self.busy = true;
        self.notice.clear();
        cx.notify();
        cx.spawn_in(w, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |s, w, cx| {
                s.busy = false;
                match result {
                    Ok(host) => {
                        s.address.update(cx, |v, cx| v.set_value("", w, cx));
                        s.code.update(cx, |v, cx| v.set_value("", w, cx));
                        s.hosts
                            .retain(|h| h.id != host.id && h.origin != host.origin);
                        s.hosts.push(host.clone());
                        cx.set_global(HostDirectory(s.hosts.clone()));
                        s.select(Some(host), cx);
                    }
                    Err(e) => s.notice = e.to_string(),
                }
                cx.notify()
            });
        })
        .detach()
    }
}
#[derive(Default)]
pub(crate) struct HostDirectory(pub Vec<SavedHost>);
impl Global for HostDirectory {}
impl Render for Hosts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let mut list = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_size(p.text(Step::Title)).child("Hosts"));
        if !cx.global::<crate::app::ClientOnly>().0 {
            list = list.child(button(
                p,
                "local-host",
                "This computer",
                self.selected.is_none(),
                !self.busy,
                cx.listener(|s, _, _, cx| s.select(None, cx)),
            ));
        }
        for host in &self.hosts {
            let host = host.clone();
            list = list.child(button(
                p,
                format!("host-{}", host.id),
                host.name.clone(),
                self.selected.as_ref().is_some_and(|h| h.id == host.id),
                !self.busy,
                cx.listener(move |s, _, _, cx| s.select(Some(host.clone()), cx)),
            ));
        }
        if let Some(host) = self.selected.clone() {
            let revoke = host.clone();
            let forget = host.clone();
            list = list.child(host.origin.clone()).child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(button(
                        p,
                        "review-keys",
                        "Send API keys…",
                        false,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| {
                            let host = host.clone();
                            s.work(
                                cx,
                                move || KeyReview::prepare(host),
                                |s, r, _| s.review = Some(r),
                            );
                        }),
                    ))
                    .child(button(
                        p,
                        "revoke-desktop",
                        "Revoke this desktop",
                        false,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| {
                            s.remove(revoke.clone(), true, cx);
                        }),
                    ))
                    .child(button(
                        p,
                        "forget-host",
                        "Forget on this desktop",
                        false,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| s.remove(forget.clone(), false, cx)),
                    )),
            );
        }
        if let Some(review) = &self.review {
            list=list.child(div().text_color(rgb(p.warning)).child(format!("Send selected API keys to {} ({}). Anyone with access to that host’s OX key store can use the keys and incur charges. Revoking this desktop later leaves the copies on the host.",review.host.name,review.host.origin)));
            for (i, row) in review.rows.iter().enumerate() {
                let key = &row.key;
                let status = &row.status;
                let label = if key.scope == "https://api.search.brave.com/" {
                    format!("Brave Search ({})", key.name)
                } else {
                    key.name.clone()
                };
                list = list.child(button(
                    p,
                    ("transfer-key", i),
                    format!(
                        "{} {label} · {} · {status}",
                        if row.selected { "☑" } else { "☐" },
                        key.scope
                    ),
                    false,
                    !self.busy && row.available,
                    cx.listener(move |s, _, _, cx| {
                        if let Some(r) = &mut s.review {
                            r.rows[i].selected = !r.rows[i].selected;
                        }
                        cx.notify();
                    }),
                ));
            }
            let count = review.rows.iter().filter(|row| row.selected).count();
            if review.rows.is_empty() {
                list = list.child("This computer has no configured API keys to send.");
            }
            list = list.child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        p,
                        "send-keys",
                        format!("Send {count} selected"),
                        true,
                        count > 0 && !self.busy,
                        cx.listener(|s, _, _, cx| {
                            if let Some(r) = s.review.take() {
                                s.work(
                                    cx,
                                    move || r.send(),
                                    |s, count, _| s.notice = format!("Sent {count} API keys."),
                                );
                            }
                        }),
                    ))
                    .child(button(
                        p,
                        "cancel-keys",
                        "Cancel",
                        false,
                        !self.busy,
                        cx.listener(|s, _, _, cx| {
                            s.review = None;
                            cx.notify()
                        }),
                    )),
            );
        }
        if let Some(status) = &self.status {
            list = list.child(format!("OX {} · {}", status.installed, status.message));
            if status.managed {
                let mut controls = div().flex().gap_2().flex_wrap();
                for (label, command, selected) in [
                    (
                        "Automatic when idle",
                        Command::SetMode {
                            mode: Mode::Automatic,
                        },
                        status.mode == Mode::Automatic,
                    ),
                    (
                        "Download only",
                        Command::SetMode {
                            mode: Mode::Download,
                        },
                        status.mode == Mode::Download,
                    ),
                    (
                        "Manual",
                        Command::SetMode { mode: Mode::Manual },
                        status.mode == Mode::Manual,
                    ),
                    (
                        "Stable",
                        Command::SetChannel {
                            channel: Channel::Stable,
                        },
                        status.channel == Channel::Stable,
                    ),
                    (
                        "Dev",
                        Command::SetChannel {
                            channel: Channel::Dev,
                        },
                        status.channel == Channel::Dev,
                    ),
                    ("Update when idle", Command::UpdateWhenIdle, false),
                    ("Refresh", Command::Status, false),
                ] {
                    controls = controls.child(button(
                        p,
                        label,
                        label,
                        selected,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| s.update(command, cx)),
                    ));
                }
                if status.scheduled {
                    controls = controls.child(button(
                        p,
                        "cancel-host-update",
                        "Cancel scheduled update",
                        false,
                        !self.busy,
                        cx.listener(|s, _, _, cx| s.update(Command::Cancel, cx)),
                    ));
                }
                list = list.child(controls);
            }
        }
        list = list
            .child(div().text_size(p.text(Step::Title)).child("Add host"))
            .child("Run ox host pair on the other machine. Add --terminal to allow shells and subscription sign-in from this desktop.")
            .child(Input::new(&self.address))
            .child(Input::new(&self.code))
            .child(button(
                p,
                "add-host",
                "Pair host",
                true,
                !self.busy,
                cx.listener(|s, _, w, cx| s.add(w, cx)),
            ));
        if self.busy {
            list = list.child("Contacting host…");
        }
        if !self.notice.is_empty() {
            list = list.child(self.notice.clone());
        }
        list
    }
}
