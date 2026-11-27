//! Subscription windows published in the host's worker inventory.
use super::Limits;
use std::sync::atomic::{AtomicU64, Ordering};
/// Native processes in different scopes can report during the same clock tick.
pub(crate) fn next_sequence() -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}
use std::hash::{Hash, Hasher};
/// A native account identity used only in memory. Without an identity, keep reports
/// from this process separate from reports produced by other processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reporter {
    Account(u64),
    Process(u64),
}
impl Reporter {
    pub(crate) fn verified(
        email: Option<&str>,
        organization: Option<&str>,
        previous: Option<Self>,
    ) -> Self {
        if let Some(email) = email.filter(|s| !s.is_empty()) {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            (email, organization).hash(&mut hash);
            Self::Account(hash.finish())
        } else if let Some(Self::Process(id)) = previous {
            Self::Process(id)
        } else {
            Self::Process(next_sequence())
        }
    }
}
pub struct WorkerReport(Reporter, Report);
enum Report {
    Select,
    Leave,
    Windows(Observation),
}
impl WorkerReport {
    pub(crate) fn select(id: Reporter) -> Self {
        Self(id, Report::Select)
    }
    pub(crate) fn leave(id: Reporter) -> Self {
        Self(id, Report::Leave)
    }
    pub(crate) fn windows(id: Reporter, windows: Observation) -> Self {
        Self(id, Report::Windows(windows))
    }
}

use crate::runtime::Route;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

/// Worker limits are optional inventory hints. An unsupported window must not
/// prevent the rest of the host directory from loading.
pub fn deserialize_windows<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, Vec<ReportedWindow>>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, rows)| {
            let windows: Vec<_> = rows
                .as_array()?
                .iter()
                .filter_map(|row| ReportedWindow::deserialize(row).ok())
                .collect();
            (!windows.is_empty()).then(|| (key.clone(), windows))
        })
        .collect())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReportedWindow {
    pub label: String,
    pub used_percent: Option<i64>,
    pub status: Option<String>,
    pub resets_at: Option<i64>,
    pub observed_at: i64,
}

impl ReportedWindow {
    fn new(
        label: String,
        used_percent: Option<i64>,
        status: Option<String>,
        resets_at: Option<i64>,
    ) -> Self {
        let observed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        Self {
            label,
            used_percent,
            status,
            resets_at,
            observed_at,
        }
    }
}

/// A read replaces all buckets as of the request. A notice replaces just its bucket.
pub(crate) struct Observation {
    at: u64,
    complete: bool,
    buckets: BTreeMap<String, Vec<ReportedWindow>>,
}

