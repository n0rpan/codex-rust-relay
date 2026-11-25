//! Run the installed clients' native sign-in and sessions without a relay.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::clients::ClientKind;
use crate::provider::{CodeoxConfig, DEFAULT_SEARCH_ENV_KEY, RelayError, TOKEN_ENV};

pub(crate) async fn dispatch(args: &[OsString], home: &Path) -> Option<Result<u8, RelayError>> {
    match args {
        [command, account, rest @ ..] if command == "login" => {
            Some(login(account.to_str(), rest, home).await)
        }
        [client, mode, rest @ ..] if client == "codex" && mode == "--chatgpt" => {
            Some(launch_chatgpt(rest, home, None).await)
        }
        [client, mode, args @ ..] if client == "claude" && mode == "--subscription" => {
            Some(launch_claude_subscription(args, home).await)
        }
        [client, mode, rest @ ..] if client == "claude" && mode == "--native" => {
            Some(launch_claude(rest, home).await)
        }
        _ => None,
    }
}

pub(crate) async fn launch_if_signed_in(
    client: ClientKind,
    home: &Path,
) -> Result<Option<u8>, RelayError> {
    match client {
        ClientKind::Codex if matches!(saved_chatgpt_account(home), Some(ChatGptAccount::File)) => {}
        ClientKind::Claude if has_saved_claude_account() => {}
        _ => return Ok(None),
    }
    launch_subscription(client, home, None).await.map(Some)
}

pub(crate) async fn launch_subscription(
    client: ClientKind,
    home: &Path,
    relay: Option<crate::workers::Relay>,
) -> Result<u8, RelayError> {
    match client {
        ClientKind::Codex => launch_chatgpt(&[], home, relay).await,
        // The same checked settings overlay as every other subscription launch, so
        // a user or project settings file cannot point the session at another origin.
        // No `auth status` probe here: choosing the subscription in the launcher starts
        // Claude Code directly, which shows its own sign-in when it needs one.
        ClientKind::Claude => {
            launch_session(
                checked_claude_command(home, false)?,
                &[],
                home,
                client,
                relay,
            )
            .await
        }
    }
}

pub(crate) fn report_saved_accounts(home: &Path) -> bool {
    let codex = saved_chatgpt_account(home);
    let claude = has_saved_claude_account();
    if codex.is_none() && !claude {
        return false;
    }
    println!("ox doctor (native client status)");
    println!("home: {}", home.display());
    println!("API gateway providers: 0");
    match codex {
        Some(ChatGptAccount::File) => {
            println!("Codex: saved ChatGPT sign-in found. Start with `ox codex`.")
        }
        Some(ChatGptAccount::Keyring) => println!(
            "Codex: keyring auth configured. Open `ox codex --chatgpt` to check the account."
        ),
        None => {}
    }
    if claude {
        println!("Claude Code: saved Claude account found. Start with `ox claude`.");
    }
    println!(
        "To share API models between both clients, choose Add an API key provider in `ox setup`."
    );
    true
}

pub(crate) async fn setup_subscription(account: &str, home: &Path) -> Result<u8, RelayError> {
    let (client, name, native, plain) = if account == "chatgpt" {
        (ClientKind::Codex, "Codex", "ox codex --chatgpt", "ox codex")
    } else {
        (
            ClientKind::Claude,
            "Claude Code",
            "ox claude --native",
            "ox claude",
        )
    };
    if has_saved_account(client, home) {
        if account == "chatgpt" {
            print_chatgpt_status(home);
        } else {
            println!("{name} has a saved Claude account. Run `ox login claude` to sign in again.");
        }
    } else {
        let code = login(Some(account), &[], home).await?;
        if code != 0 {
            return Ok(code);
        }
    }
    println!("Use {native} to start a subscription session.");
    println!("Without an ox API provider for this client, {plain} also uses your sign-in.");
    Ok(0)
}

