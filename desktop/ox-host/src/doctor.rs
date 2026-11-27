//! `ox host doctor`: one screen on the session host's health.
//!
//! Each check prints `ok`, `warn` or `FAIL` with the fix. Any FAIL is a
//! blocking problem and makes the command exit non-zero: the host is stopped,
//! remote access is on but Tailscale Serve does not route to the host or
//! Funnel exposes it, or a stock client is below OX's floor.
use crate::{control, durable::Ledger, handover, runtime::RuntimeConfig};
use ox_host_service::{Paths, errlog};
use ox_host_web::serve::Config;
use serde_json::Value;
use std::{
    io,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const GATE: &str = include_str!("../../gate.toml");
/// Tailscale Serve exposes OX on this HTTPS port and proxies to the host's
/// loopback listener (desktop/ox-desktop/src/platform/remote.rs).
const SERVE_PORT: &str = "8443";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Default)]
pub(crate) struct Report {
    lines: Vec<(Level, String)>,
}
impl Report {
    fn add(&mut self, level: Level, text: impl Into<String>) {
        self.lines.push((level, text.into()));
    }
    fn info(&mut self, text: impl Into<String>) {
        self.add(Level::Ok, text);
    }
    pub(crate) fn failures(&self) -> usize {
        self.lines.iter().filter(|(l, _)| *l == Level::Fail).count()
    }
    fn print(&self) {
        for (level, text) in &self.lines {
            let tag = match level {
                Level::Ok => "  ok  ",
                Level::Warn => "  warn",
                Level::Fail => "  FAIL",
            };
            println!("{tag} {text}");
        }
    }
}

pub(crate) fn run(paths: &Paths, args: &[String]) -> io::Result<()> {
    let errors = match args {
        [] => 10,
        [flag, n] if flag == "--errors" => n
            .parse()
            .map_err(|_| io::Error::other("--errors takes a number"))?,
        _ => return Err(io::Error::other("Usage: ox host doctor [--errors N]")),
    };
    let mut report = Report::default();
    let running = host(paths, &mut report);
    if running {
        remote(paths, &mut report, &Tailscale::find());
    } else {
        report.add(
            Level::Warn,
            "remote access: not checked while the host is stopped",
        );
    }
    stock_clients(&mut report, &search_path());
    routes(paths, &mut report);
    recent_errors(paths, &mut report, errors);
    report.print();
    match report.failures() {
        0 => {
            println!("\nOX host is healthy.");
            Ok(())
        }
        n => Err(io::Error::other(format!("{n} blocking problem(s)"))),
    }
}

fn host(paths: &Paths, report: &mut Report) -> bool {
    report.info(format!("socket: {}", paths.socket.display()));
    report.info(format!("state: {}", paths.state_dir.display()));
    match ox_host_service::service_installed() {
        Ok(true) => report.info("login service: installed"),
        Ok(false) => report.add(
            Level::Warn,
            "login service: not installed; the desktop starts the host on demand. Run `ox host install` to keep it running after login.",
        ),
        Err(e) => report.add(Level::Warn, format!("login service: {e}")),
    }
    if let Some(last) = handover::peek(&paths.state_dir) {
        report.info(format!(
            "last stop: {} of {} ({} running turn(s) interrupted)",
            last.reason, last.version, last.running
        ));
    }
    match control::probe(paths) {
        Ok(None) => {
            report.add(
                Level::Fail,
                "host: stopped. Start it with `ox desktop .` or `ox host install`.",
            );
            false
        }
        Err(e) => {
            report.add(
                Level::Fail,
                format!("host: not answering ({e}). Run `ox host restart`."),
            );
            false
        }
        Ok(Some(probe)) if probe.refused.is_some() => {
            report.add(
                Level::Fail,
                format!(
                    "host: running (pid {}) but refused this version: {}",
                    probe.pid,
                    probe.refused.as_deref().unwrap_or_default()
                ),
            );
            true
        }
        Ok(Some(probe)) => {
            match &probe.host {
                Some(info) => {
                    if let Some(id) = &info.id {
                        report.info(format!("host identity: {} ({id})", info.name.as_deref().unwrap_or("unnamed")));
                    }
                    report.info(format!(
                        "host: running, version {}, epoch {}, pid {}, protocol {} ({})",
                        info.version,
                        info.epoch,
                        probe.pid,
                        probe.version,
                        probe.capabilities.join(", ")
                    ));
                    if let Some(policy) = info.mode_policy {
                        report.info(format!("host mode policy: {}", policy.label()));
                    }
                    if !info.labels.is_empty() {
                        report.info(format!("host labels: {}", info.labels.join(", ")));
                    }
                    if let Some(r) = &info.resources {
                        report.info(format!("host resources: {} {}, {:.1} GiB available of {:.1} GiB, load {:.1}, {} active workers",
                            r.os, r.arch, r.available_memory as f64 / 1_073_741_824., r.total_memory as f64 / 1_073_741_824., r.load_one_minute, r.running_workers));
                    }
                    if info.version != handover::version() {
                        report.add(
                            Level::Warn,
                            format!(
                                "host: running {} but {} is installed. Run `ox host restart` to finish running turns and switch.",
                                info.version,
                                handover::version()
                            ),
                        );
                    }
                }
                None => report.add(
                    Level::Warn,
                    format!(
                        "host: running (pid {}) an older build without version reporting. Run `ox host restart`.",
                        probe.pid
                    ),
                ),
            }
            if !ox_desk_core::host_protocol::supported(probe.version) {
                report.add(
                    Level::Fail,
                    ox_desk_core::host_protocol::version_mismatch(
                        probe.version,
                        ox_desk_core::host_protocol::VERSION,
                    ),
                );
            }
            true
        }
    }
}