impl Observation {
    pub(super) fn codex<'a>(
        at: u64,
        complete: bool,
        buckets: impl Iterator<Item = (&'a String, &'a Limits)>,
    ) -> Self {
        let buckets = buckets
            .map(|(id, limits)| {
                let name = limits.limit_name.as_deref().unwrap_or(id);
                let windows = limits
                    .windows()
                    .filter(|w| w.used_percent >= 0)
                    .map(|w| {
                        let label = match w.window_duration_mins {
                            Some(m) if m > 0 && m % 1440 == 0 => format!("{name} {}d", m / 1440),
                            Some(m) if m > 0 && m % 60 == 0 => format!("{name} {}h", m / 60),
                            Some(m) if m > 0 => format!("{name} {m}m"),
                            _ => name.to_owned(),
                        };
                        ReportedWindow::new(label, Some(w.used_percent), None, w.resets_at)
                    })
                    .collect();
                (id.clone(), windows)
            })
            .collect();
        Self {
            at,
            complete,
            buckets,
        }
    }

    pub(crate) fn claude(info: ClaudeLimit) -> Self {
        let id = info
            .rate_limit_type
            .unwrap_or_else(|| "subscription".into());
        let label = id.replace('_', " ");
        let used = info
            .utilization
            .filter(|u| u.is_finite() && *u >= 0.)
            .map(|u| (100. * u).round() as i64);
        let window = ReportedWindow::new(
            label,
            used,
            Some(info.status.replace('_', " ")),
            info.resets_at,
        );
        Self {
            at: next_sequence(),
            complete: false,
            buckets: BTreeMap::from([(id, vec![window])]),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeLimit {
    pub status: String,
    pub rate_limit_type: Option<String>,
    pub utilization: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Default)]
struct Account {
    reporter: Option<Reporter>,
    last_read: Option<u64>,
    buckets: BTreeMap<String, (u64, Vec<ReportedWindow>)>,
}
#[derive(Default)]
pub struct ReportedLimits {
    accounts: BTreeMap<String, Account>,
}

impl ReportedLimits {
    pub fn record(&mut self, route: &Route, report: WorkerReport) {
        if route.is_api() {
            return;
        }
        let account = self
            .accounts
            .entry(route.runtime.executable().into())
            .or_default();
        let WorkerReport(source, change) = report;
        let observation = match change {
            Report::Select => {
                if account.reporter != Some(source) {
                    *account = Account {
                        reporter: Some(source),
                        ..Default::default()
                    };
                }
                return;
            }
            Report::Leave if account.reporter == Some(source) => {
                *account = Account::default();
                return;
            }
            Report::Windows(w) if account.reporter == Some(source) => w,
            _ => return,
        };
        if account.last_read.is_some_and(|t| t > observation.at) {
            return;
        }
        if observation.complete {
            account.last_read = Some(observation.at);
            account.buckets.retain(|_, (at, _)| *at > observation.at);
        }
        for (key, windows) in observation.buckets {
            if account
                .buckets
                .get(&key)
                .is_none_or(|(at, _)| *at <= observation.at)
            {
                account.buckets.insert(key, (observation.at, windows));
            }
        }
    }

    pub fn current(&self, now: i64) -> BTreeMap<String, Vec<ReportedWindow>> {
        self.accounts
            .iter()
            .filter_map(|(name, account)| {
                let windows: Vec<_> = account
                    .buckets
                    .values()
                    .flat_map(|(_, w)| w)
                    .filter(|w| w.resets_at.is_none_or(|reset| reset > now))
                    .cloned()
                    .collect();
                (!windows.is_empty()).then(|| (name.clone(), windows))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        claude::{Decoder, Event},
        runtime::Runtime,
    };
    use serde_json::json;

    #[test]
    fn reports_keep_provider_scope_and_ignore_older_reads_and_expired_windows() {
        let mut cache = ReportedLimits::default();
        let bucket = |used| {
            serde_json::from_value::<Limits>(json!({
            "limitName":"standard","primary":{"usedPercent":used,"resetsAt":200,"windowDurationMins":300}
        })).unwrap()
        };
        let id = "standard".to_owned();
        let source = Reporter::verified(Some("fixture@example.invalid"), None, None);
        for rt in [Runtime::Codex, Runtime::Claude] {
            cache.record(&rt.route(None), WorkerReport::select(source));
        }
        let mut record =
            |route: &Route, obs| cache.record(route, WorkerReport::windows(source, obs));
        let read_started = next_sequence();
        record(
            &Route::CODEX_SUBSCRIPTION,
            Observation::codex(next_sequence(), false, std::iter::once((&id, &bucket(80)))),
        );
        record(
            &Route::CODEX_SUBSCRIPTION,
            Observation::codex(read_started, true, std::iter::once((&id, &bucket(10)))),
        );
        let frame = json!({"type":"rate_limit_event","session_id":"native","rate_limit_info":{
            "status":"allowed_warning","rateLimitType":"five_hour","resetsAt":300,"utilization":0.91
        }});
        let mut decoder = Decoder::default();
        let Event::SubscriptionLimit { session_id, info } =
            decoder.decode_value(frame).unwrap().pop().unwrap()
        else {
            panic!()
        };
        assert_eq!(session_id, "native");
        record(
            &Runtime::Claude.route(Some("api".into())),
            Observation::claude(info.clone()),
        );

        record(&Runtime::Claude.route(None), Observation::claude(info));
        let recent = cache.current(100);
        assert_eq!(recent["codex"][0].used_percent, Some(80));
        assert_eq!(recent["claude"][0].used_percent, Some(91));
        assert_eq!(
            recent["claude"][0].status.as_deref(),
            Some("allowed warning")
        );
        assert!(!cache.current(200).contains_key("codex"));
        assert!(cache.current(300).is_empty());
        let other = Reporter::verified(None, None, None);
        cache.record(&Route::CODEX_SUBSCRIPTION, WorkerReport::select(other));
        cache.record(
            &Route::CODEX_SUBSCRIPTION,
            WorkerReport::windows(
                source,
                Observation::codex(next_sequence(), false, std::iter::once((&id, &bucket(90)))),
            ),
        );
        assert!(!cache.current(100).contains_key("codex"));
    }
}
