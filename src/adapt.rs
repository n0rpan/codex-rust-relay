//! Bounded recovery from provider request-validation errors.
//!
//! OX supports any OpenAI- or Anthropic-compatible endpoint, and each of them
//! has its own idea of which request fields exist. When a provider rejects a
//! field the stock client always sends, a pure relay ends the turn with a raw
//! provider message. Instead OX names the offending field, removes only that
//! field, and retries (at most [`MAX_ATTEMPTS`] sends in total). The rule is
//! remembered for that provider and model for the rest of the process.
//!
//! The rule table is deliberately small and auditable:
//!
//! * a provider that refuses to disable reasoning loses only the disabling
//!   value (`effort: "none"`, `enabled: false`, `thinking.type: "disabled"`);
//! * a stateless provider loses `store`, and a null `previous_response_id`;
//!   a non-null reference is never dropped, because it is the only link to
//!   earlier turns;
//! * a rejected `reasoning.<field>` loses only that sub-field;
//! * an unknown/unsupported parameter loses exactly the field it names.
//!
//! Fields that carry user intent (`model`, `input`, `messages`, `tools`,
//! `instructions`, `text`, `tool_choice`, `parallel_tool_calls`) and anything
//! that controls credentials, the sandbox or search are never dropped.
//! Authentication, payment, permission and rate-limit failures are never
//! retried, and a response is never retried after any byte of it has reached
//! the client.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// How many times one request may be sent: the original plus two adaptations.
pub const MAX_ATTEMPTS: usize = 3;

/// Top-level request fields OX may omit after a provider rejects them. The list
/// is ordered: the first match wins when a message names several. `text` and
/// `tool_choice` are not here: one carries the JSON-schema output format and the
/// other an explicit tool-use constraint, so answering without either is worse
/// than surfacing the provider error. `parallel_tool_calls` is also kept: its
/// default differs from `false`.
const DROPPABLE: &[&str] = &[
    "reasoning",
    "reasoning_effort",
    "thinking",
    "output_config",
    "cache_control",
    "include",
    "previous_response_id",
    "store",
    "temperature",
    "top_p",
    "top_k",
    "service_tier",
    "stream_options",
];

/// `reasoning` sub-fields OX may omit on their own. A rejection that names
/// `reasoning.summary` says nothing about the effort the user picked, so only
/// the named sub-field goes and the rest of the block is still sent.
const REASONING_FIELDS: &[&str] = &["summary", "generate_summary", "effort"];

