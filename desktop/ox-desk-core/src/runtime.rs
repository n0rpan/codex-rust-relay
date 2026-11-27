//! Identity and user-facing types shared by native workbench runtimes.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    #[default]
    Codex,
    Claude,
}

impl Runtime {
    pub fn routes(aliases: &[String]) -> impl Iterator<Item = Route> + '_ {
        [Self::Codex, Self::Claude]
            .into_iter()
            .flat_map(move |runtime| {
                std::iter::once(runtime.route(None)).chain(
                    aliases
                        .iter()
                        .map(move |alias| runtime.route(Some(alias.clone()))),
                )
            })
    }
    pub fn route(self, alias: Option<String>) -> Route {
        Route {
            runtime: self,
            billing: alias.map_or(Billing::Subscription, Billing::Api),
        }
    }
    pub fn executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
    pub fn account(self) -> &'static str {
        match self {
            Self::Codex => "ChatGPT",
            Self::Claude => "Claude",
        }
    }
    pub fn install_guide(self) -> &'static str {
        match self {
            Self::Codex => "https://developers.openai.com/codex/cli/",
            Self::Claude => "https://code.claude.com/docs/en/setup",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    Subscription,
    Api(String),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Route {
    pub runtime: Runtime,
    pub billing: Billing,
}

/// The provider prefix identifies the account billed by an OX API model.
pub fn same_api_provider(active: &str, model: &str) -> bool {
    active == model
        || active
            .split_once('/')
            .zip(model.split_once('/'))
            .is_some_and(|((a, _), (b, _))| !a.is_empty() && a == b)
}

impl Route {
    pub const CODEX_SUBSCRIPTION: Self = Self {
        runtime: Runtime::Codex,
        billing: Billing::Subscription,
    };

    pub fn codex_api(alias: String) -> Self {
        Self {
            runtime: Runtime::Codex,
            billing: Billing::Api(alias),
        }
    }

    pub fn is_api(&self) -> bool {
        matches!(self.billing, Billing::Api(_))
    }

    pub fn label(&self) -> String {
        match (&self.billing, self.runtime) {
            (Billing::Subscription, Runtime::Codex) => "ChatGPT subscription".into(),
            (Billing::Subscription, Runtime::Claude) => "Claude subscription".into(),
            (Billing::Api(alias), Runtime::Codex) => format!("OX API · {alias}"),
            (Billing::Api(alias), Runtime::Claude) => format!("Claude Code · OX API · {alias}"),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    Text(String),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Access {
    ReadOnly,
    Workspace,
    AutoReview,
    Yolo,
    Configured,
    Default,
    Auto,
    AcceptEdits,
    Plan,
    BypassPermissions,
}

impl Access {
    pub fn choices(runtime: Runtime) -> &'static [Self] {
        match runtime {
            Runtime::Codex => &[
                Self::ReadOnly,
                Self::Workspace,
                Self::AutoReview,
                Self::Yolo,
            ],
            Runtime::Claude => &[
                Self::Default,
                Self::Auto,
                Self::AcceptEdits,
                Self::Plan,
                Self::BypassPermissions,
            ],
        }
    }
    pub fn bypasses_permissions(self) -> bool {
        matches!(self, Self::Yolo | Self::BypassPermissions)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Auto => "Auto",
            Self::AcceptEdits => "Accept edits",
            Self::Plan => "Plan mode",
            Self::BypassPermissions => "YOLO · bypass permissions",
            Self::ReadOnly => "Read only",
            Self::Workspace => "Workspace",
            Self::AutoReview => "Auto-review",
            Self::Yolo => "YOLO",
            Self::Configured => "Configured",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Default => "Use Claude Code’s default permission prompts.",
            Self::Auto => "Let Claude Code’s classifier review tool actions before they run.",
            Self::AcceptEdits => "Let Claude Code apply edits without asking about each edit.",
            Self::Plan => "Let Claude Code explore and plan before making changes.",
            Self::BypassPermissions => "Run Claude Code tools without permission prompts.",
            Self::ReadOnly => "Read files. Ask before writing or running outside the sandbox.",
            Self::Workspace => "Edit the workspace. Ask you before leaving the sandbox.",
            Self::AutoReview => "Edit the workspace. Let Codex review approval requests.",
            Self::Yolo => "Full filesystem and network access. Run commands without approval.",
            Self::Configured => "Use the active Codex permission settings.",
        }
    }
}

/// Stock CLI arguments, with the verified provider and native session identity.
pub fn terminal_args(route: &Route, provider: &str, thread: Option<&str>) -> Vec<String> {
    if route.runtime == Runtime::Claude {
        let mut args = vec!["claude".into()];
        match &route.billing {
            // No `--permission-mode`: the terminal starts in Claude Code's own default.
            Billing::Subscription => args.push("--subscription".into()),
            Billing::Api(alias) => args.extend(["--model".into(), alias.clone()]),
        }
        if let Some(id) = thread {
            args.extend(["--resume".into(), id.into()]);
        }
        return args;
    }
    let mut args = crate::codex::launch_args(route, provider);
    args.push("--no-daemon".into());
    if let Some(id) = thread {
        args.extend(["resume".into(), "--".into(), id.into()]);
    }
    args
}
