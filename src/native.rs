//! Run the installed clients' native sign-in and sessions without a relay.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

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

async fn login(account: Option<&str>, args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    match (account, args) {
        (Some("chatgpt"), [status]) if status == "status" => {
            if chatgpt_signed_in(home).await? {
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
            let mut command = command("claude", home)?;
            command.args(["auth", "login", "--claudeai"]);
            launch(command).await
        }
        _ => Err("usage: ox login chatgpt [--device-auth | status], or ox login claude".into()),
    }
}

async fn launch_chatgpt(args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    if !chatgpt_signed_in(home).await? {
        return Err("no ChatGPT sign-in found. Run ox login chatgpt first".into());
    }
    let mut command = codex_command(home)?;
    command.args(args);
    launch(command).await
}

async fn launch_claude(args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    let mut command = command("claude", home)?;
    command.args(args);
    launch(command).await
}

async fn chatgpt_signed_in(home: &Path) -> Result<bool, RelayError> {
    // Let Codex read its own file or keyring. Never capture tokens ourselves or
    // force an auth policy that could sign the user out of an existing account.
    let output = codex_command(home)?
        .args(["login", "status"])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| crate::client_launch_error("codex", error))?;
    Ok(output.status.success()
        && [&output.stdout, &output.stderr].iter().any(|stream| {
            String::from_utf8_lossy(stream)
                .lines()
                .any(|line| line.trim() == "Logged in using ChatGPT")
        }))
}

fn codex_command(home: &Path) -> Result<Command, RelayError> {
    let mut command = command("codex", home)?;
    command
        .env("CODEX_HOME", home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
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
