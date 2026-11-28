//! Typed companion operations for the local browser.
use std::{path::Path, process::Command};

fn parse_port(endpoint: &str) -> Option<u16> {
    let line = endpoint
        .strip_suffix("\r\n")
        .or_else(|| endpoint.strip_suffix('\n'))
        .unwrap_or(endpoint);
    let port = line.strip_prefix("http://127.0.0.1:")?;
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>().ok().filter(|port| *port != 0)
}

pub(crate) fn preview_port(project: &Path, label: &str) -> Result<u16, String> {
    let output = Command::new(super::ox())
        .args(["browser", "--session", label, "preview"])
        .current_dir(project)
        .output()
        .map_err(|error| format!("Could not find OX’s browser component: {error}"))?;
    match output.status.code() {
        Some(0) => std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(parse_port)
            .ok_or_else(|| "OX returned an invalid local browser address.".to_owned()),
        Some(2) => Err("Install the browser component with ox browser install.".into()),
        Some(3) => Err("Open this session’s browser and try again.".into()),
        _ => Err("Could not find the local browser session.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_port;

    #[test]
    fn accepts_only_the_companion_loopback_origin() {
        assert_eq!(parse_port("http://127.0.0.1:5678\n"), Some(5678));
        for value in [
            "http://127.0.0.1:0",
            "http://127.0.0.1:+5678",
            "http://127.0.0.1:5678/",
            "http://127.0.0.1:5678\n\n",
            "http://127.0.0.1:5678\nextra",
            "http://127.0.0.1:5678@external.test",
            "http://localhost:5678",
            "https://127.0.0.1:5678",
        ] {
            assert!(parse_port(value).is_none(), "{value}");
        }
    }
}
