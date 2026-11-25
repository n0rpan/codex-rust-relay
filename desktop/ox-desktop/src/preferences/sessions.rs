//! Read-only session facts: the idle reaper and the write-worker sandbox. No new behaviour; the
//! reaper is still set only by the environment variable.
use super::Preferences;
use crate::appearance::{Palette, Step};
use gpui::{prelude::*, *};

/// Minutes an unused session lasts before the host ends it, as the host reads its setting.
pub(super) fn idle_minutes(setting: Option<&str>) -> u64 {
    setting
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(30)
}

pub(super) fn idle_text(minutes: u64) -> String {
    match minutes {
        0 => "Never (the idle reaper is off)".to_owned(),
        m if m % 60 == 0 => format!("{} h", m / 60),
        m => format!("{m} min"),
    }
}

fn on_path(program: &str, path: Option<&std::ffi::OsStr>) -> bool {
    path.is_some_and(|path| {
        std::env::split_paths(path)
            .any(|dir| std::fs::metadata(dir.join(program)).is_ok_and(|meta| meta.is_file()))
    })
}

/// Names of the sandbox tools that are missing from `PATH`.
pub(super) fn missing_sandbox_tools(path: Option<&std::ffi::OsStr>) -> Vec<&'static str> {
    ["bwrap", "socat"]
        .into_iter()
        .filter(|tool| !on_path(tool, path))
        .collect()
}

/// A positive limit as the host reads it; unset, zero or unparsable means none.
pub(super) fn worker_limit(setting: Option<&str>) -> Option<u64> {
    setting
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
}

pub(super) fn limits_text(minutes: Option<u64>, per_parent: Option<u64>) -> String {
    if minutes.is_none() && per_parent.is_none() {
        return "none (set OX_WORKER_MAX_MINUTES / OX_WORKER_MAX_PER_PARENT to add one)".to_owned();
    }
    let minutes = minutes.map_or("no time limit".to_owned(), |m| format!("{m} min each"));
    let count = per_parent.map_or("no count limit".to_owned(), |c| format!("{c} per session"));
    format!("{minutes}, {count}")
}

pub(super) fn sandbox_text(missing: &[&str]) -> String {
    if missing.is_empty() {
        "Ready (bwrap and socat found).".to_owned()
    } else {
        format!(
            "Not available: {} not found. Write workers can only edit files, without a sandbox.",
            missing.join(" and ")
        )
    }
}

impl Preferences {
    pub(super) fn sessions_page(&self, p: Palette, _cx: &mut Context<Self>) -> Div {
        let minutes = idle_minutes(std::env::var("OX_HOST_IDLE_MINUTES").ok().as_deref());
        let missing = missing_sandbox_tools(std::env::var_os("PATH").as_deref());
        let fact = |label: &'static str, value: String, note: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(p.text(Step::Ui)).child(label))
                .child(
                    div()
                        .text_size(p.text(Step::Ui))
                        .text_color(rgb(if label == "Worker sandbox" && !missing.is_empty() {
                            p.warning
                        } else {
                            p.fg
                        }))
                        .child(value),
                )
                .child(
                    div()
                        .text_size(p.text(Step::Label))
                        .text_color(rgb(p.muted))
                        .child(note),
                )
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().text_size(p.text(Step::Title)).child("Sessions"))
            .child(fact(
                "Idle sessions end after",
                idle_text(minutes),
                "Sessions with a running turn, a pending approval or a worker are never ended. \
                 To change this, set OX_HOST_IDLE_MINUTES (0 turns it off) before starting OX.",
            ))
            .child(fact(
                "Worker limits",
                limits_text(
                    worker_limit(std::env::var("OX_WORKER_MAX_MINUTES").ok().as_deref()),
                    worker_limit(std::env::var("OX_WORKER_MAX_PER_PARENT").ok().as_deref()),
                ),
                "Workers may run for days. Both limits are opt-in environment variables set \
                 before starting OX.",
            ))
            .child(fact(
                "Worker sandbox",
                sandbox_text(&missing),
                "Write workers use bwrap and socat to isolate their commands. This is a status \
                 check only.",
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        idle_minutes, idle_text, limits_text, missing_sandbox_tools, sandbox_text, worker_limit,
    };

    #[test]
    fn session_facts_read_naturally() {
        assert_eq!(idle_minutes(None), 30);
        assert_eq!(idle_minutes(Some("x")), 30);
        assert_eq!(idle_text(30), "30 min");
        assert_eq!(idle_text(60), "1 h");
        assert!(idle_text(0).starts_with("Never"));

        let missing = missing_sandbox_tools(Some(std::ffi::OsStr::new("/nonexistent-dir")));
        assert_eq!(missing, ["bwrap", "socat"]);
        assert!(sandbox_text(&missing).contains("bwrap and socat"));
        assert!(sandbox_text(&[]).starts_with("Ready"));
        assert_eq!(worker_limit(Some("0")), None);
        assert!(limits_text(None, None).starts_with("none"));
        assert_eq!(limits_text(Some(90), Some(2)), "90 min each, 2 per session");
    }
}