pub(crate) struct Tailscale {
    pub binary: Option<PathBuf>,
}
impl Tailscale {
    fn find() -> Self {
        let mut candidates: Vec<PathBuf> =
            search_path().iter().map(|p| p.join("tailscale")).collect();
        candidates.push("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into());
        Self {
            binary: candidates.into_iter().find(|p| p.is_file()),
        }
    }
    fn json(&self, args: &[&str]) -> Result<Value, String> {
        let binary = self
            .binary
            .as_ref()
            .ok_or("Tailscale is not installed or not on PATH")?;
        let out = Command::new(binary)
            .args(args)
            .output()
            .map_err(|e| format!("could not run tailscale: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        serde_json::from_slice(&out.stdout).map_err(|_| "unreadable tailscale output".into())
    }
}

/// What Tailscale Serve does with OX's HTTPS port, judged by the same rules
/// as the desktop's Remote access setup.
pub(crate) fn serve_state(v: &Value, authority: &str, port: u16) -> Config {
    ox_host_web::serve::configuration(
        v,
        authority,
        SERVE_PORT,
        &format!("http://127.0.0.1:{port}"),
    )
}

fn remote(paths: &Paths, report: &mut Report, tailscale: &Tailscale) {
    let status =
        match ox_host_web::admin::request(&paths.state_dir, &ox_host_web::admin::Request::Status) {
            Ok(s) => s,
            Err(e) => {
                report.add(
                    Level::Warn,
                    format!("remote access: no answer from the host ({e})"),
                );
                return;
            }
        };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    if status.devices.is_empty() {
        report.info("paired devices: none");
    }
    for d in &status.devices {
        report.info(format!(
            "paired device: {} ({}), last contact {}{}",
            d.label,
            d.login,
            ago(now, d.last_used),
            if d.terminal { ", terminal allowed" } else { "" }
        ));
    }
    for host in &status.worker_hosts {
        if let Some(grant) = &host.worker {
            report.info(format!(
                "worker host: {} ({}), launches {}, ceiling {}, memory sync {}, last contact {}",
                host.label,
                host.login,
                if grant.may_launch { "on" } else { "off" },
                grant.ceiling.name(),
                if grant.may_sync_memory { "on" } else { "off" },
                ago(now, host.last_used)
            ));
        }
    }
    if !status.enabled {
        report.info("remote access: off");
        return;
    }
    let origin = status.origin.clone().unwrap_or_default();
    let authority = origin.trim_start_matches("https://").to_owned();
    match tailscale.json(&["serve", "status", "--json"]) {
        Err(e) => report.add(
            Level::Fail,
            format!("remote access: on for {origin}, but Tailscale Serve could not be read: {e}"),
        ),
        Ok(v) => match serve_state(&v, &authority, status.port) {
            Config::Owned => report.info(format!(
                "remote access: {origin} → 127.0.0.1:{} (tailnet only)",
                status.port
            )),
            Config::Public => report.add(
                Level::Fail,
                format!(
                    "remote access: Funnel exposes port {SERVE_PORT} to the internet. OX refuses public access; run `tailscale funnel {SERVE_PORT} off`."
                ),
            ),
            Config::Free => report.add(
                Level::Fail,
                format!(
                    "remote access: on for {origin}, but Tailscale Serve is off. Turn Remote access off and on in Desktop settings."
                ),
            ),
            Config::Conflict => report.add(
                Level::Fail,
                format!(
                    "remote access: Tailscale Serve port {SERVE_PORT} does not route {origin} to the host. Turn Remote access off and on in Desktop settings."
                ),
            ),
        },
    }
}

fn ago(now: i64, then: i64) -> String {
    if then <= 0 {
        return "never".into();
    }
    match (now - then).max(0) {
        s if s < 120 => "just now".into(),
        s if s < 7200 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

fn search_path() -> Vec<PathBuf> {
    RuntimeConfig::discover()
        .map(|c| std::env::split_paths(&c.path).collect())
        .unwrap_or_default()
}

pub(crate) fn floor(client: &str) -> Option<String> {
    let mut section = "";
    for line in GATE.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
        } else if section == client
            && let Some(v) = line.strip_prefix("minimum_version")
        {
            return Some(
                v.trim_start_matches([' ', '='])
                    .trim()
                    .trim_matches('"')
                    .to_owned(),
            );
        }
    }
    None
}

fn parse_version(text: &str) -> Option<Vec<u64>> {
    text.split_whitespace().find_map(|w| {
        let parts: Option<Vec<u64>> = w.split('.').map(|p| p.parse().ok()).collect();
        parts.filter(|p| p.len() >= 2)
    })
}

pub(crate) fn below(version: &str, floor: &str) -> bool {
    match (parse_version(version), parse_version(floor)) {
        (Some(v), Some(f)) => v < f,
        _ => false,
    }
}

/// The update command for the channel that owns this Codex. All of them ship
/// the same native binary; `brew install` would leave an old cask in place.
pub(crate) fn codex_update(bin: &std::path::Path) -> &'static str {
    let real = bin.canonicalize().unwrap_or_else(|_| bin.to_path_buf());
    let has = |name: &str| real.components().any(|c| c.as_os_str() == name);
    if has("mise") {
        "mise use --global codex@latest"
    } else if has("Caskroom") || has("Cellar") {
        "brew upgrade --cask codex"
    } else {
        "npm install -g @openai/codex@latest"
    }
}

fn stock_clients(report: &mut Report, path: &[PathBuf]) {
    let mut found = 0;
    for (client, binary, install) in [
        ("codex", "codex", "npm install -g @openai/codex@latest"),
        ("claude", "claude", "claude update"),
    ] {
        let floor = floor(client).unwrap_or_default();
        let Some(bin) = path.iter().map(|p| p.join(binary)).find(|p| p.is_file()) else {
            report.add(
                Level::Warn,
                format!("{binary}: not found on PATH (OX needs {floor} or later to use it)"),
            );
            continue;
        };
        found += 1;
        let install = if client == "codex" {
            codex_update(&bin)
        } else {
            install
        };
        let version = Command::new(&bin)
            .arg("--version")
            .env("NO_COLOR", "1")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
        match version {
            None => report.add(
                Level::Fail,
                format!("{binary}: {} does not run", bin.display()),
            ),
            Some(v) if below(&v, &floor) => report.add(
                Level::Fail,
                format!("{binary}: {v} is below OX's floor {floor}. Update: {install}"),
            ),
            Some(v) => report.info(format!("{binary}: {v} (floor {floor})")),
        }
    }
    if found == 0 {
        report.add(
            Level::Fail,
            "stock clients: neither Codex nor Claude Code is installed",
        );
    }
}

fn routes(paths: &Paths, report: &mut Report) {
    let Ok(ledger) = Ledger::open(&paths.state_dir) else {
        report.add(Level::Warn, "routes: the session ledger could not be read");
        return;
    };
    if ledger.data.scopes.is_empty() {
        report.info("routes in use: none yet");
    }
    for scope in &ledger.data.scopes {
        report.info(format!(
            "route in use: {} in {} ({} session(s))",
            scope.scope.route.label(),
            scope.scope.project.display(),
            scope.sessions.len()
        ));
    }
}

fn recent_errors(paths: &Paths, report: &mut Report, n: usize) {
    let errors = errlog::tail(&paths.state_dir, n);
    if errors.is_empty() {
        report.info("host errors: none recorded");
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    report.info(format!("host errors: last {}", errors.len()));
    for (at, message) in errors {
        report.info(format!("  {}: {message}", ago(now, at as i64)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_updates_through_the_channel_that_installed_it() {
        use std::path::Path;
        let update = |p: &str| codex_update(Path::new(p));
        assert_eq!(
            update("/opt/homebrew/Caskroom/codex/0.170.0/codex"),
            "brew upgrade --cask codex"
        );
        assert_eq!(
            update("/home/a/.local/share/mise/installs/codex/0.170.0/codex"),
            "mise use --global codex@latest"
        );
        assert_eq!(
            update("/usr/lib/node_modules/@openai/codex/bin/codex.js"),
            "npm install -g @openai/codex@latest"
        );
    }

    #[test]
    fn floors_come_from_the_gate_and_versions_compare_numerically() {
        assert!(floor("codex").is_some() && floor("claude").is_some());
        assert!(below("codex-cli 0.99.0", "0.171.0"));
        assert!(!below("codex-cli 0.171.0", "0.171.0"));
        assert!(!below("2.1.400 (Claude Code)", "2.1.331"));
        assert!(below("2.1.9 (Claude Code)", "2.1.331"));
    }

    #[test]
    fn serve_status_is_routed_off_other_or_funnel() {
        let authority = "box.tail.ts.net:8443";
        let routed = json!({"TCP":{"8443":{"HTTPS":true}},"Web":{authority:{"Handlers":{"/":{"Proxy":"http://127.0.0.1:48162"}}}}});
        assert_eq!(serve_state(&routed, authority, 48162), Config::Owned);
        assert_eq!(serve_state(&json!({}), authority, 48162), Config::Free);
        assert_eq!(serve_state(&routed, authority, 1234), Config::Conflict);
        let mut funnel = routed.clone();
        funnel["AllowFunnel"] = json!({authority: true});
        assert_eq!(serve_state(&funnel, authority, 48162), Config::Public);
        let foreground = json!({"Foreground": {"session": funnel}});
        assert_eq!(serve_state(&foreground, authority, 48162), Config::Public);
    }

    #[test]
    fn a_stopped_host_and_an_old_stock_client_are_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            socket: dir.path().join("host.sock"),
            state_dir: dir.path().join("state"),
        };
        let mut report = Report::default();
        assert!(!host(&paths, &mut report));
        assert_eq!(report.failures(), 1);

        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        for (name, out) in [
            ("codex", "codex-cli 0.1.0"),
            ("claude", "9.9.9 (Claude Code)"),
        ] {
            let file = bin.join(name);
            std::fs::write(&file, format!("#!/bin/sh\necho '{out}'\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut report = Report::default();
        stock_clients(&mut report, &[bin]);
        assert_eq!(report.failures(), 1);
        assert!(
            report
                .lines
                .iter()
                .any(|(l, t)| *l == Level::Fail && t.contains("codex-cli 0.1.0"))
        );

        let mut report = Report::default();
        stock_clients(&mut report, &[]);
        assert_eq!(report.failures(), 1);
    }

    #[test]
    fn errors_are_listed_oldest_first_and_cut_to_the_last_n() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            socket: dir.path().join("s"),
            state_dir: dir.path().to_owned(),
        };
        let lines = |report: &Report| -> Vec<String> {
            report.lines.iter().map(|(_, t)| t.clone()).collect()
        };
        let mut report = Report::default();
        recent_errors(&paths, &mut report, 5);
        assert_eq!(lines(&report), ["host errors: none recorded"]);
        std::fs::write(
            dir.path().join(errlog::FILE),
            "100 first failure\n200 second failure\n300 third failure\n",
        )
        .unwrap();
        let mut report = Report::default();
        recent_errors(&paths, &mut report, 2);
        let shown = lines(&report);
        assert_eq!(shown[0], "host errors: last 2");
        assert!(shown[1].ends_with(": second failure"), "{shown:?}");
        assert!(shown[2].ends_with(": third failure"), "{shown:?}");
        assert_eq!(shown.len(), 3);
    }
}
