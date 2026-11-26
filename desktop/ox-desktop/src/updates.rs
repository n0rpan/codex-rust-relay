//! One background check per application. The CLI owns package verification and activation.
use crate::{appearance::Palette, kit, platform, version::VERSION};
use gpui::{prelude::*, *};
use ox_host_service::managed_install::package_store;
use platform::versions::{self, Candidate};
use std::{path::PathBuf, time::Duration};

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Checking,
    Current,
    Ready(Candidate),
    WaitingForIdle(Candidate, Option<String>),
    IdleCheckFailed(Candidate, String),
    Applying {
        version: String,
        idle: bool,
    },
    Failed(String),
    RestartFailed(Candidate, String),
}

#[derive(Default)]
pub struct Updates {
    store: Option<PathBuf>,
    state: State,
    dismissed: Option<String>,
    manual: bool,
    idle_wait: Option<Task<()>>,
}
impl Global for Updates {}

pub fn start(smoke: bool, cx: &mut App) {
    let store = std::env::current_exe().ok().and_then(|p| package_store(&p));
    let automatic =
        store.is_some() && !smoke && std::env::var_os("OX_NO_BACKGROUND_UPDATE").is_none();
    cx.set_global(Updates {
        store,
        ..Default::default()
    });
    if automatic {
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            loop {
                cx.update(|cx| check(false, cx));
                cx.background_executor()
                    .timer(Duration::from_secs(1800))
                    .await;
            }
        })
        .detach();
    }
}

pub fn managed(cx: &App) -> bool {
    cx.try_global::<Updates>()
        .is_some_and(|u| u.store.is_some())
}

fn check(manual: bool, cx: &mut App) {
    if !manual
        && !cx
            .global::<crate::preferences::behavior::Active>()
            .background_updates
    {
        return;
    }
    let u = cx.global_mut::<Updates>();
    if matches!(
        u.state,
        State::Checking | State::WaitingForIdle(..) | State::Applying { .. }
    ) {
        return;
    }
    if manual {
        u.dismissed = None;
    } else if matches!(u.state, State::Ready(_) | State::RestartFailed(_, _)) {
        return;
    }
    u.manual = manual;
    u.state = State::Checking;
    cx.spawn(async move |cx| {
        let result = cx
            .background_spawn(async move { versions::stage(!manual) })
            .await;
        cx.update(|cx| {
            cx.global_mut::<Updates>().state = match result {
                Ok(Some(candidate)) => State::Ready(candidate),
                Ok(None) => State::Current,
                Err(e) => State::Failed(e),
            };
        });
    })
    .detach();
}

pub fn can_schedule(cx: &App) -> bool {
    cx.try_global::<Updates>()
        .is_some_and(|u| matches!(u.state, State::Ready(_) | State::IdleCheckFailed(..)))
}

pub fn schedule_idle(cx: &mut App) {
    let candidate = match &cx.global::<Updates>().state {
        State::Ready(c) | State::IdleCheckFailed(c, _) => c.clone(),
        _ => return,
    };
    let u = cx.global_mut::<Updates>();
    let Some(store) = u.store.clone() else { return };
    u.state = State::WaitingForIdle(candidate.clone(), None);
    u.dismissed = None;
    u.manual = true;
    let task = cx.spawn(async move |cx| {
        let interval = Duration::from_secs(3);
        let mut delay = interval;
        loop {
            cx.background_executor().timer(delay).await;
            let path = store.clone();
            match cx
                .background_spawn(async move { platform::reserve_idle_update(&path) })
                .await
            {
                Ok(None) => {
                    if delay != interval {
                        cx.update(|cx| {
                            cx.global_mut::<Updates>().state =
                                State::WaitingForIdle(candidate.clone(), None)
                        });
                    }
                    delay = interval;
                    continue;
                }
                Ok(Some(reservation)) => cx.update(|cx| apply(candidate, Some(reservation), cx)),
                Err(e)
                    if !matches!(
                        e.kind(),
                        std::io::ErrorKind::Unsupported
                            | std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::InvalidData
                    ) =>
                {
                    cx.update(|cx| {
                        cx.global_mut::<Updates>().state =
                            State::WaitingForIdle(candidate.clone(), Some(e.to_string()))
                    });
                    delay = (delay * 2).min(Duration::from_secs(60));
                    continue;
                }
                Err(e) => cx.update(|cx| {
                    cx.global_mut::<Updates>().state =
                        State::IdleCheckFailed(candidate, e.to_string());
                }),
            }
            break;
        }
    });
    cx.global_mut::<Updates>().idle_wait = Some(task);
}

fn cancel_idle(cx: &mut App) {
    let u = cx.global_mut::<Updates>();
    if let State::WaitingForIdle(c, _) = &u.state {
        u.state = State::Ready(c.clone());
        u.idle_wait = None;
    }
}

