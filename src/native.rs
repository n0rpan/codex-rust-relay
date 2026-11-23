//! Run the installed clients' native sign-in and sessions without a relay.

use std::ffi::OsString;
use std::path::Path;
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
            Some(launch_chatgpt(rest, home).await)
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
    if !signed_in(client, home).await? {
        return Ok(None);
    }
    launch_subscription(client, home).await.map(Some)
}

pub(crate) async fn launch_subscription(client: ClientKind, home: &Path) -> Result<u8, RelayError> {
    let command = match client {
        ClientKind::Codex => codex_command(home)?,
        ClientKind::Claude => claude_account_command(home)?,
    };
    launch(command).await
}

pub(crate) async fn report_signed_in(home: &Path) -> Result<bool, RelayError> {
    let (codex, claude) = tokio::try_join!(
        signed_in(ClientKind::Codex, home),
        signed_in(ClientKind::Claude, home),
    )?;
    if !codex && !claude {
        return Ok(false);
    }
    println!("ox doctor (native client status)");
    println!("home: {}", home.display());
    println!("API gateway providers: 0");
    if codex {
        println!("Codex: signed in with ChatGPT. Start with `ox codex`.");
    }
    if claude {
        println!("Claude Code: signed in with Claude. Start with `ox claude`.");
    }
    println!(
        "To share API models between both clients, choose Add an API key provider in `ox setup`."
    );
    Ok(true)
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
    if signed_in(client, home).await? {
        let plan = if account == "chatgpt" {
            "ChatGPT"
        } else {
            "Claude"
        };
        println!("{name} is already signed in with {plan}.");
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
            if signed_in(ClientKind::Codex, home).await? {
                println!("Codex is signed in with ChatGPT.");
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
        (Some("claude"), []) => {
            let mut command = claude_account_command(home)?;
            command.args(["auth", "login", "--claudeai"]);
            launch(command).await
        }
        _ => Err("usage: ox login chatgpt [--device-auth | status], or ox login claude".into()),
    }
}

async fn launch_chatgpt(args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    if !signed_in(ClientKind::Codex, home).await? {
        return Err("no ChatGPT sign-in found. Run ox login chatgpt first".into());
    }
    let mut command = codex_command(home)?;
    command.args(args);
    launch(command).await
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
    let mut command = command("claude", home)?;
    command.args(args);
    launch(command).await
}

pub(crate) async fn signed_in(client: ClientKind, home: &Path) -> Result<bool, RelayError> {
    // Let the stock client access its own auth store. Consume status only.
    let mut command = match client {
        ClientKind::Codex => {
            let mut command = codex_command(home)?;
            command.args(["login", "status"]);
            command
        }
        ClientKind::Claude => {
            let mut command = claude_account_command(home)?;
            command.args(["auth", "status", "--json"]);
            command
        }
    };
    let output = match tokio::time::timeout(
        Duration::from_secs(5),
        command.stdin(Stdio::null()).output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Ok(_)) | Err(_) => return Ok(false),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Ok(Err(error)) => return Err(error.into()),
    };
    Ok(match client {
        ClientKind::Codex => [&output.stdout, &output.stderr].iter().any(|stream| {
            String::from_utf8_lossy(stream)
                .lines()
                .any(|line| line.trim() == "Logged in using ChatGPT")
        }),
        ClientKind::Claude => serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .is_ok_and(|status| {
                status["loggedIn"] == true
                    && status["authMethod"] == "claude.ai"
                    && status["apiProvider"] == "firstParty"
            }),
    })
}

fn claude_account_command(home: &Path) -> Result<Command, RelayError> {
    let mut command = command("claude", home)?;
    for name in crate::clients::ROUTING_ENV {
        command.env_remove(name);
    }
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

async fn launch(mut command: Command) -> Result<u8, RelayError> {
    let status = command.status().await.map_err(|error| {
        let client = command.as_std().get_program().to_string_lossy();
        crate::client_launch_error(&client, error)
    })?;
    Ok(status.code().unwrap_or(1) as u8)
}