/// What to change in a rejected request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// Remove one top-level field.
    Drop(&'static str),
    /// Remove one field inside the `reasoning` object, keeping the others.
    DropReasoningField(&'static str),
    /// Remove only the value that turns reasoning off, keeping any other
    /// reasoning setting the caller asked for.
    ReasoningOff,
    /// The provider keeps no conversation state: remove `store` when there is
    /// no `previous_response_id` to lose.
    StripStateless,
}

impl Action {
    /// Short description for the one-line notice.
    pub fn label(self) -> String {
        match self {
            Self::Drop(field) => format!("omitting {field:?}"),
            Self::DropReasoningField(field) => format!("omitting \"reasoning.{field}\""),
            Self::ReasoningOff => "not sending a disabled reasoning setting".to_owned(),
            Self::StripStateless => "omitting store".to_owned(),
        }
    }

    /// Advice the user can apply permanently, when one exists.
    pub fn advice(self) -> &'static str {
        match self {
            Self::ReasoningOff => {
                "set reasoning_mandatory = true for this model in codeox.toml so `none` is never offered"
            }
            Self::Drop("reasoning" | "reasoning_effort" | "thinking" | "output_config")
            | Self::DropReasoningField(_) => {
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
        Action::DropReasoningField(field) => {
            let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) else {
                return false;
            };
            if reasoning.remove(field).is_none() {
                return false;
            }
            if reasoning.is_empty() {
                object.remove("reasoning");
            }
            true
        }
        Action::ReasoningOff => {
            let mut changed = false;
            if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
                if reasoning.get("effort").and_then(Value::as_str) == Some("none") {
                    reasoning.remove("effort");
                    changed = true;
                }
                if reasoning.get("enabled").and_then(Value::as_bool) == Some(false) {
                    reasoning.remove("enabled");
                    changed = true;
                }
                if reasoning.is_empty() {
                    object.remove("reasoning");
                }
            }
            if object.get("reasoning_effort").and_then(Value::as_str) == Some("none") {
                object.remove("reasoning_effort");
                changed = true;
            }
            if object
                .get("thinking")
                .and_then(|thinking| thinking.get("type"))
                .and_then(Value::as_str)
                == Some("disabled")
            {
                object.remove("thinking");
                changed = true;
            }
            changed
        }
        Action::StripStateless => {
            // A non-null reference is the only link to earlier turns in this
            // request. Removing it would answer from incomplete context, so the
            // provider error is surfaced instead of a silently wrong answer.
            if object
                .get("previous_response_id")
                .is_some_and(|value| !value.is_null())
            {
                return false;
            }
            object.remove("previous_response_id").is_some() | object.remove("store").is_some()
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
    if mentions_reasoning_shutdown(&text)
        && (body.get("reasoning").is_some_and(Value::is_object)
            || body.get("reasoning_effort").is_some()
            || body.get("thinking").is_some())
    {
        // Rejecting the disabling value does not say the whole parameter is
        // unsupported, so only that value is removed and the rule stays useful
        // when the user later picks a real effort.
        return Some(Action::ReasoningOff);
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
        ],
    ) {
        if let Some(field) = named_reasoning_field(&text, body) {
            return Some(Action::DropReasoningField(field));
        }
        return named_field(&text, body).map(Action::Drop);
    }
    None
}

/// A quoted `reasoning.<field>` path whose sub-field the request carries.
fn named_reasoning_field(text: &str, body: &Value) -> Option<&'static str> {
    let reasoning = body.get("reasoning").and_then(Value::as_object)?;
    let quoted = quoted_tokens(text);
    REASONING_FIELDS.iter().copied().find(|field| {
        reasoning.contains_key(*field)
            && quoted
                .iter()
                .any(|token| token.strip_prefix("reasoning.") == Some(*field))
    })
}

/// Picks the request field an error names, without mistaking a field name that
/// appears inside a rejected value for the parameter itself.
///
/// `Unsupported include value: reasoning.encrypted_content` names `include`; the
/// `reasoning` in the value is not the offending parameter. Quoted parameter
/// paths win (`'reasoning.summary'` means `reasoning`), then whole tokens that
/// are not followed by a dot.
fn named_field(text: &str, body: &Value) -> Option<&'static str> {
    let quoted = quoted_tokens(text);
    for field in DROPPABLE {
        if body.get(*field).is_some()
            && quoted
                .iter()
                .any(|token| *token == *field || token.split('.').next() == Some(*field))
        {
            return Some(field);
        }
    }
    DROPPABLE
        .iter()
        .copied()
        .find(|field| body.get(*field).is_some() && names_token(text, field))
}

fn quoted_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    for quote in ['\'', '"', '`'] {
        let mut rest = text;
        while let Some(start) = rest.find(quote) {
            let after = &rest[start + 1..];
            let Some(end) = after.find(quote) else {
                break;
            };
            tokens.push(&after[..end]);
            rest = &after[end + 1..];
        }
    }
    tokens
}

