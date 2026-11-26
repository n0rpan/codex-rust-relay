//! Permission presets correspond to stock Codex thread settings and apply to subsequent turns.
//!
//! `thread/start`, `thread/resume`, `thread/fork` and `thread/settings/updated`
//! report the thread's effective settings: the legacy `sandbox`/`sandboxPolicy`
//! projection, `approvalPolicy`, `approvalsReviewer` and, with the experimental
//! API, `activePermissionProfile`. Two facts are read from them:
//!
//! - [`Access`], the mode shown to the user. It names a preset only when the
//!   settings are exactly that preset; anything else is `Configured`.
//! - The ceiling, the highest [`Level`] whose preset grants a child nothing the
//!   thread cannot already do without a person. A configured thread still has
//!   one when every reported fact is understood. It is lowered whenever a
//!   detail could make a preset broader than the thread.
//!
//! A user-defined permission profile is never verified. Stock Codex no longer
//! represents restricted reads in the legacy projection, so a profile that
//! denies reads still reports `readOnly` or `workspaceWrite`. Children always
//! start in built-in presets that read the whole filesystem.
use crate::modes::Level;
use serde_json::{Map, Value, json};

pub use crate::runtime::Access;

/// Built-in profile ids. Any other id is user-defined or unknown.
const READ_ONLY: &str = ":read-only";
const WORKSPACE: &str = ":workspace";
const FULL_ACCESS: &str = ":danger-full-access";

/// The verified facts of a Codex thread's effective settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions {
    /// The mode shown to the user.
    pub access: Access,
    /// What the thread grants its children. `None` is unverified.
    pub ceiling: Option<Level>,
}

impl Permissions {
    pub fn effective(value: &Value) -> Self {
        match Facts::read(value) {
            Some(facts) => Self {
                access: facts.access(),
                ceiling: Some(facts.ceiling()),
            },
            None => Self {
                access: Access::Configured,
                ceiling: None,
            },
        }
    }
    pub(super) fn from_settings<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::Deserialize;
        Ok(Self::effective(&Value::deserialize(de)?))
    }
}

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

    pub fn effective(value: &Value) -> Self {
        Permissions::effective(value).access
    }
}

/// What the sandbox lets the thread do without asking anyone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Reach {
    /// Reads everything and writes nothing.
    Read,
    /// Writes the workspace and the temporary directories, as the Workspace preset does.
    Write,
    /// No sandbox.
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Approval {
    OnRequest,
    Never,
    /// Asks before commands that are not known to be safe.
    Untrusted,
    /// On-request with some prompts rejected without asking.
    Granular,
}

struct Facts {
    reach: Reach,
    /// The sandbox is exactly the one its preset sets, with nothing added or removed.
    exact: bool,
    approval: Approval,
    /// A classifier rather than a person reviews approval requests.
    classifier: bool,
}

impl Facts {
    /// `None` when any reported fact is unknown or cannot be verified.
    fn read(value: &Value) -> Option<Self> {
        let profile = match value.get("activePermissionProfile") {
            None | Some(Value::Null) => None,
            Some(p) => Some(p.get("id")?.as_str()?),
        };
        if profile.is_some_and(|id| ![READ_ONLY, WORKSPACE, FULL_ACCESS].contains(&id)) {
            return None;
        }
        let sandbox = value
            .get("sandboxPolicy")
            .or_else(|| value.get("sandbox"))?
            .as_object()?;
        let (reach, exact) = Self::sandbox(sandbox, profile)?;
        let approval = match value.get("approvalPolicy")? {
            Value::String(s) => match s.as_str() {
                "on-request" => Approval::OnRequest,
                "never" => Approval::Never,
                "untrusted" => Approval::Untrusted,
                _ => return None,
            },
            Value::Object(o) if o.len() == 1 && o.get("granular").is_some_and(Value::is_object) => {
                Approval::Granular
            }
            _ => return None,
        };
        let classifier = match value.get("approvalsReviewer") {
            None | Some(Value::Null) => false,
            Some(v) => match v.as_str()? {
                "user" => false,
                "auto_review" | "guardian_subagent" => true,
                _ => return None,
            },
        };
        Some(Self {
            reach,
            exact,
            approval,
            classifier,
        })
    }

