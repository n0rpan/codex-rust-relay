//! Exact Claude subscription models: the stock picker list, pinning a requested
//! model before the first inference, and confirming the model that answered.
//! Shared by the standalone worker (`workers/jobs.rs`), the worker MCP gateway
//! and the hosted launch (`ox-desk-core`, which includes this file by path).
//! It depends on `std` and `serde_json` alone.
//!
//! The picker comes from stock Claude Code's stream-JSON `initialize` reply,
//! which lists the signed-in account's choices without running a turn, e.g.
//! `{"value":"fable","resolvedModel":"claude-fable-5-2","displayName":"Fable 5.2"}`.
use serde_json::{Value, json};

/// One row of the account's native model picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerModel {
    /// What the picker selects, such as `fable` or `claude-fable-5-1`.
    pub value: String,
    /// The exact model ID the value runs, when the picker reports one.
    pub resolved: Option<String>,
    pub display: String,
    pub description: String,
}

impl PickerModel {
    /// The exact model ID this choice runs.
    pub fn exact(&self) -> &str {
        self.resolved.as_deref().unwrap_or(&self.value)
    }
    pub fn to_json(&self) -> Value {
        json!({
            "value": self.value,
            "resolvedModel": self.resolved,
            "displayName": self.display,
            "description": self.description,
        })
    }
}

/// A requested model resolved against the picker: the exact ID it must run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub requested: String,
    pub exact: String,
    pub display: String,
}

/// The stream-JSON control request that returns the picker without a turn.
pub fn initialize_request(request_id: &str) -> Value {
    json!({"type":"control_request","request_id":request_id,"request":{"subtype":"initialize"}})
}

/// The picker rows of an `initialize` reply payload (its `models` array).
pub fn parse_picker(payload: &Value) -> Option<Vec<PickerModel>> {
    let rows = payload["models"].as_array()?;
    Some(
        rows.iter()
            .filter_map(|row| {
                let value = row["value"].as_str().filter(|v| valid_model(v))?;
                Some(PickerModel {
                    value: value.to_owned(),
                    resolved: row["resolvedModel"]
                        .as_str()
                        .filter(|v| valid_model(v))
                        .map(str::to_owned),
                    display: row["displayName"].as_str().unwrap_or(value).to_owned(),
                    description: row["description"].as_str().unwrap_or("").to_owned(),
                })
            })
            .collect(),
    )
}

/// A plausible model name: what the stock CLI accepts as `--model`, never a flag.
pub fn valid_model(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'[' | b']'))
}

/// Resolve a requested subscription model to the exact model it runs. An exact
/// model ID matches the row that resolves to it; a picker value such as `fable`
/// matches its row. Anything else is refused with the account's choices, so a
/// worker never starts on another model.
pub fn resolve(picker: &[PickerModel], requested: &str) -> Result<Pin, String> {
    let requested = requested.trim();
    if !valid_model(requested) {
        return Err("Provide a Claude subscription model ID such as claude-fable-5-2.".into());
    }
    let row = picker
        .iter()
        .find(|m| m.exact() == requested)
        .or_else(|| picker.iter().find(|m| m.value == requested));
    match row {
        Some(row) => Ok(Pin {
            requested: requested.to_owned(),
            exact: row.exact().to_owned(),
            display: row.display.clone(),
        }),
        None => Err(unavailable(picker, requested)),
    }
}

/// The actionable error for a model the account's picker does not offer.
pub fn unavailable(picker: &[PickerModel], requested: &str) -> String {
    let mut offered: Vec<&str> = Vec::new();
    for m in picker {
        if !offered.contains(&m.exact()) {
            offered.push(m.exact());
        }
    }
    if offered.is_empty() {
        format!(
            "The Claude subscription picker offered no models, so {requested} cannot be pinned. Nothing was started or billed."
        )
    } else {
        format!(
            "This Claude subscription does not offer {requested}. Nothing was started or billed. Choose one of: {}.",
            offered.join(", ")
        )
    }
}

/// Rows whose value, exact model or display name contains the query.
pub fn search<'a>(picker: &'a [PickerModel], query: &str) -> Vec<&'a PickerModel> {
    let query = query.trim().to_lowercase();
    let mut seen: Vec<&str> = Vec::new();
    picker
        .iter()
        .filter(|m| {
            query.is_empty()
                || [m.value.as_str(), m.exact(), m.display.as_str()]
                    .iter()
                    .any(|s| s.to_lowercase().contains(&query))
                // "fable 5.2" finds claude-fable-5-2.
                || m.exact().replace('-', " ").contains(&query.replace(['.', '-'], " "))
        })
        // An alias and the exact row of the same model are one choice: keep the first.
        .filter(|m| {
            let fresh = !seen.contains(&m.exact());
            seen.push(m.exact());
            fresh
        })
        .collect()
}

/// The model a top-level assistant message reports, if it is a real model.
pub fn answering_model(message: &Value) -> Option<&str> {
    message["model"]
        .as_str()
        .filter(|m| valid_model(m) && *m != "<synthetic>")
}

