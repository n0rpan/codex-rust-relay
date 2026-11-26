use super::{child_path, ox};
use serde::Deserialize;
use std::process::{Command, Stdio};

fn output(args: &[&str]) -> Result<Vec<u8>, String> {
    output_with_stdin(args, Stdio::null())
}

fn output_with_stdin(args: &[&str], stdin: Stdio) -> Result<Vec<u8>, String> {
    let reply = Command::new(ox())
        .args(args)
        .env("PATH", child_path())
        .stdin(stdin)
        .output()
        .map_err(|e| format!("Could not start the companion ox executable: {e}"))?;
    if !reply.status.success() {
        let detail = String::from_utf8_lossy(&reply.stderr);
        return Err(if detail.trim().is_empty() {
            format!("ox {} failed ({}).", args.join(" "), reply.status)
        } else {
            detail.trim().to_owned()
        });
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

#[derive(Clone, Debug, Deserialize)]
pub struct Candidate {
    pub version: String,
    pub digest: String,
}

pub fn stage(background: bool) -> Result<Option<Candidate>, String> {
    let mut args = vec!["update", "--stage-desktop"];
    if background {
        args.push("--background");
    }
    serde_json::from_slice(&output(&args)?).map_err(|e| e.to_string())
}

pub fn apply(candidate: &Candidate) -> Result<(), String> {
    output(&[
        "update",
        "--apply-desktop",
        &candidate.version,
        &candidate.digest,
    ])
    .map(|_| ())
}

pub fn apply_if_idle(
    candidate: &Candidate,
    reservation: super::IdleReservation,
) -> Result<bool, String> {
    let mut args = vec![
        "update",
        "--try-apply-desktop",
        &candidate.version,
        &candidate.digest,
    ];
    let stdin = match reservation {
        super::IdleReservation::Running(socket) => Stdio::from(std::os::fd::OwnedFd::from(socket)),
        super::IdleReservation::Stopped(lock) => {
            args.push("--host-stopped");
            Stdio::from(lock)
        }
    };
    let reply = output_with_stdin(&args, stdin)?;
    match reply.as_slice() {
        b"" => Ok(true),
        b"busy\n" => Ok(false),
        _ => Err("Could not confirm whether the update was installed.".into()),
    }
}
