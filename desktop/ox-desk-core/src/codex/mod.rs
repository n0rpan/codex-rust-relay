//! Names and frames exchanged with the stock Codex app-server.
pub mod access;
pub mod activity;
pub mod ask;
pub mod frame;
pub mod history;
pub mod installed;
pub mod items;
pub mod names;
pub mod notice;
pub mod reply;
pub mod request;

pub mod commands;

pub mod version;

/// Installed schema methods needed to dispatch each runtime-owned action on Codex.
pub fn action_methods(operation: crate::actions::Operation) -> &'static [names::Method] {
    use crate::actions::Operation;
    use names::Method;
    match operation {
        Operation::OpenCatalogRow => &[Method::Read, Method::Resume],
        Operation::SelectThread => &[Method::Read, Method::Resume],
        Operation::RenameThread => &[Method::Rename],
        Operation::ArchiveThread => &[Method::Archive],
        Operation::ForkThread => &[Method::Fork],
        Operation::LoadOlder => &[Method::Turns],
        Operation::SearchThreads => &[Method::List],
        Operation::MoreThreads => &[Method::List],
        Operation::ExecuteCommand => &[
            Method::GetGoal,
            Method::ClearGoal,
            Method::SetGoal,
            Method::Settings,
            Method::Compact,
            Method::Rename,
            Method::Account,
            Method::ThreadStart,
        ],
        Operation::PaletteCommand => &[
            Method::GetGoal,
            Method::ClearGoal,
            Method::SetGoal,
            Method::Settings,
            Method::Compact,
            Method::Rename,
            Method::Account,
            Method::ThreadStart,
        ],
        Operation::Submit => &[
            Method::ThreadStart,
            Method::Account,
            Method::TurnStart,
            Method::Steer,
        ],
        Operation::Stop => &[Method::Interrupt],
        Operation::ModelOptions => &[Method::Settings, Method::TurnStart],
        Operation::ReadAccountLimits => &[Method::Limits],
        Operation::SetAccess => &[Method::Settings],
        Operation::ResumeHere => &[Method::Read, Method::Resume],
    }
}

/// Billing arguments shared by host execution and interactive terminal sessions.
pub fn launch_args(route: &crate::runtime::Route, provider: &str) -> Vec<String> {
    use crate::runtime::Billing;
    let mut args = vec!["codex".into()];
    match &route.billing {
        Billing::Subscription => {
            args.extend(["--chatgpt", "-c", "forced_login_method=\"chatgpt\""].map(String::from))
        }
        Billing::Api(model) => {
            args.extend([
                "--model".into(),
                model.clone(),
                "-c".into(),
                format!("model_provider={}", serde_json::json!(provider)),
            ]);
            if provider != "codeox" {
                args.extend([
                    "-c".into(),
                    format!(
                        "model_providers.{provider}.name={}",
                        serde_json::json!(provider)
                    ),
                ]);
            }
        }
    }
    args
}