/// Whether the model that answered is the pinned one. A dated snapshot of the
/// pinned ID (`claude-haiku-4-5` answered as `claude-haiku-4-5-20251001`) counts.
pub fn same_model(pinned: &str, actual: &str) -> bool {
    actual == pinned
        || actual.strip_prefix(pinned).is_some_and(|rest| {
            rest.strip_prefix('-')
                .is_some_and(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// How far the runtime has confirmed a worker's model.
/// `pending`: no answer yet. `confirmed`: the first answer came from the pinned
/// model. `differs`: it came from another model. `reported`: the runtime named a
/// model and nothing was pinned to compare it with.
pub fn model_state(pinned: Option<&str>, actual: Option<&str>) -> &'static str {
    match (pinned, actual) {
        (_, None) => "pending",
        (Some(p), Some(a)) if same_model(p, a) => "confirmed",
        (Some(_), Some(_)) => "differs",
        (None, Some(_)) => "reported",
    }
}

/// The warning a worker carries when another model answered than the one pinned.
pub fn differs_warning(pinned: &str, actual: &str) -> String {
    format!(
        "This worker was pinned to {pinned}, but {actual} answered. Treat its result as coming from {actual}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape stock Claude Code 2.1.332 returned for a subscription account.
    fn picker() -> Vec<PickerModel> {
        parse_picker(&json!({"models":[
            {"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)"},
            {"value":"opus","resolvedModel":"claude-opus-5-5","displayName":"Opus 5.5"},
            {"value":"fable","resolvedModel":"claude-fable-5-2","displayName":"Fable 5.2"},
            {"value":"claude-fable-5-1","resolvedModel":"claude-fable-5-1","displayName":"Fable 5.1"},
            {"value":"claude-haiku-4-5-20251001","displayName":"Haiku 4.5"},
            {"value":"--bad","displayName":"flag"}
        ]}))
        .unwrap()
    }

    #[test]
    fn exact_ids_and_picker_values_resolve_to_the_model_they_run() {
        let p = picker();
        assert_eq!(p.len(), 5, "a flag-like value is never offered");
        let pin = resolve(&p, "claude-fable-5-2").unwrap();
        assert_eq!(
            (pin.exact.as_str(), pin.display.as_str()),
            ("claude-fable-5-2", "Fable 5.2")
        );
        assert_eq!(resolve(&p, "fable").unwrap().exact, "claude-fable-5-2");
        assert_eq!(resolve(&p, "default").unwrap().exact, "claude-opus-5-5");
        assert_eq!(
            resolve(&p, "claude-haiku-4-5-20251001").unwrap().exact,
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn an_unoffered_model_is_refused_with_the_choices_and_nothing_billed() {
        let p = picker();
        let error = resolve(&p, "claude-fable-9").unwrap_err();
        assert!(error.contains("does not offer claude-fable-9"), "{error}");
        assert!(error.contains("Nothing was started or billed"));
        assert!(error.contains("claude-fable-5-2"));
        assert_eq!(error.matches("claude-opus-5-5").count(), 1, "{error}");
        assert!(resolve(&p, "--model").is_err());
        assert!(
            resolve(&[], "claude-fable-5-2")
                .unwrap_err()
                .contains("offered no models")
        );
    }

    #[test]
    fn search_finds_subscription_choices_by_name_or_id_once() {
        let p = picker();
        let exact = |q| {
            search(&p, q)
                .iter()
                .map(|m| m.exact().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(exact("fable"), ["claude-fable-5-2", "claude-fable-5-1"]);
        assert_eq!(exact("Fable 5.2"), ["claude-fable-5-2"]);
        assert_eq!(exact("claude-fable-5-2"), ["claude-fable-5-2"]);
        assert_eq!(
            exact("opus"),
            ["claude-opus-5-5"],
            "default and opus are one choice"
        );
        assert!(exact("gpt").is_empty());
    }

    #[test]
    fn the_answering_model_confirms_or_contradicts_the_pin() {
        assert_eq!(model_state(Some("claude-fable-5-2"), None), "pending");
        assert_eq!(
            model_state(Some("claude-fable-5-2"), Some("claude-fable-5-2")),
            "confirmed"
        );
        assert_eq!(
            model_state(Some("claude-haiku-4-5"), Some("claude-haiku-4-5-20251001")),
            "confirmed"
        );
        assert_eq!(
            model_state(Some("claude-fable-5-2"), Some("claude-opus-5-5")),
            "differs"
        );
        assert_eq!(
            model_state(Some("claude-fable-5"), Some("claude-fable-5-2")),
            "differs"
        );
        assert_eq!(model_state(None, Some("claude-opus-5-5")), "reported");
        assert_eq!(answering_model(&json!({"model":"<synthetic>"})), None);
        assert_eq!(
            answering_model(&json!({"model":"claude-fable-5-2"})),
            Some("claude-fable-5-2")
        );
    }
}
