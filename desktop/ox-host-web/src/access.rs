//! Configure a single tailnet-only Serve HTTPS port and the authenticated loopback listener.
use crate::admin::{self, Reply, Request};
use ox_pty::child_path;
use serde_json::Value;
use std::{
    ffi::OsString,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};
use std::{
    io::Read,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

pub const DEFAULT_LOOPBACK_PORT: u16 = 48162;
pub const DEFAULT_HTTPS_PORT: u16 = 8443;
const PORT: u16 = DEFAULT_LOOPBACK_PORT;

pub fn validate_ports(
    https: Option<u16>,
    loopback: Option<u16>,
) -> Result<(Option<u16>, Option<u16>), String> {
    if let Some(p) = https
        && !crate::serve::valid_https_port(p)
    {
        return Err("--port takes an HTTPS port from 1024 to 65535.".into());
    }
    if let Some(p) = loopback
        && !crate::serve::valid_https_port(p)
    {
        return Err("--listen-port takes a loopback port from 1024 to 65535.".into());
    }
    if let (Some(h), Some(l)) = (https, loopback)
        && h == l
    {
        return Err("Choose different ports for Tailscale HTTPS (--port) and the local listener (--listen-port).".into());
    }
    Ok((https, loopback))
}

#[derive(Clone)]
pub enum Operation {
    Status,
    Enable,
    EnableOn {
        https_port: Option<u16>,
        loopback_port: Option<u16>,
    },
    Disable,
    Pair {
        terminal: bool,
    },
    PairWorker,
    Revoke(String),
    TerminalGrant(String, bool),
}
pub struct Report {
    pub host: Reply,
    pub network: Result<Node, String>,
    pub denied: Option<PermissionHelp>,
    pub notice: Option<String>,
}
#[derive(Clone)]
pub struct Node {
    pub origin: String,
    pub login: String,
    pub https_port: u16,
    pub loopback_port: u16,
    state: Config,
}
use crate::serve::Config;
impl Node {
    pub fn ready(&self) -> bool {
        self.state == Config::Owned
    }
    pub fn available(&self) -> bool {
        matches!(self.state, Config::Owned | Config::Free)
    }
    pub fn reason(&self) -> Option<String> {
        let port = self.https_port;
        match self.state {
            Config::Public => Some(format!(
                "Funnel is enabled on HTTPS port {port}. Remove public sharing on that port in Tailscale (`tailscale funnel --https={port} off`) or pick another port with `ox host pair --port <port>` before enabling OX."
            )),
            Config::Conflict => Some(format!(
                "HTTPS port {port} already serves another application. Run `ox host pair --port <port>` to use another port, or free port {port} in Tailscale (`tailscale serve --https={port} off`)."
            )),
            _ => None,
        }
    }
}

/// Enabling stops here, with the reason, when Serve status shows the OX port is
/// published through Funnel or taken by another application.
fn admit(node: &Node) -> Result<(), String> {
    node.reason().map_or(Ok(()), Err)
}

#[derive(Clone, Copy, PartialEq)]
enum ServeChange {
    Enable { https_port: u16, loopback_port: u16 },
    Disable { https_port: u16, loopback_port: u16 },
}

impl ServeChange {
    fn args(self) -> Vec<String> {
        match self {
            Self::Enable {
                https_port,
                loopback_port,
            } => vec![
                "serve".into(),
                "--bg".into(),
                format!("--https={https_port}"),
                format!("http://127.0.0.1:{loopback_port}"),
            ],
            Self::Disable { https_port, .. } => {
                vec![
                    "serve".into(),
                    format!("--https={https_port}"),
                    "off".into(),
                ]
            }
        }
    }
    fn https_port(self) -> u16 {
        match self {
            Self::Enable { https_port, .. } | Self::Disable { https_port, .. } => https_port,
        }
    }
    fn loopback_port(self) -> u16 {
        match self {
            Self::Enable { loopback_port, .. } | Self::Disable { loopback_port, .. } => {
                loopback_port
            }
        }
    }
}

#[derive(Clone)]
pub struct PermissionHelp {
    paths: ox_host_service::Paths,
    pub error: String,
    origin: String,
    login: String,
    change: ServeChange,
    expected_enabled: bool,
}

impl PermissionHelp {
    fn denied(
        paths: &ox_host_service::Paths,
        change: ServeChange,
        node: &Node,
        expected_enabled: bool,
        error: String,
    ) -> Result<Self, String> {
        let msg = error.to_ascii_lowercase();
        if ![
            "access denied",
            "permission denied",
            "operation not permitted",
            "requires root",
        ]
        .iter()
        .any(|word| msg.contains(word))
        {
            return Err(error);
        }
        Ok(Self {
            paths: paths.clone(),
            error,
            origin: node.origin.clone(),
            login: node.login.clone(),
            change,
            expected_enabled,
        })
    }
    pub fn pending_removal(&self) -> bool {
        matches!(self.change, ServeChange::Disable { .. }) && self.expected_enabled
    }
    pub fn suggested(&self) -> String {
        format!("sudo tailscale {}", self.change.args().join(" "))
    }
    pub fn args(&self) -> Vec<OsString> {
        std::iter::once("tailscale".to_owned())
            .chain(self.change.args())
            .map(OsString::from)
            .collect()
    }
    fn check_mapping(&self, expected: Config) -> Result<(), String> {
        let node = inspect_ports(self.change.https_port(), self.change.loopback_port())?;
        if node.origin != self.origin || node.login != self.login || node.state != expected {
            return Err(node.reason().unwrap_or_else(|| {
                "The Tailscale account or Serve mapping changed. Refresh Remote access.".into()
            }));
        }
        Ok(())
    }
    pub fn prepare(&self) -> Result<admin::Reservation, String> {
        let paths = &self.paths;
        match self.change {
            ServeChange::Enable { loopback_port, .. } => {
                let node = inspect_ports(self.change.https_port(), loopback_port)?;
                if node.origin != self.origin || node.login != self.login {
                    return Err(
                        "The Tailscale account or Serve mapping changed. Refresh Remote access."
                            .into(),
                    );
                }
                let prior = status(paths)?;
                let prior_loopback = (prior.port != 0)
                    .then_some(prior.port)
                    .unwrap_or(DEFAULT_LOOPBACK_PORT);
                let owned_prior = prior_loopback != loopback_port
                    && inspect_ports(self.change.https_port(), prior_loopback)
                        .is_ok_and(|n| n.ready());
                if node.state != Config::Free && !owned_prior {
                    return Err(node.reason().unwrap_or_else(|| {
                        "The Tailscale account or Serve mapping changed. Refresh Remote access."
                            .into()
                    }));
                }
                let lease =
                    admin::Reservation::bind(&paths.state_dir, self.origin.clone(), loopback_port)
                        .map_err(|e| e.to_string())?;
                let after = inspect_ports(self.change.https_port(), loopback_port)?;
                if after.origin != self.origin
                    || after.login != self.login
                    || (after.state != Config::Free && !owned_prior)
                {
                    return Err(after.reason().unwrap_or_else(|| {
                        "The Tailscale account or Serve mapping changed. Refresh Remote access."
                            .into()
                    }));
                }
                Ok(lease)
            }
            ServeChange::Disable { .. } => {
                self.check_mapping(Config::Owned)?;
                let lease = admin::Reservation::removal(
                    &paths.state_dir,
                    self.origin.clone(),
                    self.expected_enabled,
                )
                .map_err(|e| e.to_string())?;
                self.check_mapping(Config::Owned)?;
                Ok(lease)
            }
        }
    }
    pub fn finish(
        &self,
        status: Option<i32>,
        lease: Option<admin::Reservation>,
    ) -> Result<Report, String> {
        let result: Result<(), String> = (|| {
            match status {
                Some(0) => {
                    self.check_mapping(match self.change {
                        ServeChange::Enable { .. } => Config::Owned,
                        ServeChange::Disable { .. } => Config::Free,
                    })?;
                    lease
                        .ok_or("The host reservation expired. Refresh Remote access.")?
                        .commit()
                        .map_err(|e| e.to_string())?;
                }
                Some(code) => {
                    return Err(format!(
                        "The terminal command exited with status {code}. Refresh to check Tailscale."
                    ));
                }
                None => {
                    return Err(
                        "Setup was cancelled or could not start. Check the terminal for details."
                            .into(),
                    );
                }
            }
            Ok(())
        })();
        let paths = &self.paths;
        let mut report = run(paths, Operation::Status)?;
        report.notice = result.err();
        Ok(report)
    }
}

fn cli() -> Result<PathBuf, String> {
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let installed = std::env::split_paths(&child_path())
        .filter(|p| p.is_absolute())
        .map(|dir| dir.join("tailscale"))
        .find(|p| executable(p));
    #[cfg(target_os = "macos")]
    let installed = installed.or_else(|| {
        let path = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        executable(&path).then_some(path)
    });
    installed.ok_or_else(|| {
        "Install Tailscale on both devices, then sign in to the same tailnet.".into()
    })
}
fn output(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut child = Command::new(cli()?)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let drain = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut output = Vec::new();
            let _ = pipe.by_ref().take(2 * 1024 * 1024).read_to_end(&mut output);
            let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            output
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let until = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            break child.wait().map_err(|e| e.to_string())?;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stderr = stderr.join().unwrap_or_default();
    let stdout = stdout.join().unwrap_or_default();
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr);
        let message = message.trim();
        return Err(if message.is_empty() {
            "Tailscale did not finish this request. Open Tailscale, check HTTPS permissions, and refresh.".into()
        } else {
            message.chars().take(8192).collect()
        });
    }
    Ok(stdout)
}
fn configuration(v: &Value, authority: &str, https_port: u16, loopback_port: u16) -> Config {
    crate::serve::configuration(
        v,
        authority,
        &https_port.to_string(),
        &format!("http://127.0.0.1:{loopback_port}"),
    )
}
/// A browser can select a new key recipient without someone at the source console.
#[cfg(feature = "server")]
pub(crate) fn verify_key_destination(origin: &str, login: &str) -> Result<(), &'static str> {
    let status = output(&["status", "--json"])
        .map_err(|_| "Could not check the destination’s owner with Tailscale.")?;
    let status: Value = serde_json::from_slice(&status)
        .map_err(|_| "Tailscale returned an unreadable peer list.")?;
    let url = url::Url::parse(origin).map_err(|_| "Invalid destination address.")?;
    let name = url.host_str().ok_or("Invalid destination address.")?;
    let owner = &status["Self"]["UserID"];
    let id = owner
        .as_u64()
        .map(|id| id.to_string())
        .or_else(|| owner.as_str().map(str::to_owned));
    let same_login = id.is_some_and(|id| status["User"][id]["LoginName"].as_str() == Some(login));
    let owned = status["Peer"]
        .as_object()
        .into_iter()
        .flat_map(|peers| peers.values())
        .any(|peer| {
            peer["DNSName"]
                .as_str()
                .is_some_and(|dns| dns.trim_end_matches('.').eq_ignore_ascii_case(name))
                && peer["UserID"] == *owner
                && peer["Tags"].as_array().is_none_or(|tags| tags.is_empty())
        });
    if status["BackendState"] == "Running" && same_login && owned {
        Ok(())
    } else {
        Err("Choose a destination owned by your Tailscale login, or pair it in OX Desktop.")
    }
}