async fn login(account: Option<&str>, args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    match (account, args) {
        (Some("chatgpt"), [status]) if status == "status" => {
            if print_chatgpt_status(home) {
                Ok(0)
            } else {
                eprintln!("ox: run ox login chatgpt to sign in with your ChatGPT account");
                Ok(1)
            }
        }
        (Some("chatgpt"), _)
            if args.is_empty() || matches!(args, [flag] if flag == "--device-auth") =>
        {
            let mut command = codex_command(home)?;
            command.arg("login").args(args);
            launch(command).await
        }
        (Some("claude"), [status]) if status == "status" => {
            check_claude_subscription(home).await?;
            println!("Claude Code is signed in with a Claude subscription.");
            Ok(0)
        }
        (Some("claude"), []) => {
            let mut command = claude_account_command(home)?;
            command.args(["auth", "login", "--claudeai"]);
            launch(command).await
        }
        _ => Err(
            "usage: ox login chatgpt [--device-auth | status], or ox login claude [status]".into(),
        ),
    }
}

// Exposed to the desktop as a stable, non-identifying failure marker.
const CLAUDE_SIGN_IN: &str = "no Claude subscription sign-in found. Run ox login claude first";

fn checked_claude_command(home: &Path, sandbox: bool) -> Result<Command, RelayError> {
    crate::clients::ensure_no_managed_override(&[])?;
    let mut command = claude_account_command(home)?;
    // Stock settings outrank its inherited environment. Preserve permissions,
    // hooks, and other user settings while keeping the selected account route.
    let mut env = serde_json::Map::new();
    for name in crate::clients::ROUTING_ENV
        .iter()
        .copied()
        .chain(["ANTHROPIC_PROFILE"])
    {
        env.insert(name.to_owned(), "".into());
    }
    let config = CodeoxConfig::path(home);
    if config.try_exists()? {
        for name in CodeoxConfig::load(&config)?.env_refs() {
            env.insert(name, "".into());
        }
    }
    // The optional OX worker MCP server sets its own short-lived TOKEN_ENV.
    for name in [crate::daemon::KEY_ENV, DEFAULT_SEARCH_ENV_KEY] {
        env.insert(name.into(), "".into());
    }
    let native_dir = crate::clients::claude_profiles(home)?.subscription;
    env.insert(
        "CLAUDE_CONFIG_DIR".into(),
        serde_json::to_value(&native_dir)?,
    );
    command.env("CLAUDE_CONFIG_DIR", native_dir);
    let mut settings = serde_json::json!({"env":env,"apiKeyHelper":""});
    if sandbox {
        settings["sandbox"] = crate::clients::claude_sandbox_document()["sandbox"].clone();
    }
    command.arg("--settings").arg(settings.to_string());
    Ok(command)
}

async fn check_claude_subscription(home: &Path) -> Result<(), RelayError> {
    let mut command = checked_claude_command(home, false)?;
    command
        .args(["auth", "status", "--json"])
        .stdin(Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| "Claude Code timed out checking the subscription")??;
    let account: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Claude Code did not return a readable account status")?;
    if output.status.success()
        && account["loggedIn"] == true
        && account["apiProvider"] == "firstParty"
        && account["authMethod"] == "claude.ai"
        && account["apiKeySource"].as_str().is_none_or(|s| s == "none")
    {
        return Ok(());
    }
    Err(CLAUDE_SIGN_IN.into())
}

async fn launch_claude_subscription(args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    launch_claude_subscription_with(args, home, None).await
}

/// Every Claude subscription launch, with or without further arguments, goes
/// through the checked account and settings overlay here.
async fn launch_claude_subscription_with(
    args: &[OsString],
    home: &Path,
    relay: Option<crate::workers::Relay>,
) -> Result<u8, RelayError> {
    if args.iter().any(|arg| {
        let name = arg.to_string_lossy();
        matches!(
            name.split('=').next(),
            Some("--managed-settings" | "--bare")
        )
    }) {
        return Err("subscription launches own their authentication settings".into());
    }
    // Only the exact sandbox document may ride along; it is merged into the
    // settings this launch builds, so it cannot replace the account settings.
    let (args, sandbox) = crate::clients::take_subscription_sandbox(args)?;
    check_claude_subscription(home).await?;
    launch_session(
        checked_claude_command(home, sandbox)?,
        &args,
        home,
        ClientKind::Claude,
        relay,
    )
    .await
}

/// The subscription command for a standalone worker, with the same checked
/// settings overlay (the worker's sandbox document is merged into it).
pub(crate) fn claude_subscription_worker_command(
    home: &Path,
    sandbox: bool,
) -> Result<Command, RelayError> {
    checked_claude_command(home, sandbox)
}