    /// The legacy sandbox projection. Fields it does not know make it unverified:
    /// older servers reported restricted reads in `access` and `readOnlyAccess`.
    fn sandbox(sandbox: &Map<String, Value>, profile: Option<&str>) -> Option<(Reach, bool)> {
        let only = |known: &[&str]| {
            sandbox
                .keys()
                .all(|k| k == "type" || known.contains(&k.as_str()))
        };
        let flag = |key: &str| sandbox.get(key).map_or(Some(false), Value::as_bool);
        let roots = || -> Option<Vec<&str>> {
            sandbox.get("writableRoots").map_or(Some(Vec::new()), |r| {
                r.as_array()?.iter().map(Value::as_str).collect()
            })
        };
        const WRITE: &[&str] = &[
            "writableRoots",
            "networkAccess",
            "excludeTmpdirEnvVar",
            "excludeSlashTmp",
        ];
        Some(match (sandbox.get("type")?.as_str()?, profile) {
            ("readOnly", None | Some(READ_ONLY)) if only(&["networkAccess"]) => {
                (Reach::Read, !flag("networkAccess")?)
            }
            ("workspaceWrite", None | Some(WORKSPACE)) if only(WRITE) => {
                let extra = !roots()?.is_empty() || flag("networkAccess")?;
                // A preset child writes /tmp and $TMPDIR. A thread that may not cannot pass that on.
                let no_tmp = flag("excludeSlashTmp")? || flag("excludeTmpdirEnvVar")?;
                let reach = if no_tmp { Reach::Read } else { Reach::Write };
                (reach, !extra && !no_tmp)
            }
            ("dangerFullAccess", None | Some(FULL_ACCESS)) if only(&[]) => (Reach::Full, true),
            // The built-in full-access profile projects onto a writable `/` with network.
            ("workspaceWrite", Some(FULL_ACCESS))
                if only(WRITE)
                    && roots()? == ["/"]
                    && flag("networkAccess")?
                    && !flag("excludeSlashTmp")?
                    && !flag("excludeTmpdirEnvVar")? =>
            {
                (Reach::Full, true)
            }
            _ => return None,
        })
    }

    /// The preset these settings are exactly, or `Configured`.
    fn access(&self) -> Access {
        match (self.reach, self.approval, self.classifier) {
            _ if !self.exact => Access::Configured,
            (Reach::Full, Approval::Never, _) => Access::Yolo,
            (Reach::Read, Approval::OnRequest, false) => Access::ReadOnly,
            (Reach::Write, Approval::OnRequest, true) => Access::AutoReview,
            (Reach::Write, Approval::OnRequest, false) => Access::Workspace,
            _ => Access::Configured,
        }
    }

