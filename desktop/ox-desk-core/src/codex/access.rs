//! Permission presets correspond to stock Codex thread settings and apply to subsequent turns.
use serde_json::{Value, json};

pub use crate::runtime::Access;

impl Access {
    pub fn params(self, thread: &str) -> Option<Value> {
        let sandbox = match self {
            Self::ReadOnly => "readOnly",
            Self::Workspace | Self::AutoReview => "workspaceWrite",
            Self::Yolo => "dangerFullAccess",
            _ => return None,
        };
        Some(json!({
            "threadId":thread, "sandboxPolicy":{"type":sandbox},
            "approvalPolicy":if self == Self::Yolo {"never"} else {"on-request"},
            "approvalsReviewer":if self == Self::AutoReview {"auto_review"} else {"user"},
        }))
    }

    pub(super) fn from_settings<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::Deserialize;
        Ok(Self::effective(&Value::deserialize(de)?))
    }
    pub fn effective(value: &Value) -> Self {
        let sandbox = value
            .get("sandboxPolicy")
            .or_else(|| value.get("sandbox"))
            .and_then(|s| s["type"].as_str());
        match (
            sandbox,
            value["approvalPolicy"].as_str(),
            value["approvalsReviewer"].as_str(),
        ) {
            (Some("dangerFullAccess"), Some("never"), _) => Self::Yolo,
            (Some("readOnly"), Some("on-request"), Some("user") | None) => Self::ReadOnly,
            (
                Some("workspaceWrite"),
                Some("on-request"),
                Some("auto_review" | "guardian_subagent"),
            ) => Self::AutoReview,
            (Some("workspaceWrite"), Some("on-request"), Some("user") | None) => Self::Workspace,
            _ => Self::Configured,
        }
    }
}
