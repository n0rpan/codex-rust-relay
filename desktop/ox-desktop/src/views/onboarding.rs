//! First-run actions are scoped to the project and route of the visible pane.
use super::workbench::Workbench;
use crate::{activity, appearance::Palette, col, kit::button, platform, small};
use gpui::*;

pub(super) struct ConfigureProvider;
impl EventEmitter<ConfigureProvider> for Workbench {}

pub(super) enum Issue {
    Missing,
    Old(String),
    SignIn,
    Provider,
}
impl Workbench {
    pub(super) fn first_run_issue(&self) -> Option<Issue> {
        if self.connecting {
            return None;
        }
        let state = self.store.as_ref().map(|s| s.state());
        if matches!(
            state,
            Some(
                ox_desk_core::store::Connection::Ready
                    | ox_desk_core::store::Connection::Connecting
            )
        ) {
            return None;
        }
        if let Some(ox_desk_core::store::Connection::Closed(e)) = state
            && e.starts_with(self.route.runtime.name())
            && e.contains("older than the required")
        {
            return Some(Issue::Old(e.clone()));
        }
        if let Some(e) = &self.launch_error {
            if e.starts_with(self.route.runtime.name()) && e.contains("older than the required") {
                return Some(Issue::Old(e.clone()));
            }
            if e.starts_with("Configure an OX API provider") {
                return Some(Issue::Provider);
            }
        }
        if platform::onboarding::missing(self.route.runtime) {
            return Some(Issue::Missing);
        }
        matches!(state, Some(ox_desk_core::store::Connection::SignInRequired))
            .then_some(Issue::SignIn)
    }
    pub(super) fn onboarding(&self, issue: Issue, cx: &Context<Self>) -> Div {
        let p = Palette::get(cx);
        let runtime = self.route.runtime;
        let name = runtime.name();
        let (title, note) = match &issue {
            Issue::Missing => (
                format!("Install {name}"),
                format!("Install the stock {name} executable on this computer to open a session."),
            ),
            Issue::Old(v) => (format!("Update {name}"), v.clone()),
            Issue::Provider => (
                "Configure an API provider".into(),
                "This project’s saved API route needs an OX provider connection.".into(),
            ),
            Issue::SignIn => (
                "Connect your account".into(),
                format!(
                    "Sign in with {name} to use your {} subscription.",
                    runtime.account()
                ),
            ),
        };
        let mut panel = col()
            .w_full()
            .max_w(px(470.))
            .gap_3()
            .child(activity::ox(p, 0, 3.))
            .child(div().text_2xl().child(title))
            .child(small(p, note));
        match issue {
            Issue::Missing | Issue::Old(_) => {
                if let Some((label, args)) =
                    platform::onboarding::installer(runtime, matches!(issue, Issue::Old(_)))
                {
                    panel=panel.child(button(p,"install-codex",label,true,true,cx.listener(move|s,_,_,cx|{
                        match platform::onboarding::repair(&s.project,&args){
                            Ok(())=>{s.awaiting_login=true;s.launch_error=Some("Finish installing Codex in the terminal, then return here.".into());}
                            Err(e)=>s.launch_error=Some(e),
                        }
                        cx.notify()
                    })));
                } else {
                    panel = panel.child(button(
                        p,
                        "codex-guide",
                        format!("Open {name} installation guide ↗"),
                        true,
                        true,
                        cx.listener(move |s, _, _, cx| {
                            s.awaiting_login = true;
                            cx.open_url(runtime.install_guide())
                        }),
                    ));
                }
            }
            Issue::Provider => {
                panel = panel.child(button(
                    p,
                    "setup-provider",
                    "Configure an API provider",
                    true,
                    true,
                    cx.listener(|_, _, _, cx| cx.emit(ConfigureProvider)),
                ))
            }
            Issue::SignIn => {
                panel = panel
                    .child(button(
                        p,
                        "native-sign-in",
                        format!("Sign in with {} ↗", runtime.account()),
                        true,
                        true,
                        cx.listener(|s, _, _, cx| s.start_sign_in(cx)),
                    ))
                    .child(button(
                        p,
                        "setup-provider",
                        "Configure an API provider",
                        false,
                        true,
                        cx.listener(|_, _, _, cx| cx.emit(ConfigureProvider)),
                    ));
            }
        }
        if self.awaiting_login {
            panel = panel.child(button(
                p,
                "retry-connection",
                "Check connection",
                false,
                true,
                cx.listener(|s, _, w, cx| s.reconnect(w, cx)),
            ));
        }
        if let Some(message) = self.launch_error.as_ref().filter(|s| {
            !s.contains("older than the required") && !s.starts_with("Configure an OX API provider")
        }) {
            panel = panel.child(small(p, message.clone()));
        }
        col()
            .size_full()
            .items_center()
            .justify_center()
            .px_6()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .child(panel)
    }
    fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        self.awaiting_login = true;
        let runtime = self.route.runtime;
        let login = cx.background_spawn(async move { platform::launch_login(runtime, &project) });
        cx.spawn(async move |view, cx| {
            let result = login.await;
            let _ = view.update(cx, |s, cx| {
                if s.awaiting_login {
                    s.launch_error = Some(match result {
                        Ok(()) => {
                            format!(
                                "Finish signing in with {}, then return to this window.",
                                runtime.name()
                            )
                        }
                        Err(error) => {
                            s.awaiting_login = false;
                            error
                        }
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
