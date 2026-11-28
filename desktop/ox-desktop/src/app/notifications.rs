//! OS alerts for live host events. A click navigates through the original pairing.
#[cfg(target_os = "linux")]
mod linux;
use super::Workspace;
use crate::{platform::HostLink, preferences::HostDirectory};
#[cfg(not(target_os = "linux"))]
use gpui::SystemNotification;
use gpui::{App, Context, Global, Window};
use ox_desk_core::host_protocol::{Cursor, NotificationKind, Scope};
use std::{
    collections::VecDeque,
    hash::{DefaultHasher, Hash, Hasher},
};

#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyEvents {
    Off,
    Attention,
    #[default]
    All,
}
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct StoredSettings {
    events: LegacyEvents,
    enabled: Option<bool>,
    approval: Option<bool>,
    question: Option<bool>,
    finished: Option<bool>,
    failed: Option<bool>,
    goal: bool,
    titles: bool,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(from = "StoredSettings", into = "StoredSettings")]
pub(crate) struct Settings {
    pub enabled: bool,
    approval: bool,
    question: bool,
    finished: bool,
    failed: bool,
    pub goal: bool,
    pub titles: bool,
}
impl From<StoredSettings> for Settings {
    fn from(saved: StoredSettings) -> Self {
        Self {
            enabled: saved.enabled.unwrap_or(saved.events != LegacyEvents::Off),
            approval: saved.approval.unwrap_or(true),
            question: saved.question.unwrap_or(true),
            finished: saved
                .finished
                .unwrap_or(saved.events != LegacyEvents::Attention),
            failed: saved.failed.unwrap_or(true),
            goal: saved.goal,
            titles: saved.titles,
        }
    }
}
impl From<Settings> for StoredSettings {
    fn from(settings: Settings) -> Self {
        // An older desktop can still restore the nearest subset of the
        // enabled alerts from the same file.
        let events =
            if settings.enabled && settings.approval && settings.question && settings.failed {
                if settings.finished {
                    LegacyEvents::All
                } else {
                    LegacyEvents::Attention
                }
            } else {
                LegacyEvents::Off
            };
        Self {
            events,
            enabled: Some(settings.enabled),
            approval: Some(settings.approval),
            question: Some(settings.question),
            finished: Some(settings.finished),
            failed: Some(settings.failed),
            goal: settings.goal,
            titles: settings.titles,
        }
    }
}
impl Default for Settings {
    fn default() -> Self {
        StoredSettings::default().into()
    }
}
impl Global for Settings {}
impl Settings {
    pub(crate) fn selected(self, kind: NotificationKind) -> bool {
        match kind {
            NotificationKind::Approval => self.approval,
            NotificationKind::Question => self.question,
            NotificationKind::Finished => self.finished,
            NotificationKind::Failed => self.failed,
        }
    }
    pub(crate) fn set(&mut self, kind: NotificationKind, value: bool) {
        match kind {
            NotificationKind::Approval => self.approval = value,
            NotificationKind::Question => self.question = value,
            NotificationKind::Finished => self.finished = value,
            NotificationKind::Failed => self.failed = value,
        }
    }
    fn headline(self, kind: NotificationKind, goal_reached: bool) -> Option<&'static str> {
        if !self.enabled {
            return None;
        }
        if kind == NotificationKind::Finished && goal_reached && self.goal {
            return Some("Goal reached");
        }
        self.selected(kind).then_some(match kind {
            NotificationKind::Finished => "Turn finished",
            NotificationKind::Failed => "Turn failed",
            NotificationKind::Approval => "Approval needed",
            NotificationKind::Question => "Question from agent",
        })
    }
    pub(crate) fn save(self, cx: &mut App) -> std::io::Result<()> {
        let dir = ox_desk_core::desktop_behavior::directory()
            .ok_or_else(|| std::io::Error::other("Desktop settings directory unavailable"))?;
        crate::workspace_state::write_json(&dir.join("notifications.json"), &self)?;
        let hide_titles = cx.default_global::<Self>().titles && !self.titles;
        cx.set_global(self);
        if !self.enabled || hide_titles {
            dismiss_where(cx, |_| true);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_desktop_alert_presets_keep_their_meaning() {
        let all: Settings = serde_json::from_str(r#"{"events":"all"}"#).unwrap();
        assert!(all.enabled && all.selected(NotificationKind::Finished));

        let attention: Settings =
            serde_json::from_str(r#"{"events":"attention","titles":true}"#).unwrap();
        assert!(attention.enabled && attention.titles);
        assert!(attention.selected(NotificationKind::Approval));
        assert!(attention.selected(NotificationKind::Question));
        assert!(attention.selected(NotificationKind::Failed));
        assert!(!attention.selected(NotificationKind::Finished));

        let off: Settings = serde_json::from_str(r#"{"events":"off"}"#).unwrap();
        assert!(!off.enabled);
    }

    #[test]
    fn switching_off_preserves_selected_events_on_disk() {
        let mut current = Settings::default();
        current.enabled = false;
        current.set(NotificationKind::Question, false);
        let saved = serde_json::to_value(current).unwrap();
        assert_eq!(saved["events"], "off");
        let restored: Settings = serde_json::from_value(saved).unwrap();
        assert!(!restored.enabled);
        assert!(!restored.selected(NotificationKind::Question));
        assert!(restored.selected(NotificationKind::Approval));
    }

    #[test]
    fn a_completed_goal_uses_one_enabled_headline() {
        let mut settings = Settings::default();
        assert_eq!(
            settings.headline(NotificationKind::Finished, true),
            Some("Turn finished")
        );
        settings.goal = true;
        assert_eq!(
            settings.headline(NotificationKind::Finished, true),
            Some("Goal reached")
        );
        settings.set(NotificationKind::Finished, false);
        assert_eq!(
            settings.headline(NotificationKind::Finished, true),
            Some("Goal reached")
        );
        assert_eq!(settings.headline(NotificationKind::Finished, false), None);
        assert_eq!(
            settings.headline(NotificationKind::Failed, true),
            Some("Turn failed")
        );
        settings.enabled = false;
        assert_eq!(settings.headline(NotificationKind::Finished, true), None);
    }
}
#[derive(Clone)]
pub(crate) struct Notice {
    pub host_name: Option<String>,
    pub cursor: Cursor,
    pub scope: Scope,
    pub thread: String,
    pub title: String,
    pub kind: NotificationKind,
    pub goal_reached: bool,
}
#[derive(Clone)]
struct Target {
    host: Option<(String, String, String)>,
    scope: Scope,
    thread: String,
}
#[derive(Default)]
struct Notifications {
    targets: VecDeque<(String, Cursor, Target)>,
    #[cfg(target_os = "linux")]
    sender: Option<flume::Sender<linux::Command>>,
}
impl Global for Notifications {}

impl Notifications {
    /// Independent sidebar and fleet subscriptions can observe the same event.
    fn remember(&mut self, link: &HostLink, notice: &Notice) -> Option<String> {
        let host = link
            .remote()
            .map(|h| (h.id.clone(), h.origin.clone(), h.pairing_id().to_owned()));
        let mut hash = DefaultHasher::new();
        (&host, &notice.scope, &notice.thread, notice.kind as u8).hash(&mut hash);
        let tag = format!("ox-session-{:016x}", hash.finish());
        if let Some(index) = self.targets.iter().position(|(t, _, _)| t == &tag) {
            let (_, previous, _) = &self.targets[index];
            if previous.epoch == notice.cursor.epoch && previous.seq >= notice.cursor.seq {
                return None;
            }
            self.targets.remove(index);
        }
        if self.targets.len() >= 256
            && let Some((_old, _, _)) = self.targets.pop_front()
        {
            #[cfg(target_os = "linux")]
            if let Some(sender) = &self.sender {
                let _ = sender.send(linux::Command::Close(_old));
            }
        }
        self.targets.push_back((
            tag.clone(),
            notice.cursor.clone(),
            Target {
                host,
                scope: notice.scope.clone(),
                thread: notice.thread.clone(),
            },
        ));
        Some(tag)
    }
}
impl Notice {
    pub(crate) fn show(&self, link: &HostLink, cx: &mut App) {
        let settings = *cx.default_global::<Settings>();
        let Some(event) = settings.headline(self.kind, self.goal_reached) else {
            return;
        };
        let Some(tag) = cx.default_global::<Notifications>().remember(link, self) else {
            return;
        };
        if cx.active_window().is_some() {
            return;
        }
        let host = self
            .host_name
            .as_deref()
            .or_else(|| link.remote().map(|h| h.name.as_str()))
            .unwrap_or("This computer");
        let clean = |s: &str| {
            s.chars()
                .filter(|c| !c.is_control())
                .take(128)
                .collect::<String>()
        };
        let body = if settings.titles {
            clean(&self.title)
        } else {
            String::new()
        };
        let title = format!("{} · {event}", clean(host));
        #[cfg(target_os = "linux")]
        if let Some(sender) = &cx.default_global::<Notifications>().sender {
            let _ = sender.send(linux::Command::Show { tag, title, body });
        }
        #[cfg(not(target_os = "linux"))]
        cx.show_system_notification(SystemNotification {
            tag: tag.into(),
            title: title.into(),
            body: body.into(),
            actions: vec![],
        });
    }
}
fn dismiss_where(cx: &mut App, remove: impl Fn(&Target) -> bool) {
    let state = cx.default_global::<Notifications>();
    let tags: Vec<_> = state
        .targets
        .iter()
        .filter(|(_, _, target)| remove(target))
        .map(|(tag, _, _)| tag.clone())
        .collect();
    state.targets.retain(|(_, _, target)| !remove(target));
    #[cfg(target_os = "linux")]
    if let Some(sender) = &state.sender {
        for tag in tags {
            let _ = sender.send(linux::Command::Close(tag));
        }
    }
    #[cfg(not(target_os = "linux"))]
    for tag in tags {
        cx.dismiss_system_notification(&tag);
    }
}
pub(super) fn remove_unpaired(cx: &mut App) {
    let peers = cx
        .global::<HostDirectory>()
        .0
        .iter()
        .map(|h| (h.id.clone(), h.origin.clone(), h.pairing_id().to_owned()))
        .collect::<std::collections::HashSet<_>>();
    dismiss_where(cx, |target| {
        target.host.as_ref().is_some_and(|h| !peers.contains(h))
    });
}
pub(crate) fn init(cx: &mut App) {
    cx.set_global(
        crate::workspace_state::read_preference::<Settings>("notifications.json")
            .unwrap_or_default(),
    );
}
pub(super) fn listen(w: &Window, cx: &mut Context<Workspace>) {
    let workspace = cx.weak_entity();
    let window = w.window_handle();
    let open = move |tag: &str, cx: &mut App| {
        let target = cx
            .default_global::<Notifications>()
            .targets
            .iter()
            .find(|(key, _, _)| key == tag)
            .map(|(_, _, target)| target.clone());
        let Some(target) = target else { return };
        let _ = window.update(cx, |_, w, cx| {
            let _ = workspace.update(cx, |s, cx| {
                w.activate_window();
                let host = match &target.host {
                    Some((id, origin, key))
                        if cx.global::<HostDirectory>().0.iter().any(|h| {
                            h.id == *id && h.origin == *origin && h.pairing_id() == key
                        }) =>
                    {
                        Some(id.clone())
                    }
                    Some(_) => {
                        s.error = Some("This notification’s host is no longer paired.".into());
                        cx.notify();
                        return;
                    }
                    None => None,
                };
                if let Some(entry) = s.inventory.get(&host)
                    && !entry.read(cx).online()
                {
                    s.error = Some(format!("{} is offline.", entry.read(cx).display_name()));
                    cx.notify();
                    return;
                }
                if host.is_none() && !s.inventory.contains_key(&None) {
                    if let Some(local) = &s.local {
                        local.update(cx, |view, cx| {
                            view.show_thread(target.scope, Some(target.thread), None, w, cx)
                        });
                        s.set_active(None);
                        s.settings = None;
                        s.page = None;
                        cx.notify();
                    }
                } else {
                    s.open_confirmed_result(host, target.scope, Some(target.thread), true, w, cx);
                }
            });
        });
    };
    #[cfg(target_os = "linux")]
    {
        let (clicked, events) = flume::unbounded();
        cx.default_global::<Notifications>().sender = Some(linux::start(clicked));
        cx.spawn(async move |_, cx| {
            while let Ok(tag) = events.recv_async().await {
                cx.update(|cx| open(&tag, cx));
            }
        })
        .detach();
    }
    #[cfg(not(target_os = "linux"))]
    cx.on_system_notification_response(move |response, cx| open(&response.tag, cx));
}
