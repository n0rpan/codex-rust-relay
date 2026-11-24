use super::{child_path, ox};
use serde::Deserialize;
use std::process::{Command, Stdio};

fn output(args: &[&str]) -> Result<Vec<u8>, String> {
    let reply = Command::new(ox())
        .args(args)
        .env("PATH", child_path())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Could not start the companion ox executable: {e}"))?;
    if !reply.status.success() {
        return Err(format!("ox {} failed ({}).", args.join(" "), reply.status));
    }
    Ok(reply.stdout)
}

pub fn companion() -> Result<String, String> {
    let raw = output(&["--version"])?;
    let version = std::str::from_utf8(&raw)
        .ok()
        .and_then(|text| text.trim().strip_prefix("ox "))
        .filter(|v| !v.is_empty() && !v.contains(char::is_whitespace));
    version
        .map(str::to_owned)
        .ok_or_else(|| "Could not read the companion ox version.".into())
}

#[derive(Deserialize)]
pub struct Update {
    pub installed: String,
    pub available: Option<String>,
}

pub fn check() -> Result<Update, String> {
    serde_json::from_slice(&output(&["update", "--check", "--json"])?)
        .map_err(|_| "Could not read the update check from ox.".into())
}