fn inspect_ports(https_port: u16, loopback_port: u16) -> Result<Node, String> {
    let v: Value = serde_json::from_slice(&output(&["status", "--json", "--peers=false"])?)
        .map_err(|_| "Tailscale returned an unreadable status.")?;
    if v["BackendState"] != "Running" {
        return Err("Open Tailscale and sign in to your tailnet, then refresh.".into());
    }
    let dns = v["Self"]["DNSName"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches('.');
    if dns.is_empty()
        || !dns
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
    {
        return Err("Enable MagicDNS and HTTPS in the tailnet DNS settings, then refresh.".into());
    }
    let id = v["Self"]["UserID"]
        .as_u64()
        .map(|i| i.to_string())
        .or_else(|| v["Self"]["UserID"].as_str().map(str::to_owned))
        .ok_or("Tailscale did not provide the local user identity.")?;
    let login = v["User"][&id]["LoginName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Sign in to Tailscale under a user account to pair a device.")?
        .to_owned();
    let authority = format!("{dns}:{https_port}");
    let raw = output(&["serve", "status", "--json"])?;
    let v: Value = serde_json::from_slice(&raw)
        .map_err(|_| "Tailscale returned an unreadable Serve configuration.")?;
    Ok(Node {
        origin: format!("https://{authority}"),
        login,
        https_port,
        loopback_port,
        state: configuration(&v, &authority, https_port, loopback_port),
    })
}
pub fn status(paths: &ox_host_service::Paths) -> Result<Reply, String> {
    ox_host_service::ensure_running(paths).map_err(|e| e.to_string())?;
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match admin::request(&paths.state_dir, &Request::Status) {
            Ok(mut status) => {
                status.devices.append(&mut status.worker_hosts);
                return Ok(status);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) && Instant::now() < until =>
            {
                thread::sleep(Duration::from_millis(25))
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}
pub fn run(paths: &ox_host_service::Paths, op: Operation) -> Result<Report, String> {
    let request = |r: Request| admin::request(&paths.state_dir, &r).map_err(|e| e.to_string());
    let mut prior = status(paths)?;
    let (requested_https, requested_loopback) = match &op {
        Operation::EnableOn {
            https_port,
            loopback_port,
        } => validate_ports(*https_port, *loopback_port)?,
        _ => (None, None),
    };
    let https_port = requested_https
        .or(prior.https_port)
        .or_else(|| prior.origin.as_deref().and_then(crate::serve::https_port))
        .unwrap_or(DEFAULT_HTTPS_PORT);
    let loopback_port = requested_loopback
        .or_else(|| (prior.port != 0).then_some(prior.port))
        .unwrap_or(DEFAULT_LOOPBACK_PORT);
    validate_ports(Some(https_port), Some(loopback_port))?;
    let mut network = inspect_ports(https_port, loopback_port);
    let mut denied = None;
    let host = match op {
        Operation::Status => prior,
        Operation::TerminalGrant(id, enabled) => request(Request::TerminalGrant { id, enabled })?,
        Operation::Revoke(id) => request(Request::Revoke { id })?,
        Operation::Pair { .. } | Operation::PairWorker => {
            let node = network.as_ref().map_err(Clone::clone)?;
            if !node.ready() || !prior.enabled || prior.origin.as_ref() != Some(&node.origin) {
                return Err("Enable tailnet access before pairing a device.".into());
            }
            let login = node.login.clone();
            if let Operation::Pair { terminal } = op {
                request(Request::Pair { terminal, login })?
            } else {
                request(Request::WorkerInvitation { login })?
            }
        }
        Operation::Enable | Operation::EnableOn { .. } => {
            let prior_loopback = (prior.port != 0)
                .then_some(prior.port)
                .unwrap_or(DEFAULT_LOOPBACK_PORT);
            let mut node = network.as_ref().map_err(Clone::clone)?.clone();
            if node.state == Config::Conflict
                && prior_loopback != loopback_port
                && inspect_ports(https_port, prior_loopback).is_ok_and(|n| n.ready())
            {
                // The HTTPS port is already owned by OX's previous loopback port.
                // Treat it as available to update to the new loopback port.
                node.state = Config::Free;
            }
            admit(&node)?;
            if prior.enabled
                && (prior.origin.as_ref() != Some(&node.origin) || prior.port != loopback_port)
            {
                if let Some(old_https) = prior
                    .https_port
                    .or_else(|| prior.origin.as_deref().and_then(crate::serve::https_port))
                    && old_https != https_port
                {
                    let old_loopback = if prior.port != 0 { prior.port } else { PORT };
                    if let Ok(old_node) = inspect_ports(old_https, old_loopback)
                        && old_node.ready()
                    {
                        let args = ServeChange::Disable {
                            https_port: old_https,
                            loopback_port: old_loopback,
                        }
                        .args();
                        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                        let _ = output(&refs);
                    }
                }
                prior = request(Request::Disable)?;
            }
            if node.ready() {
                request(Request::Enable {
                    origin: node.origin.clone(),
                    port: loopback_port,
                })?
            } else {
                // Retain exclusive ownership of the loopback port until the exact
                // Serve mapping is verified, including across an interactive sudo prompt.
                let lease =
                    admin::Reservation::bind(&paths.state_dir, node.origin.clone(), loopback_port)
                        .map_err(|e| e.to_string())?;
                let change = ServeChange::Enable {
                    https_port,
                    loopback_port,
                };
                let args = change.args();
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                match output(&refs) {
                    Ok(_) => {
                        let after = inspect_ports(https_port, loopback_port)?;
                        if !after.ready()
                            || node.origin != after.origin
                            || node.login != after.login
                        {
                            return Err("Tailscale did not confirm the private OX mapping.".into());
                        }
                        network = Ok(after);
                        lease.commit().map_err(|e| e.to_string())?
                    }
                    Err(e) => {
                        denied = Some(PermissionHelp::denied(
                            paths,
                            change,
                            &node,
                            prior.enabled,
                            e,
                        )?);
                        prior
                    }
                }
            }
        }
        Operation::Disable => {
            if let Ok(node) = &network
                && node.ready()
                && prior
                    .origin
                    .as_ref()
                    .is_none_or(|origin| origin == &node.origin)
            {
                let lease = admin::Reservation::removal(
                    &paths.state_dir,
                    node.origin.clone(),
                    prior.enabled,
                )
                .map_err(|e| e.to_string())?;
                let change = ServeChange::Disable {
                    https_port,
                    loopback_port,
                };
                let args = change.args();
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                match output(&refs) {
                    Ok(_) => {
                        let after = inspect_ports(https_port, loopback_port)?;
                        if after.state != Config::Free
                            || after.origin != node.origin
                            || after.login != node.login
                        {
                            return Err(
                                "Tailscale did not confirm removal of the OX mapping.".into()
                            );
                        }
                        network = Ok(after);
                        lease.commit().map_err(|e| e.to_string())?
                    }
                    Err(e) => {
                        denied = Some(PermissionHelp::denied(
                            paths,
                            change,
                            node,
                            prior.enabled,
                            e,
                        )?);
                        prior
                    }
                }
            } else {
                request(Request::Disable)?
            }
        }
    };
    Ok(Report {
        host,
        network,
        denied,
        notice: None,
    })
}
#[cfg(test)]
mod tests {
    use super::{
        Config, DEFAULT_HTTPS_PORT, DEFAULT_LOOPBACK_PORT, Node, admit, configuration,
        validate_ports,
    };
    use serde_json::{Value, json};
    const PROXY: &str = "http://127.0.0.1:48162";
    #[test]
    fn funnel_in_serve_status_fixture_refuses_remote_access() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../ox-desktop/tests/fixtures/serve-status.json"
        ))
        .unwrap();
        let authority = "work.tail1234.ts.net:8443";
        let node = |case: &str| Node {
            origin: format!("https://{authority}"),
            login: "me@example.test".into(),
            https_port: DEFAULT_HTTPS_PORT,
            loopback_port: DEFAULT_LOOPBACK_PORT,
            state: configuration(
                &cases[case],
                authority,
                DEFAULT_HTTPS_PORT,
                DEFAULT_LOOPBACK_PORT,
            ),
        };
        let private = node("private");
        assert!(private.ready());
        assert_eq!(admit(&private), Ok(()));
        for case in ["funnel", "foreground_funnel"] {
            let public = node(case);
            assert!(!public.ready() && !public.available(), "{case}");
            let refused = admit(&public).unwrap_err();
            assert!(refused.contains("Funnel"), "{case}: {refused}");
        }
        // Funnel on an unrelated port does not block the private OX port.
        let elsewhere = node("funnel_elsewhere");
        assert_eq!(admit(&elsewhere), Ok(()));
        assert!(elsewhere.available());
    }
    #[test]
    fn sharing_never_overwrites_a_port_or_publishes_to_funnel() {
        let key = "work.tail.ts.net:8443";
        let owned =
            json!({"TCP":{"8443":{"HTTPS":true}},"Web":{(key):{"Handlers":{"/":{"Proxy":PROXY}}}}});
        let cfg = |v: &Value| configuration(v, key, DEFAULT_HTTPS_PORT, DEFAULT_LOOPBACK_PORT);
        assert_eq!(cfg(&json!({})), Config::Free);
        assert_eq!(cfg(&owned), Config::Owned);
        let mut other = owned.clone();
        other["Web"][key]["Handlers"]["/"]["Proxy"] = "http://localhost:9999".into();
        assert_eq!(cfg(&other), Config::Conflict);
        let mut public = owned.clone();
        public["AllowFunnel"][key] = true.into();
        assert_eq!(cfg(&public), Config::Public);
        assert_eq!(
            cfg(&json!({"Foreground":{"transient":public}})),
            Config::Public
        );
        assert_eq!(
            cfg(&json!({"Foreground":{"transient":owned}})),
            Config::Conflict
        );
        assert_eq!(
            cfg(&json!({"AllowFunnel":{"unrelated:443":true},"TCP":{"443":{"HTTPS":true}}})),
            Config::Free
        );
        let custom_key = "work.tail.ts.net:9443";
        let custom_owned = json!({"TCP":{"9443":{"HTTPS":true}},"Web":{(custom_key):{"Handlers":{"/":{"Proxy":"http://127.0.0.1:49000"}}}}});
        assert_eq!(
            configuration(&custom_owned, custom_key, 9443, 49000),
            Config::Owned
        );
        assert!(validate_ports(Some(9443), Some(49000)).is_ok());
        assert!(validate_ports(Some(443), None).is_err());
        assert!(validate_ports(None, Some(80)).is_err());
        assert!(validate_ports(Some(8443), Some(8443)).is_err());
    }
}
