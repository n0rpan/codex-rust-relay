//! Bounded recovery from provider request-validation errors.
//!
//! OX supports any OpenAI- or Anthropic-compatible endpoint, and each of them
//! has its own idea of which request fields exist. When a provider rejects a
//! field the stock client always sends, a pure relay ends the turn with a raw
//! provider message. Instead OX names the offending field, removes only that
//! field, retries once, and remembers the rule for that provider and model for
//! the rest of the process.
//!
//! The rule table is deliberately small and auditable:
//!
//! * a provider that refuses to disable reasoning loses its reasoning block;
//! * a stateless provider loses `store` and `previous_response_id`;
//! * an unknown/unsupported parameter loses exactly the field it names.
//!
//! Fields that carry user intent (`model`, `input`, `messages`, `tools`,
//! `instructions`) and anything that controls credentials, the sandbox or
//! search are never dropped. Authentication, payment, permission and rate-limit
//! failures are never retried, and a response is never retried after any byte
//! of it has reached the client.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// How many times one request may be sent: the original plus two adaptations.
pub const MAX_ATTEMPTS: usize = 3;

/// Top-level request fields OX may omit after a provider rejects them. The list
/// is ordered: the first match wins when a message names several.
const DROPPABLE: &[&str] = &[
    "reasoning",
    "reasoning_effort",
    "thinking",
    "output_config",
    "cache_control",
    "parallel_tool_calls",
    "include",
    "previous_response_id",
    "store",
    "tool_choice",
    "temperature",
    "top_p",
    "top_k",
    "text",
    "service_tier",
    "stream_options",
];

/// What to change in a rejected request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// Remove one top-level field.
    Drop(&'static str),
    /// The provider keeps no conversation state: remove `store` and
    /// `previous_response_id` together.
    StripStateless,
}

impl Action {
    /// Short description for the one-line notice.
    pub fn label(self) -> String {
        match self {
            Self::Drop(field) => format!("omitting {field:?}"),
            Self::StripStateless => "omitting store/previous_response_id".to_owned(),
        }
    }

    /// Advice the user can apply permanently, when one exists.
    pub fn advice(self) -> &'static str {
        match self {
            Self::Drop("reasoning" | "reasoning_effort" | "thinking" | "output_config") => {
                "declare the model's reasoning levels in codeox.toml (reasoning_efforts, default_reasoning_effort)"
            }
            Self::Drop("cache_control") => {
                "this provider does not accept cache hints; OX omits them automatically"
            }
            Self::Drop("store" | "previous_response_id") | Self::StripStateless => {
                "this provider is stateless; OX omits the storage fields automatically"
            }
            Self::Drop(_) => "this provider does not accept that field; OX omits it automatically",
        }
    }
}

/// Applies one action to `body`. Returns true when the body changed.
pub fn apply(action: Action, body: &mut Value) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    match action {
        Action::Drop(field) => object.remove(field).is_some(),
        Action::StripStateless => {
            let store = object.remove("store").is_some();
            let previous = object.remove("previous_response_id").is_some();
            store || previous
        }
    }
}

/// Picks the field a provider error names, if any.
///
/// `message` is the upstream error text (already decoded), and `body` is the
/// request that was rejected, so a field the request does not carry can never
/// be selected.
pub fn classify(status: u16, message: &str, body: &Value) -> Option<Action> {
    if !matches!(status, 400 | 422) {
        return None;
    }
    let text = message.to_ascii_lowercase();
    if text.trim().is_empty() {
        return None;
    }
    if mentions_reasoning_shutdown(&text) {
        if body.get("reasoning").is_some_and(Value::is_object) {
            return Some(Action::Drop("reasoning"));
        }
        for field in ["reasoning_effort", "thinking", "output_config"] {
            if body.get(field).is_some() {
                return Some(Action::Drop(field));
            }
        }
    }
    if mentions(&text, &["store", "previous_response_id", "stateless"])
        && mentions(
            &text,
            &["not supported", "rejected", "stateless", "unsupported"],
        )
        && (body.get("store").is_some() || body.get("previous_response_id").is_some())
    {
        return Some(Action::StripStateless);
    }
    if mentions(
        &text,
        &[
            "unsupported",
            "not supported",
            "unrecognized",
            "unknown",
            "extra",
            "unexpected",
            "not permitted",
            "does not support",
            "invalid",
        ],
    ) {
        for field in DROPPABLE {
            if body.get(*field).is_some() && text.contains(field) {
                return Some(Action::Drop(field));
            }
        }
    }
    None
}

fn mentions(text: &str, words: &[&str]) -> bool {
    words.iter().any(|word| text.contains(word))
}