fn names_token(text: &str, field: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(position) = text[from..].find(field) {
        let start = from + position;
        let end = start + field.len();
        let before = start == 0 || !is_ident_byte(bytes[start - 1]);
        // A dot means the match is part of a path/value such as
        // `reasoning.encrypted_content`, not the parameter itself.
        let after = bytes
            .get(end)
            .is_none_or(|byte| !is_ident_byte(*byte) && *byte != b'.');
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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
        self.rules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            Some(Action::ReasoningOff)
        );
        let mut body = body;
        assert!(apply(Action::ReasoningOff, &mut body));
        assert!(body.get("reasoning").is_none());
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
            Some(Action::ReasoningOff)
        );
        let mut body = body;
        assert!(apply(Action::ReasoningOff, &mut body));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn an_anthropic_route_loses_thinking_instead() {
        let body = json!({"model": "m", "thinking": {"type": "disabled"}});
        assert_eq!(
            classify(400, "thinking cannot be disabled for this model", &body),
            Some(Action::ReasoningOff)
        );
        let mut body = body;
        assert!(apply(Action::ReasoningOff, &mut body));
        assert!(body.get("thinking").is_none());
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
    fn a_stateless_refusal_only_strips_when_no_context_would_be_lost() {
        let body = json!({"model": "m", "store": true});
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
        assert!(body.get("store").is_none());

        // A continuation carries its only conversation reference here; the
        // provider error must reach the user instead of a wrong answer.
        let mut body = json!({"model": "m", "store": true, "previous_response_id": "resp_1"});
        assert!(!apply(Action::StripStateless, &mut body));
        assert_eq!(body["previous_response_id"], "resp_1");
    }

    #[test]
    fn a_learned_reasoning_rule_keeps_explicit_supported_efforts() {
        let mut body = json!({"reasoning": {"effort": "high", "summary": "auto"}});
        assert!(!apply(Action::ReasoningOff, &mut body));
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn a_structured_output_constraint_is_never_dropped() {
        let body = json!({"model": "m", "text": {"format": {"type": "json_schema"}}});
        assert_eq!(
            classify(400, "Unsupported parameter: 'text' is not supported", &body),
            None
        );
    }

    #[test]
    fn a_field_name_inside_a_rejected_value_is_not_the_parameter() {
        let body = json!({
            "model": "m",
            "reasoning": {"effort": "high"},
            "include": ["reasoning.encrypted_content"],
        });
        assert_eq!(
            classify(
                400,
                "Unsupported include value: reasoning.encrypted_content",
                &body
            ),
            Some(Action::Drop("include"))
        );
        let body = json!({"model": "m", "reasoning": {"summary": "auto"}});
        assert_eq!(
            classify(
                400,
                "Unsupported parameter: 'reasoning.summary' is not supported with this model.",
                &body
            ),
            Some(Action::DropReasoningField("summary"))
        );
    }

    #[test]
    fn a_rejected_reasoning_sub_field_keeps_the_explicit_effort() {
        let body = json!({"model": "m", "reasoning": {"effort": "high", "summary": "auto"}});
        let action = classify(
            400,
            "Unsupported parameter: 'reasoning.summary' is not supported with this model.",
            &body,
        );
        assert_eq!(action, Some(Action::DropReasoningField("summary")));

        // Learned once, the rule still leaves a later explicit effort alone.
        let memory = Memory::default();
        memory.learn("openrouter", "m", action.unwrap());
        let mut later = json!({"reasoning": {"effort": "xhigh", "summary": "detailed"}});
        memory.apply_known("openrouter", "m", &mut later);
        assert_eq!(later, json!({"reasoning": {"effort": "xhigh"}}));

        // A block left empty is removed instead of sent as `{}`.
        let mut only_summary = json!({"reasoning": {"summary": "auto"}});
        assert!(apply(
            Action::DropReasoningField("summary"),
            &mut only_summary
        ));
        assert!(only_summary.get("reasoning").is_none());
    }

    #[test]
    fn a_value_validation_error_does_not_remove_the_parameter() {
        let body = json!({"model": "m", "temperature": 3});
        assert_eq!(
            classify(400, "Invalid temperature: must be between 0 and 2", &body),
            None
        );
    }

    #[test]
    fn explicit_tool_constraints_are_never_dropped() {
        for body in [
            json!({"model": "m", "tool_choice": "required"}),
            json!({"model": "m", "tool_choice": "none"}),
            json!({"model": "m", "tool_choice": {"type": "function", "name": "f"}}),
            json!({"model": "m", "parallel_tool_calls": false}),
        ] {
            assert_eq!(
                classify(400, "Unsupported parameter: 'tool_choice'", &body),
                None,
                "{body}"
            );
        }
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