pub(crate) fn title_generation() -> bool {
    std::env::var_os("OX_TITLE_GENERATION").as_deref() == Some(std::ffi::OsStr::new("1"))
}

async fn launch_chatgpt(
    args: &[OsString],
    home: &Path,
    relay: Option<crate::workers::Relay>,
) -> Result<u8, RelayError> {
    let ready = match saved_chatgpt_account(home) {
        Some(ChatGptAccount::File) => true,
        Some(ChatGptAccount::Keyring) => check_chatgpt_keyring(home).await?,
        None => false,
    };
    if !ready {
        return Err("no ChatGPT sign-in found. Run ox login chatgpt first".into());
    }
    let mut command = codex_command(home)?;
    // A tool-free ephemeral exec ignores the user's general startup config, but it must
    // use the same ChatGPT account store and endpoint as the foreground stock client.
    if title_generation() {
        let config = home.join("config.toml");
        if config.try_exists()? {
            let text = std::fs::read_to_string(config)?;
            let doc = text.parse::<toml_edit::DocumentMut>()?;
            for key in ["cli_auth_credentials_store", "chatgpt_base_url"] {
                if let Some(v) = doc.get(key).and_then(toml_edit::Item::as_str) {
                    command
                        .arg("-c")
                        .arg(format!("{key}={}", serde_json::json!(v)));
                }
            }
        }
    }
    launch_session(command, args, home, ClientKind::Codex, relay).await
}

async fn check_chatgpt_keyring(home: &Path) -> Result<bool, RelayError> {
    // The user chose a native session. Let Codex resolve its own keyring
    // credential and confirm that it is a subscription before inference.
    let mut command = codex_command(home)?;
    command.args(["login", "status"]).stdin(Stdio::null());
    let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(5), command.output()).await
    else {
        return Ok(false);
    };
    Ok(output.status.success()
        && [&output.stdout, &output.stderr].iter().any(|stream| {
            String::from_utf8_lossy(stream)
                .lines()
                .any(|line| line.trim() == "Logged in using ChatGPT")
        }))
}

async fn launch_claude(args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    if [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_UNIX_SOCKET",
    ]
    .iter()
    .chain(
        crate::clients::ROUTING_ENV
            .iter()
            .filter(|name| name.starts_with("CLAUDE_CODE_USE_")),
    )
    .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        eprintln!(
            "ox: native Claude Code may use the API or gateway overrides in your shell. For subscription use, launch from a shell without those overrides."
        );
    }
    launch_session(
        command("claude", home)?,
        args,
        home,
        ClientKind::Claude,
        None,
    )
    .await
}

async fn launch_session(
    mut command: Command,
    args: &[OsString],
    home: &Path,
    client: ClientKind,
    relay: Option<crate::workers::Relay>,
) -> Result<u8, RelayError> {
    // A new Claude Code session gets an ID OX knows before the first turn, so
    // the worker gateway can record it. This applies with or without a host.
    let args = match client {
        ClientKind::Claude => crate::clients::claude_session_args(args.to_vec())?.0,
        ClientKind::Codex => args.to_vec(),
    };
    let args = args.as_slice();
    let relay = match relay {
        Some(relay) => Some(relay),
        None => crate::workers::Relay::load(home).await?,
    };
    match relay {
        Some(relay) => relay.launch(command, args, home, client).await,
        None => {
            command.args(args);
            launch(command).await
        }
    }
}

pub(crate) fn has_saved_account(client: ClientKind, home: &Path) -> bool {
    saved_subscription(client, home).is_some()
}

pub(crate) fn saved_subscription(client: ClientKind, home: &Path) -> Option<&'static str> {
    match client {
        ClientKind::Codex => match saved_chatgpt_account(home)? {
            ChatGptAccount::File => Some("saved_sign_in"),
            ChatGptAccount::Keyring => Some("configured_keyring"),
        },
        ClientKind::Claude => has_saved_claude_account().then_some("saved_sign_in"),
    }
}

enum ChatGptAccount {
    File,
    Keyring,
}