    /// The highest level whose Codex preset does nothing this thread could not
    /// do without a person. Every legacy sandbox reads the whole filesystem, so
    /// read is always within it.
    fn ceiling(&self) -> Level {
        let reach = match self.reach {
            Reach::Read => Level::Read,
            Reach::Write => Level::Write,
            Reach::Full => Level::Yolo,
        };
        match self.approval {
            // It asks a person before most commands, as Claude Code's Default does.
            Approval::Untrusted => Level::Read,
            // Some approval requests are rejected unasked, so a classifier
            // reviewing all of them would be broader.
            Approval::Granular => reach.min(Level::Write),
            // Nothing leaves the sandbox. Without a sandbox, nothing is reviewed.
            Approval::Never => reach,
            // Requests leave the sandbox only with approval. A classifier may
            // approve them for a child only when one approves them here, or when
            // there is no sandbox to leave. A person still grants YOLO.
            Approval::OnRequest => match self.reach {
                Reach::Read => Level::Read,
                Reach::Write if self.classifier => Level::Classifier,
                Reach::Write => Level::Write,
                Reach::Full => Level::Classifier,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(sandbox: Value, approval: Value, reviewer: &str, profile: Value) -> Value {
        json!({"sandbox":sandbox,"approvalPolicy":approval,"approvalsReviewer":reviewer,
               "activePermissionProfile":profile})
    }
    fn check(value: Value, access: Access, ceiling: Option<Level>) {
        let p = Permissions::effective(&value);
        assert_eq!((p.access, p.ceiling), (access, ceiling), "{value}");
        // A preset's ceiling is always its own level.
        if access != Access::Configured {
            assert_eq!(p.ceiling, Level::of(crate::runtime::Runtime::Codex, access));
        }
    }

    /// Replies captured from stock `codex app-server` 0.172.0 (`thread/start` and
    /// `thread/settings/updated`) with an isolated `CODEX_HOME`.
    #[test]
    fn stock_presets_stay_exact() {
        let read = json!({"type":"readOnly","networkAccess":false});
        let ws = json!({"type":"workspaceWrite","writableRoots":[],"networkAccess":false,
                        "excludeTmpdirEnvVar":false,"excludeSlashTmp":false});
        // An empty config: the implicit built-in default.
        check(
            settings(
                read.clone(),
                "on-request".into(),
                "user",
                json!({"id":":read-only","extends":null}),
            ),
            Access::ReadOnly,
            Some(Level::Read),
        );
        // OX's own presets via thread/settings/update report no profile.
        check(
            settings(read, "on-request".into(), "user", Value::Null),
            Access::ReadOnly,
            Some(Level::Read),
        );
        check(
            settings(ws.clone(), "on-request".into(), "user", Value::Null),
            Access::Workspace,
            Some(Level::Write),
        );
        check(
            settings(
                ws.clone(),
                "on-request".into(),
                "user",
                json!({"id":":workspace"}),
            ),
            Access::Workspace,
            Some(Level::Write),
        );
        check(
            settings(ws, "on-request".into(), "auto_review", Value::Null),
            Access::AutoReview,
            Some(Level::Classifier),
        );
        check(
            settings(
                json!({"type":"dangerFullAccess"}),
                "never".into(),
                "user",
                Value::Null,
            ),
            Access::Yolo,
            Some(Level::Yolo),
        );
        // `permissions = ":danger-full-access"` projects onto a writable `/`.
        let full = json!({"type":"workspaceWrite","writableRoots":["/"],"networkAccess":true,
                          "excludeTmpdirEnvVar":false,"excludeSlashTmp":false});
        check(
            settings(
                full.clone(),
                "never".into(),
                "user",
                json!({"id":":danger-full-access"}),
            ),
            Access::Yolo,
            Some(Level::Yolo),
        );
        check(
            settings(
                full.clone(),
                "on-request".into(),
                "user",
                json!({"id":":danger-full-access"}),
            ),
            Access::Configured,
            Some(Level::Classifier),
        );
        // Without the built-in id, a writable `/` is only a workspace sandbox with extra roots.
        check(
            settings(full, "never".into(), "user", Value::Null),
            Access::Configured,
            Some(Level::Write),
        );
        // The notification's field name and older replies without a profile or reviewer.
        check(
            json!({"sandboxPolicy":{"type":"readOnly"},"approvalPolicy":"on-request"}),
            Access::ReadOnly,
            Some(Level::Read),
        );
    }

    #[test]
    fn configured_stock_settings_get_a_conservative_ceiling() {
        let ws = |extra: Value| {
            let mut s = json!({"type":"workspaceWrite","writableRoots":[],"networkAccess":false,
                               "excludeTmpdirEnvVar":false,"excludeSlashTmp":false});
            s.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            s
        };
        let granular = json!({"granular":{"sandbox_approval":true,"rules":true,
            "skill_approval":false,"request_permissions":false,"mcp_elicitations":true}});
        let danger = json!({"type":"dangerFullAccess"});
        let none = Value::Null;
        for (sandbox, approval, reviewer, ceiling) in [
            // sandbox_mode = "danger-full-access", approval_policy = "on-request"
            (
                danger.clone(),
                json!("on-request"),
                "user",
                Level::Classifier,
            ),
            (
                danger.clone(),
                json!("on-request"),
                "auto_review",
                Level::Classifier,
            ),
            (danger.clone(), json!("untrusted"), "user", Level::Read),
            (danger, granular.clone(), "user", Level::Write),
            // sandbox_mode = "workspace-write", approval_policy = "never", network and an extra root
            (
                ws(json!({"writableRoots":["/srv/extra"],"networkAccess":true})),
                json!("never"),
                "user",
                Level::Write,
            ),
            (ws(json!({})), json!("never"), "auto_review", Level::Write),
            (
                ws(json!({"networkAccess":true})),
                json!("on-request"),
                "user",
                Level::Write,
            ),
            (
                ws(json!({"networkAccess":true})),
                json!("on-request"),
                "auto_review",
                Level::Classifier,
            ),
            (ws(json!({})), granular.clone(), "auto_review", Level::Write),
            (ws(json!({})), json!("untrusted"), "user", Level::Read),
            // A preset child writes /tmp, which this thread may not.
            (
                ws(json!({"excludeSlashTmp":true})),
                json!("on-request"),
                "auto_review",
                Level::Read,
            ),
            (
                ws(json!({"excludeTmpdirEnvVar":true})),
                json!("never"),
                "user",
                Level::Read,
            ),
            (
                json!({"type":"readOnly","networkAccess":true}),
                json!("on-request"),
                "user",
                Level::Read,
            ),
            (
                json!({"type":"readOnly"}),
                json!("never"),
                "user",
                Level::Read,
            ),
            (json!({"type":"readOnly"}), granular, "user", Level::Read),
        ] {
            check(
                settings(sandbox, approval, reviewer, none.clone()),
                Access::Configured,
                Some(ceiling),
            );
        }
    }

    #[test]
    fn custom_and_unknown_policies_grant_nothing() {
        let read = json!({"type":"readOnly","networkAccess":false});
        let ws = json!({"type":"workspaceWrite","writableRoots":[],"networkAccess":true,
                        "excludeTmpdirEnvVar":false,"excludeSlashTmp":false});
        for value in [
            // A profile that denies /home reports a plain readOnly sandbox.
            settings(
                read.clone(),
                "on-request".into(),
                "user",
                json!({"id":"ci","extends":null}),
            ),
            // Extending a built-in can still add read restrictions.
            settings(
                ws.clone(),
                "on-request".into(),
                "user",
                json!({"id":"wide","extends":":workspace"}),
            ),
            settings(
                ws.clone(),
                "never".into(),
                "user",
                json!({"id":"rw","extends":":read-only"}),
            ),
            // Unknown or mismatched built-in ids.
            settings(
                read.clone(),
                "on-request".into(),
                "user",
                json!({"id":":future"}),
            ),
            settings(
                read.clone(),
                "on-request".into(),
                "user",
                json!({"id":":workspace"}),
            ),
            settings(
                ws.clone(),
                "never".into(),
                "user",
                json!({"id":":danger-full-access"}),
            ),
            settings(
                read.clone(),
                "on-request".into(),
                "user",
                json!({"name":"no id"}),
            ),
            // An external sandbox's bounds are not reported.
            settings(
                json!({"type":"externalSandbox","networkAccess":"restricted"}),
                "never".into(),
                "user",
                Value::Null,
            ),
            // Restricted reads in older replies and fields this build does not know.
            settings(
                json!({"type":"readOnly","access":{"type":"restricted"}}),
                "on-request".into(),
                "user",
                Value::Null,
            ),
            settings(
                json!({"type":"workspaceWrite","readOnlyAccess":{"type":"restricted"}}),
                "on-request".into(),
                "user",
                Value::Null,
            ),
            settings(
                json!({"type":"dangerFullAccess","new":1}),
                "never".into(),
                "user",
                Value::Null,
            ),
            settings(
                json!({"type":"workspaceWrite","writableRoots":"/"}),
                "never".into(),
                "user",
                Value::Null,
            ),
            // Unknown approval policies and reviewers.
            settings(read.clone(), "on-failure".into(), "user", Value::Null),
            settings(
                read.clone(),
                json!({"granular":{},"other":{}}),
                "user",
                Value::Null,
            ),
            settings(read.clone(), "on-request".into(), "someone", Value::Null),
            json!({"sandbox":read}),
            json!({"approvalPolicy":"never"}),
        ] {
            check(value, Access::Configured, None);
        }
    }
}