fn apply(candidate: Candidate, reservation: Option<platform::IdleReservation>, cx: &mut App) {
    let u = cx.global_mut::<Updates>();
    if !matches!(
        u.state,
        State::Ready(_)
            | State::RestartFailed(_, _)
            | State::WaitingForIdle(..)
            | State::IdleCheckFailed(..)
    ) {
        return;
    }
    let Some(store) = u.store.clone() else { return };
    u.state = State::Applying {
        version: candidate.version.clone(),
        idle: reservation.is_some(),
    };
    u.manual = true;
    cx.spawn(async move |cx| {
        let request = candidate.clone();
        let result = cx
            .background_spawn(async move {
                match reservation {
                    Some(socket) => versions::apply_if_idle(&request, socket),
                    None => versions::apply(&request).map(|_| true),
                }
            })
            .await;
        cx.update(|cx| match result {
            Ok(false) => {
                cx.global_mut::<Updates>().state = State::Ready(candidate);
                schedule_idle(cx);
            }
            Ok(true) => {
                #[cfg(target_os = "macos")]
                let path = store.join("current/OX Desktop.app");
                #[cfg(not(target_os = "macos"))]
                let path = ox_host_service::managed_install::active_binary(&store, "ox-desktop");
                cx.set_restart_path(path);
                cx.restart();
            }
            Err(e) => {
                let active = store.join("current").canonicalize().ok();
                let installed = candidate.digest == "current"
                    || active
                        .is_some_and(|p| p == store.join(format!("release-{}", candidate.digest)));
                cx.global_mut::<Updates>().state = if installed {
                    State::RestartFailed(candidate, e)
                } else {
                    State::Failed(e)
                };
            }
        });
    })
    .detach();
}

pub fn action_label(cx: &App) -> (&'static str, bool) {
    match &cx.global::<Updates>().state {
        State::Checking => ("Checking…", false),
        State::WaitingForIdle(..) => ("Cancel scheduled restart", true),
        State::Applying { .. } => ("Restarting…", false),
        State::Ready(_) | State::IdleCheckFailed(..) => ("Restart to update", true),
        State::RestartFailed(_, _) => ("Retry restart", true),
        State::Failed(_) => ("Retry check", true),
        _ => ("Check for updates", true),
    }
}

pub fn perform_action(cx: &mut App) {
    match &cx.global::<Updates>().state {
        State::Ready(c) | State::RestartFailed(c, _) | State::IdleCheckFailed(c, _) => {
            apply(c.clone(), None, cx)
        }
        State::WaitingForIdle(..) => cancel_idle(cx),
        _ => check(true, cx),
    }
}

pub fn about(cx: &App) -> String {
    match &cx.global::<Updates>().state {
        State::Idle => String::new(),
        State::Checking => "Waiting for any earlier update, then checking for a release…".into(),
        State::Current => format!("No newer release is available for OX {VERSION}."),
        State::Ready(c) if c.digest == "current" => format!(
            "OX {} is installed. Reopen this window on the new version.",
            c.version
        ),
        State::Ready(c) => format!(
            "OX {} is downloaded and verified. Restart when you’re ready.",
            c.version
        ),
        State::WaitingForIdle(_, Some(error)) => {
            format!("Restart is still scheduled. OX will retry the idle check. {error}")
        }
        State::WaitingForIdle(c, None) => format!(
            "OX will restart to update to {} when all agents on this host are idle.",
            c.version
        ),
        State::IdleCheckFailed(_, e) => e.clone(),
        State::Applying {
            version,
            idle: true,
        } => format!("Agents are idle. Restarting OX to update to {version}…"),
        State::Applying {
            version,
            idle: false,
        } => format!(
            "Updating to OX {version}. Any earlier update finishes first. Active turns have up to 10 minutes to finish before OX reopens."
        ),
        State::RestartFailed(_, e) | State::Failed(e) => e.clone(),
    }
}

pub fn banner(p: Palette, cx: &App) -> Option<AnyElement> {
    let u = cx.try_global::<Updates>()?;
    let bar = kit::row()
        .flex_wrap()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .bg(rgb(p.panel))
        .text_color(rgb(p.fg));
    Some(
        match &u.state {
            State::Ready(c) | State::IdleCheckFailed(c, _) if u.dismissed.as_deref() != Some(&c.digest) => {
                let c = c.clone();
                let digest = c.digest.clone();
                let message = if let State::IdleCheckFailed(_, e) = &u.state {
                    e.clone()
                } else if c.digest == "current" {
                    format!("OX {} is installed. Restart to use it in this window.", c.version)
                } else {
                    format!("OX {} is ready. Restart Desktop and the session host to update. Active turns have up to 10 minutes to finish.", c.version)
                };
                bar.child(message)
                .child(kit::button(
                    p,
                    "ox-update-restart",
                    "Restart to update",
                    true,
                    true,
                    move |_, _, cx| apply(c.clone(), None, cx),
                ))
                .child(kit::button(p, "ox-update-when-idle", "Restart when idle", false, true, |_, _, cx| schedule_idle(cx)))
                .child(kit::button(
                    p,
                    "ox-update-later",
                    "Later",
                    false,
                    true,
                    move |_, _, cx| {
                        cx.global_mut::<Updates>().dismissed = Some(digest.clone());
                    },
                ))
            }
            State::WaitingForIdle(..) => bar.child(about(cx)).child(kit::button(p, "ox-update-cancel-idle", "Cancel scheduled restart", false, true, |_, _, cx| cancel_idle(cx))),
            State::Applying { .. } => bar.child(about(cx)),
            State::Failed(e) | State::RestartFailed(_, e) if u.manual => bar
                .child(e.clone())
                .child(kit::button(
                    p,
                    "ox-update-retry",
                    action_label(cx).0,
                    false,
                    true,
                    |_, _, cx| perform_action(cx),
                ))
                .child(kit::button(
                    p,
                    "ox-update-dismiss",
                    "Dismiss",
                    false,
                    true,
                    |_, _, cx| cx.global_mut::<Updates>().manual = false,
                )),
            _ => return None,
        }
        .into_any_element(),
    )
}
