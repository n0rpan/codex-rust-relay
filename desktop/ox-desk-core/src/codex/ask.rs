//! Decode Codex approval requests and preserve exact offered decisions.
use super::{frame::RequestId, names::ServerMethod};
pub use crate::asks::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

impl Decision {
    fn command(value: &Value) -> Self {
        let mut choice = Self::new(
            match value.as_str() {
                Some("accept") => "Allow once",
                Some("acceptForSession") => "Allow for this session",
                Some("decline") => "Decline",
                Some("cancel") => "Cancel turn",
                Some(other) => other,
                None => "Apply rule",
            },
            json!({"decision":value}),
        );
        if value.is_object() {
            if let Some(args) = value
                .pointer("/acceptWithExecpolicyAmendment/execpolicy_amendment")
                .and_then(Value::as_array)
            {
                choice.label = "Always allow this prefix".into();
                choice.detail = Some(
                    args.iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            } else if let Some(rule) =
                value.pointer("/applyNetworkPolicyAmendment/network_policy_amendment")
                && let (Some(action), Some(host)) = (rule["action"].as_str(), rule["host"].as_str())
                && matches!(action, "allow" | "deny")
            {
                choice.label = if action == "allow" {
                    "Always allow this host"
                } else {
                    "Always deny this host"
                }
                .into();
                choice.detail = Some(host.to_owned());
            } else {
                choice.detail = Some(value.to_string());
            }
        }
        choice
    }
}

impl Ask {
    pub(super) fn decode(id: RequestId, method: ServerMethod, p: Value) -> Self {
        let string = |key: &str| p[key].as_str().map(str::to_owned);
        let mut lines = Vec::new();
        for key in [
            "message",
            "command",
            "cwd",
            "reason",
            "grantRoot",
            "networkApprovalContext",
            "additionalPermissions",
            "permissions",
        ] {
            if let Some(v) = p.get(key).filter(|v| !v.is_null()) {
                lines.push(format!(
                    "{key} · {}",
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                ));
            }
        }
        if let Some(v) = p["_meta"].get("tool_params") {
            lines.push(v.to_string());
        }
        let decisions = match method {
            ServerMethod::Command | ServerMethod::Files => {
                let fallback = [json!("accept"), json!("decline"), json!("cancel")];
                let offered = match p.get("availableDecisions") {
                    None | Some(Value::Null) => &fallback[..],
                    Some(Value::Array(values)) => values.as_slice(),
                    Some(_) => &[],
                };
                offered.iter().map(Decision::command).collect()
            }
            ServerMethod::Permissions if p["permissions"].is_object() => vec![
                Decision::new(
                    "Allow for this turn",
                    json!({"permissions":p["permissions"],"scope":"turn"}),
                ),
                Decision::new("Decline", json!({"permissions":{},"scope":"turn"})),
            ],
            ServerMethod::Elicit
                if p["mode"] == "form"
                    && p["_meta"]["codex_approval_kind"] == "mcp_tool_call"
                    && p["requestedSchema"]["properties"]
                        .as_object()
                        .is_some_and(|fields| fields.is_empty()) =>
            {
                vec![
                    Decision::new("Allow tool call", json!({"action":"accept","content":{}})),
                    Decision::new("Decline", json!({"action":"decline","content":null})),
                ]
            }
            _ => vec![],
        };
        let mut questions = Vec::new();
        if method == ServerMethod::Question
            && let Ok(parsed) = Vec::<Question>::deserialize(&p["questions"])
        {
            let mut ids = HashSet::new();
            if parsed.iter().all(|q| !q.id.is_empty() && ids.insert(&q.id)) {
                questions = parsed;
            }
        }
        let command = (method == ServerMethod::Command)
            .then(|| {
                Some(CommandApproval {
                    command: string("command")?,
                    cwd: string("cwd").map(Into::into),
                    decline: decisions
                        .iter()
                        .position(|d| d.answer.0 == json!({"decision":"decline"}))?,
                })
            })
            .flatten();
        Self {
            command,
            id,
            thread: string("threadId"),
            turn: string("turnId"),
            item: string("itemId"),
            title: match method {
                ServerMethod::Command => "Run command",
                ServerMethod::Files => "Apply file changes",
                ServerMethod::Permissions => "Grant permissions",
                ServerMethod::Question => "Codex has a question",
                ServerMethod::Elicit => "Allow tool call",
            }
            .into(),
            detail: lines.join("\n"),
            questions,
            decisions,
        }
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;
    #[test]
    fn only_stock_command_requests_with_an_explicit_decline_can_be_handed_off() {
        let mut params = json!({"command":"sudo -u nobody id","cwd":"/project/sub","availableDecisions":["cancel","accept","decline"]});
        let id = RequestId::Number(42);
        let command = Ask::decode(id.clone(), ServerMethod::Command, params.clone())
            .command
            .unwrap();
        assert_eq!(command.command, "sudo -u nobody id");
        assert_eq!(
            command.cwd.as_deref(),
            Some(std::path::Path::new("/project/sub"))
        );
        assert_eq!(command.decline, 2);
        assert!(
            Ask::decode(id.clone(), ServerMethod::Files, params.clone())
                .command
                .is_none()
        );
        params["availableDecisions"] = json!(["accept", "cancel"]);
        assert!(
            Ask::decode(id, ServerMethod::Command, params)
                .command
                .is_none()
        );
    }
    #[test]
    fn persistent_rules_display_the_scope_and_return_the_offered_decision() {
        let prefix = json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["cargo","test","arg with space"]}});
        let host = json!({"applyNetworkPolicyAmendment":{"network_policy_amendment":{"action":"deny","host":"test.invalid"}}});
        for (raw, label, scope) in [
            (
                prefix,
                "Always allow this prefix",
                r#""cargo" "test" "arg with space""#,
            ),
            (host, "Always deny this host", "test.invalid"),
        ] {
            let decision = Decision::command(&raw);
            assert_eq!(decision.label, label);
            assert_eq!(decision.detail.as_deref(), Some(scope));
            assert_eq!(decision.answer.0, json!({"decision":raw}));
        }
    }
}