fn saved_chatgpt_account(home: &Path) -> Option<ChatGptAccount> {
    // Stock status commands can initialize or update files in the native
    // profile. Read only local markers and let the native client validate
    // credentials when the user chooses a subscription.
    let store = crate::setup::read_optional_text_limited(&home.join("config.toml"), "config.toml")
        .ok()
        .flatten()
        .and_then(|text| {
            let document = toml_edit::Document::parse(&text).ok()?;
            document
                .get("cli_auth_credentials_store")
                .and_then(toml_edit::Item::as_str)
                .map(str::to_owned)
        });
    match store.as_deref() {
        Some("keyring" | "auto") => return Some(ChatGptAccount::Keyring),
        Some("ephemeral") => return None,
        _ => {}
    }
    let auth = read_account_json(&home.join("auth.json"), "auth.json")?;
    let chatgpt = matches!(
        auth["auth_mode"].as_str(),
        None | Some("chatgpt" | "chatgptAuthTokens")
    );
    (chatgpt
        && auth["OPENAI_API_KEY"].is_null()
        && auth["tokens"]["access_token"]
            .as_str()
            .is_some_and(|token| !token.is_empty()))
    .then_some(ChatGptAccount::File)
}

fn print_chatgpt_status(home: &Path) -> bool {
    match saved_chatgpt_account(home) {
        Some(ChatGptAccount::File) => println!("Codex has a saved ChatGPT sign-in."),
        Some(ChatGptAccount::Keyring) => {
            println!("Codex uses keyring auth. Run `ox codex --chatgpt` to check the account.");
        }
        None => return false,
    }
    true
}

fn has_saved_claude_account() -> bool {
    // Even `claude auth status` writes its global config. Use the account
    // marker as a hint for the picker, leaving OAuth and expiry to Claude.
    crate::clients::claude_config_override()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .and_then(|dir| read_account_json(&dir.join(".claude.json"), ".claude.json"))
        .is_some_and(|config| {
            config["oauthAccount"]["accountUuid"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        })
}

fn read_account_json(path: &Path, file_name: &str) -> Option<serde_json::Value> {
    let text = crate::setup::read_optional_text_limited(path, file_name)
        .ok()
        .flatten()?;
    serde_json::from_str(&text).ok()
}

pub(crate) fn claude_account_command(home: &Path) -> Result<Command, RelayError> {
    let mut command = command("claude", home)?;
    for name in crate::clients::ROUTING_ENV {
        command.env_remove(name);
    }
    // A named Console profile takes precedence over Claude Code's saved login.
    command.env_remove("ANTHROPIC_PROFILE");
    Ok(command)
}

fn codex_command(home: &Path) -> Result<Command, RelayError> {
    let mut command = command("codex", home)?;
    command
        .env("CODEX_HOME", home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("OPENAI_API_BASE")
        .args(["-c", "model_provider=\"openai\""]);
    Ok(command)
}

fn command(client: &str, home: &Path) -> Result<Command, RelayError> {
    let mut command = Command::new(client);
    let path = CodeoxConfig::path(home);
    if path.try_exists()? {
        for name in CodeoxConfig::load(&path)?.env_refs() {
            command.env_remove(name);
        }
    }
    for name in [TOKEN_ENV, crate::daemon::KEY_ENV, DEFAULT_SEARCH_ENV_KEY] {
        command.env_remove(name);
    }
    command.kill_on_drop(true);
    Ok(command)
}

pub(crate) async fn launch(mut command: Command) -> Result<u8, RelayError> {
    crate::host_lifetime::isolate_stock(&mut command);
    let status = command.status().await.map_err(|error| {
        let client = command.as_std().get_program().to_string_lossy();
        crate::client_launch_error(&client, error)
    })?;
    Ok(status.code().unwrap_or(1) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every subscription launch carries one checked overlay that blanks the
    /// routing env, so user or project settings cannot choose another origin.
    #[test]
    fn subscription_worker_command_uses_the_checked_overlay() {
        let home = std::env::temp_dir().join("ox-native-overlay-test");
        for sandbox in [false, true] {
            let command = claude_subscription_worker_command(&home, sandbox).unwrap();
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
            let at = args.iter().position(|a| a == "--settings").unwrap();
            let settings: serde_json::Value = serde_json::from_str(&args[at + 1]).unwrap();
            assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "");
            assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "");
            assert_eq!(settings["apiKeyHelper"], "");
            assert_eq!(settings["sandbox"]["enabled"] == true, sandbox);
        }
    }
}