/// Providers refuse a disabled reasoning block with wording that varies; the
/// stable part is that reasoning is mandatory or cannot be turned off.
fn mentions_reasoning_shutdown(text: &str) -> bool {
    mentions(
        text,
        &[
            "reasoning is mandatory",
            "reasoning is required",
            "thinking is required",
            "cannot be disabled",
            "can't be disabled",
            "cannot disable",
            "can not be disabled",
            "does not support 'none'",
            "does not support \"none\"",
            "does not support `none`",
            "must not be disabled",
        ],
    )
}

/// Learned rules for this process, keyed by provider and upstream model.
#[derive(Default)]
pub struct Memory {
    rules: Mutex<BTreeMap<(String, String), BTreeSet<Action>>>,
}

impl Memory {
    /// Records a rule. Returns true only the first time this provider/model
    /// needs it, which is when the caller should explain the change.
    pub fn learn(&self, vendor: &str, model: &str, action: Action) -> bool {
        let mut rules = self
            .rules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        rules
            .entry((vendor.to_owned(), model.to_owned()))
            .or_default()
            .insert(action)
    }

    /// Applies every rule learned for this provider/model so far.
    pub fn apply_known(&self, vendor: &str, model: &str, body: &mut Value) -> Vec<Action> {
        let rules = self
            .rules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(vendor.to_owned(), model.to_owned()))
            .cloned()
            .unwrap_or_default();
        rules
            .into_iter()
            .filter(|action| apply(*action, body))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_openrouter_refusal_drops_the_reasoning_block() {
        let body = json!({"model": "m", "reasoning": {"effort": "none"}});
        assert_eq!(
            classify(
                400,
                "Reasoning is mandatory for this endpoint and cannot be disabled.",
                &body
            ),
            Some(Action::Drop("reasoning"))
        );
    }

    #[test]
    fn a_chat_route_loses_reasoning_effort_instead() {
        let body = json!({"model": "m", "reasoning_effort": "none"});
        assert_eq!(
            classify(
                400,
                "This model does not support 'none' for reasoning_effort",
                &body
            ),
            Some(Action::Drop("reasoning_effort"))
        );
    }

    #[test]
    fn an_anthropic_route_loses_thinking_instead() {
        let body = json!({"model": "m", "thinking": {"type": "disabled"}});
        assert_eq!(
            classify(400, "thinking cannot be disabled for this model", &body),
            Some(Action::Drop("thinking"))
        );
    }

    #[test]
    fn an_unnamed_unsupported_parameter_is_only_dropped_when_the_request_carries_it() {
        let body = json!({"model": "m", "cache_control": {"type": "ephemeral"}});
        assert_eq!(
            classify(
                400,
                "Unsupported parameter: 'cache_control' is not supported with this model.",
                &body
            ),
            Some(Action::Drop("cache_control"))
        );
        let body = json!({"model": "m"});
        assert_eq!(
            classify(400, "Unsupported parameter: 'cache_control'", &body),
            None
        );
    }

    #[test]
    fn a_stateless_refusal_strips_both_storage_fields() {
        let body = json!({"model": "m", "store": true, "previous_response_id": "resp_1"});
        assert_eq!(
            classify(
                400,
                "Requests that set store: true or previous_response_id are rejected",
                &body
            ),
            Some(Action::StripStateless)
        );
        let mut body = body;
        assert!(apply(Action::StripStateless, &mut body));
        assert!(body.get("store").is_none() && body.get("previous_response_id").is_none());
    }

    #[test]
    fn fields_that_carry_user_intent_are_never_selected() {
        for field in ["model", "input", "messages", "tools", "instructions"] {
            let body = json!({field: "x"});
            assert_eq!(
                classify(400, &format!("Unsupported parameter: '{field}'"), &body),
                None,
                "{field}"
            );
        }
    }

    #[test]
    fn auth_rate_limit_and_server_errors_are_never_adapted() {
        let body = json!({"reasoning": {"effort": "none"}});
        for status in [401, 402, 403, 404, 429, 500, 502, 503] {
            assert_eq!(
                classify(
                    status,
                    "reasoning is mandatory and cannot be disabled",
                    &body
                ),
                None,
                "{status}"
            );
        }
    }

    #[test]
    fn learned_rules_are_per_provider_and_model_and_apply_pre_flight() {
        let memory = Memory::default();
        assert!(memory.learn("openrouter", "a", Action::Drop("reasoning")));
        assert!(!memory.learn("openrouter", "a", Action::Drop("reasoning")));
        let mut body = json!({"reasoning": {"effort": "none"}, "input": []});
        assert_eq!(
            memory.apply_known("openrouter", "a", &mut body),
            vec![Action::Drop("reasoning")]
        );
        assert!(body.get("reasoning").is_none());
        let mut other = json!({"reasoning": {"effort": "none"}});
        assert!(memory.apply_known("openrouter", "b", &mut other).is_empty());
        assert!(other.get("reasoning").is_some());
    }
}
