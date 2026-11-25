//! The "RUNNING" section pinned under the sidebar: the live processes the session
//! host runs, with per-row End and an "End all idle" action. It polls the host while
//! the sidebar shows it, and again right after every End.
use crate::appearance::Step;
use crate::{
    appearance::Palette,
    kit::{button, col, row, small, toolbar_button},
    platform,
};
use gpui::{prelude::FluentBuilder, *};
use ox_desk_core::host_protocol::{ProcessView, Scope};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const POLL: Duration = Duration::from_secs(5);

/// A compact duration such as `40s`, `12m`, `3h` or `2d`. Negative spans read as zero.
pub(crate) fn span(seconds: i64) -> String {
    match seconds.max(0) {
        s @ 0..60 => format!("{s}s"),
        s @ 60..3600 => format!("{}m", s / 60),
        s @ 3600..86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// The idle line for a row: `busy` while a turn or approval is in progress, otherwise
/// how long since its last activity.
pub(crate) fn idle_label(now: i64, process: &ProcessView) -> String {
    if process.running {
        "busy".into()
    } else {
        format!("idle {}", span(now.saturating_sub(process.last_activity)))
    }
}

/// The rows "End all idle" ends: every process that is not running, in host order.
pub(crate) fn idle_rows(processes: &[ProcessView]) -> Vec<&ProcessView> {
    processes.iter().filter(|p| !p.running).collect()
}

/// The last path component of a project, or the whole path when it has none.
pub(crate) fn project_name(project: &Path) -> String {
    project
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| project.display().to_string())
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "claude" => "Claude",
        "codex" => "Codex",
        "worker" => "Worker",
        _ => "Process",
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub struct Running {
    processes: Vec<ProcessView>,
    loaded: bool,
    loading: bool,
    /// A refresh was requested while one was in flight; run it when that one lands.
    again: bool,
    ending: bool,
    error: Option<String>,
    /// Set by the sidebar each time it draws. Polling stops while the panel is hidden.
    pub(super) shown: bool,
    _poll: Task<()>,
}

impl Running {
    /// Live host processes that are mid-turn, for the sidebar footer.
    pub fn running_count(&self) -> usize {
        self.processes.iter().filter(|p| p.running).count()
    }

    pub fn new(cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let Ok(due) = this.update(cx, |s, _| s.shown) else {
                    break;
                };
                if due {
                    let _ = this.update(cx, |s, cx| s.refresh(cx));
                }
            }
        });
        Self {
            processes: vec![],
            loaded: false,
            loading: false,
            again: false,
            ending: false,
            error: None,
            shown: false,
            _poll: poll,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            self.again = true;
            return;
        }
        self.loading = true;
        let task = cx.background_spawn(async { platform::list_processes() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.loading = false;
                match result {
                    Ok(processes) => {
                        s.processes = processes;
                        s.loaded = true;
                        s.error = None;
                    }
                    Err(e) => s.error = Some(e.to_string()),
                }
                if std::mem::take(&mut s.again) {
                    s.refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// End the live process behind each group of threads, then refresh the list.
    fn end(&mut self, groups: Vec<(Scope, Vec<String>)>, cx: &mut Context<Self>) {
        if self.ending || groups.is_empty() {
            return;
        }
        self.ending = true;
        self.error = None;
        let task = cx.background_spawn(async move {
            let mut failure = None;
            for (scope, threads) in groups {
                if let Err(e) = platform::end_threads(scope, threads) {
                    failure.get_or_insert(e.to_string());
                }
            }
            failure
        });
        cx.spawn(async move |this, cx| {
            let failure = task.await;
            let _ = this.update(cx, |s, cx| {
                s.ending = false;
                s.error = failure;
                s.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn end_all_idle(&mut self, cx: &mut Context<Self>) {
        let groups = idle_rows(&self.processes)
            .into_iter()
            .map(|p| (p.scope.clone(), p.threads.clone()))
            .collect();
        self.end(groups, cx);
    }
}

impl Render for Running {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let view = cx.weak_entity();
        let now = now_secs();
        let idle = idle_rows(&self.processes).len();
        let count = self.processes.len();

        let rows = self.processes.iter().enumerate().map(|(i, process)| {
            let view = view.clone();
            let scope = process.scope.clone();
            let threads = process.threads.clone();
            let model = process
                .model
                .clone()
                .unwrap_or_else(|| process.route.clone());
            let end_enabled = !self.ending;
            row()
                .gap_2()
                .items_start()
                .min_w_0()
                .child(
                    div()
                        .mt(px(5.))
                        .size(px(7.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(rgb(if process.running {
                            p.success
                        } else {
                            p.line_strong
                        })),
                )
                .child(
                    col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            row()
                                .gap_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(p.text(Step::Meta))
                                        .text_color(rgb(p.fg))
                                        .child(kind_label(&process.kind)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(p.text(Step::Meta))
                                        .text_color(rgb(p.muted))
                                        .child(model),
                                ),
                        )
                        .child(small(
                            p,
                            format!(
                                "{} · up {} · {}",
                                project_name(&process.project),
                                span(now.saturating_sub(process.started_at)),
                                idle_label(now, process)
                            ),
                        )),
                )
                .child(toolbar_button(
                    p,
                    ElementId::from(format!("running-end-{i}")),
                    "End",
                    end_enabled,
                    move |_, _, cx| {
                        let scope = scope.clone();
                        let threads = threads.clone();
                        let _ = view.update(cx, |s, cx| s.end(vec![(scope, threads)], cx));
                    },
                ))
        });

        let header_view = view.clone();
        col()
            .flex_shrink_0()
            .px_2()
            .py_2()
            .gap_1()
            .border_t_1()
            .border_color(rgb(p.line))
            .child(
                row()
                    .justify_between()
                    .min_w_0()
                    .child(small(p, format!("RUNNING · {count}")))
                    .child(button(
                        p,
                        "running-end-idle",
                        "End all idle",
                        false,
                        idle > 0 && !self.ending,
                        move |_, _, cx| {
                            let _ = header_view.update(cx, |s, cx| s.end_all_idle(cx));
                        },
                    )),
            )
            // Without a reachable host (local-only desktop) there is nothing to list:
            // say so quietly instead of repeating a red error every poll.
            .when(!self.loaded && self.error.is_some(), |c| {
                c.child(small(p, "Session host not running"))
            })
            .when_some(self.error.clone().filter(|_| self.loaded), |c, message| {
                c.child(
                    div()
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(p.error))
                        .child(message),
                )
            })
            .when(count == 0 && (self.loaded || self.error.is_none()), |c| {
                c.child(small(
                    p,
                    if self.loaded {
                        "No live sessions"
                    } else {
                        "Checking the session host…"
                    },
                ))
            })
            .when(count > 0, |c| {
                c.child(
                    col()
                        .id("running-rows")
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .gap_2()
                        .children(rows),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{idle_label, idle_rows, project_name, span};
    use ox_desk_core::host_protocol::{ProcessView, Scope};
    use std::path::Path;

    fn process(threads: &[&str], running: bool, last: i64) -> ProcessView {
        ProcessView {
            kind: "claude".into(),
            scope: Scope {
                project: "/work/app".into(),
                route: ox_desk_core::runtime::Runtime::Claude.route(None),
            },
            project: "/work/app".into(),
            threads: threads.iter().map(|t| (*t).to_owned()).collect(),
            route: "Claude".into(),
            model: None,
            started_at: 0,
            last_activity: last,
            running,
        }
    }

    #[test]
    fn span_formats_each_unit_and_clamps_negative() {
        assert_eq!(span(-5), "0s");
        assert_eq!(span(0), "0s");
        assert_eq!(span(59), "59s");
        assert_eq!(span(60), "1m");
        assert_eq!(span(3599), "59m");
        assert_eq!(span(3600), "1h");
        assert_eq!(span(86_399), "23h");
        assert_eq!(span(86_400), "1d");
        assert_eq!(span(3 * 86_400 + 5), "3d");
    }

    #[test]
    fn idle_label_reports_busy_or_time_since_activity() {
        let busy = process(&["a"], true, 900);
        assert_eq!(idle_label(1000, &busy), "busy");
        let idle = process(&["a"], false, 700);
        assert_eq!(idle_label(1000, &idle), "idle 5m");
        // Activity stamped in the future never shows a negative idle time.
        let future = process(&["a"], false, 2000);
        assert_eq!(idle_label(1000, &future), "idle 0s");
    }

    #[test]
    fn end_all_idle_selects_only_rows_that_are_not_running() {
        let rows = vec![
            process(&["a"], false, 0),
            process(&["b", "c"], true, 0),
            process(&["d", "e"], false, 0),
        ];
        let picked: Vec<Vec<String>> = idle_rows(&rows)
            .into_iter()
            .map(|p| p.threads.clone())
            .collect();
        assert_eq!(
            picked,
            vec![vec!["a".to_owned()], vec!["d".to_owned(), "e".to_owned()]]
        );
        assert!(idle_rows(&rows[1..2]).is_empty());
    }

    #[test]
    fn project_name_is_the_basename_or_the_whole_root() {
        assert_eq!(project_name(Path::new("/home/oscar/ox")), "ox");
        assert_eq!(project_name(Path::new("/")), "/");
    }
}
