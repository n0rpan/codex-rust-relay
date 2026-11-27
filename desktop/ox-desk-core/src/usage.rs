//! Provider-reported thread usage and account limit windows.
pub mod reported;
use reported::next_sequence;
use reported::{Observation, Reporter, WorkerReport};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCounts {
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub total: TokenCounts,
    pub last: TokenCounts,
    pub model_context_window: Option<i64>,
}
impl TokenUsage {
    pub fn context_percent(&self) -> Option<f64> {
        self.model_context_window
            .filter(|&n| n > 0)
            .map(|size| 100. * self.last.total_tokens.max(0) as f64 / size as f64)
    }
}
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    pub used_percent: i64,
    pub resets_at: Option<i64>,
    pub window_duration_mins: Option<i64>,
}
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub primary: Option<LimitWindow>,
    pub secondary: Option<LimitWindow>,
    pub limit_name: Option<String>,
    pub limit_id: Option<String>,
}
impl Limits {
    pub fn windows(&self) -> impl Iterator<Item = &LimitWindow> {
        self.primary.iter().chain(self.secondary.iter())
    }
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLimits {
    pub rate_limits: Limits,
    pub rate_limits_by_limit_id: Option<BTreeMap<String, Limits>>,
}
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub limits: BTreeMap<String, Limits>,
    pub requested: bool,
    pub loading: bool,
    pub error: Option<String>,
    #[serde(skip)]
    pub worker_report: Option<WorkerReport>,
    #[serde(skip)]
    pub(crate) reporter: Option<Reporter>,
    revision: u64,
    updated: BTreeMap<String, u64>,
}
impl Usage {
    pub(crate) fn codex_account(&mut self, reply: &crate::codex::reply::AccountReply) {
        let next = reply
            .chatgpt()
            .then(|| Reporter::verified(reply.email(), None, self.reporter));
        if next != self.reporter {
            self.requested = false;
            self.limits.clear();
            self.updated.clear();
            self.worker_report = next
                .map(WorkerReport::select)
                .or_else(|| self.reporter.map(WorkerReport::leave));
            self.reporter = next;
        }
    }
    pub(crate) fn version(&self) -> u64 {
        self.revision
    }
    pub(crate) fn update(&mut self, limits: Limits) {
        self.revision += 1;
        self.error = None;
        let key = limits.limit_id.clone().unwrap_or_else(|| "codex".into());
        self.worker_report = self.reporter.map(|id| {
            WorkerReport::windows(
                id,
                Observation::codex(next_sequence(), false, std::iter::once((&key, &limits))),
            )
        });
        self.updated.insert(key.clone(), self.revision);
        self.limits.insert(key, limits);
    }
    pub(crate) fn read(&mut self, data: AccountLimits, requested_at: u64, started: u64) {
        self.loading = false;
        self.error = None;
        let mut buckets = data.rate_limits_by_limit_id.unwrap_or_default();
        buckets
            .entry(
                data.rate_limits
                    .limit_id
                    .clone()
                    .unwrap_or_else(|| "codex".into()),
            )
            .or_insert(data.rate_limits);
        self.worker_report = self
            .reporter
            .map(|id| WorkerReport::windows(id, Observation::codex(started, true, buckets.iter())));
        self.limits
            .retain(|key, _| self.updated.get(key).is_some_and(|v| *v > requested_at));
        self.updated.retain(|_, v| *v > requested_at);
        for (key, bucket) in buckets {
            self.limits.entry(key).or_insert(bucket);
        }
    }
    /// Only warn about current windows, including a fully exhausted one.
    pub fn closest_limit(&self, now: i64) -> Option<(&str, &LimitWindow)> {
        self.limits
            .iter()
            .flat_map(|(key, bucket)| {
                bucket
                    .windows()
                    .map(move |w| (bucket.limit_name.as_deref().unwrap_or(key), w))
            })
            .filter(|(_, w)| w.used_percent >= 90 && w.resets_at.is_none_or(|t| t > now))
            .max_by_key(|(_, w)| w.used_percent)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn live_limits_win_over_older_reads_and_expired_windows_do_not_warn() {
        let mut usage = Usage::default();
        let bucket = |key: &str, percent: i64| {
            serde_json::from_value::<Limits>(json!({"limitId":key,"primary":{"usedPercent":percent,"resetsAt":200,"windowDurationMins":300}})).unwrap()
        };
        usage.update(bucket("codex", 91));
        usage.read(
            AccountLimits {
                rate_limits: bucket("codex", 20),
                rate_limits_by_limit_id: Some(BTreeMap::from([(
                    "extra".into(),
                    bucket("extra", 40),
                )])),
            },
            0,
            next_sequence(),
        );
        assert_eq!(usage.limits.len(), 2);
        assert_eq!(usage.closest_limit(199).unwrap().1.used_percent, 91);
        assert!(usage.closest_limit(200).is_none());
    }
}
