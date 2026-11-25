//! Configure a single tailnet-only Serve HTTPS port and the authenticated loopback listener.
use super::*;
use ox_host_web::admin::{self, Reply, Request};
use serde_json::Value;
use std::{
    io::Read,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

const PORT: u16 = 48162;
const PROXY: &str = "http://127.0.0.1:48162";
const HTTPS: &str = "8443";

#[derive(Clone)]
pub enum Operation {
    Status,
    Enable,
    Disable,
    Pair,
    Revoke(String),
    TerminalGrant(String, bool),
}
pub struct Report {
    pub host: Reply,
    pub network: Result<Node, String>,
    pub guide: Vec<ox_desk_core::runtime::Runtime>,
    pub denied: Option<PermissionHelp>,
    pub notice: Option<String>,
}
pub struct Node {
    pub origin: String,
    pub login: String,
    state: Config,
}
use ox_host_web::serve::Config;
impl Node {
    pub fn ready(&self) -> bool {
        self.state == Config::Owned
    }
    pub fn available(&self) -> bool {
        matches!(self.state, Config::Owned | Config::Free)
    }
    pub fn reason(&self) -> Option<&'static str> {
        match self.state {
            Config::Public => Some(
                "Funnel is enabled on HTTPS port 8443. Remove public sharing on that port in Tailscale before enabling OX.",
            ),
            Config::Conflict => Some(
                "HTTPS port 8443 already serves another application. Free that port in Tailscale to enable OX.",
            ),
            _ => None,
        }
    }
}

/// Enabling stops here, with the reason, when Serve status shows the OX port is
/// published through Funnel or taken by another application.
fn admit(node: &Node) -> Result<(), String> {
    node.reason().map_or(Ok(()), |reason| Err(reason.into()))
}

#[derive(Clone, Copy, PartialEq)]
enum ServeChange {
    Enable,
    Disable,
}

impl ServeChange {
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Enable => &["serve", "--bg", "--https=8443", PROXY],
            Self::Disable => &["serve", "--https=8443", "off"],
        }
    }
}

#[derive(Clone)]
pub struct PermissionHelp {
    pub error: String,
    origin: String,
    login: String,
    change: ServeChange,
    expected_enabled: bool,
}

impl PermissionHelp {
    fn denied(
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
            error,
            origin: node.origin.clone(),
            login: node.login.clone(),
            change,
            expected_enabled,
        })
    }
    pub fn pending_removal(&self) -> bool {
        self.change == ServeChange::Disable && self.expected_enabled
    }
    pub fn suggested(&self) -> String {
        format!("sudo tailscale {}", self.change.args().join(" "))
    }
    pub fn command(&self) -> crate::terminal::TerminalCommand {
        // sudo applies its own executable search policy to this fixed program name.
        let args = std::iter::once("tailscale")
            .chain(self.change.args().iter().copied())
            .map(OsString::from)
            .collect();
        crate::terminal::TerminalCommand {
            program: "/usr/bin/sudo".into(),
            args,
        }
    }
    fn check_mapping(&self, expected: Config) -> Result<(), String> {
        let node = inspect()?;
        if node.origin != self.origin || node.login != self.login || node.state != expected {
            return Err(node
                .reason()
                .unwrap_or("The Tailscale account or Serve mapping changed. Refresh Remote access.")
                .into());
        }
        Ok(())
    }
    pub fn prepare(&self) -> Result<admin::Reservation, String> {
        let paths = ox_host_service::Paths::discover().map_err(|e| e.to_string())?;
        match self.change {
            ServeChange::Enable => {
                self.check_mapping(Config::Free)?;
                let lease = admin::Reservation::bind(&paths.state_dir, self.origin.clone(), PORT)
                    .map_err(|e| e.to_string())?;
                self.check_mapping(Config::Free)?;
                Ok(lease)
            }
            ServeChange::Disable => {
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
                        ServeChange::Enable => Config::Owned,
                        ServeChange::Disable => Config::Free,
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
        let mut report = run(Operation::Status)?;
        report.notice = result.err();
        Ok(report)
    }
}

fn cli() -> Result<PathBuf, String> {
    let installed = find("tailscale", &child_path());
    #[cfg(target_os = "macos")]
    let installed = installed.or_else(|| {
        let path = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        executable(&path).then_some(path)
    });
    installed.ok_or_else(|| {
        "Install Tailscale on this computer and your phone, then sign in to the same tailnet."
            .into()
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
fn configuration(v: &Value, authority: &str) -> Config {
    ox_host_web::serve::configuration(v, authority, HTTPS, PROXY)
}
fn inspect() -> Result<Node, String> {
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
        .ok_or("Sign in to Tailscale under a user account to pair a phone.")?
        .to_owned();
    let authority = format!("{dns}:{HTTPS}");
    let raw = output(&["serve", "status", "--json"])?;
    let v: Value = serde_json::from_slice(&raw)
        .map_err(|_| "Tailscale returned an unreadable Serve configuration.")?;
    Ok(Node {
        origin: format!("https://{authority}"),
        login,
        state: configuration(&v, &authority),
    })
}
pub fn run(op: Operation) -> Result<Report, String> {
    let paths = ox_host_service::Paths::discover().map_err(|e| e.to_string())?;
    ox_host_service::ensure_running(&paths).map_err(|e| e.to_string())?;
    let request = |r: Request| admin::request(&paths.state_dir, &r).map_err(|e| e.to_string());
    let mut network = inspect();
    let until = Instant::now() + Duration::from_secs(2);
    let prior = loop {
        match admin::request(&paths.state_dir, &Request::Status) {
            Ok(status) => break status,
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
    };
    let mut denied = None;
    let host = match op {
        Operation::Status => prior,
        Operation::TerminalGrant(id, enabled) => request(Request::TerminalGrant { id, enabled })?,
        Operation::Revoke(id) => request(Request::Revoke { id })?,
        Operation::Pair => {
            let node = network.as_ref().map_err(Clone::clone)?;
            if !node.ready() || !prior.enabled || prior.origin.as_ref() != Some(&node.origin) {
                return Err("Enable tailnet access before pairing a device.".into());
            }
            request(Request::Pair {
                login: node.login.clone(),
            })?
        }
        Operation::Enable => {
            let node = network.as_ref().map_err(Clone::clone)?;
            admit(node)?;
            if node.ready() {
                request(Request::Enable {
                    origin: node.origin.clone(),
                    port: PORT,
                })?
            } else {
                // Retain exclusive ownership of the loopback port until the exact
                // Serve mapping is verified, including across an interactive sudo prompt.
                let lease = admin::Reservation::bind(&paths.state_dir, node.origin.clone(), PORT)
                    .map_err(|e| e.to_string())?;
                match output(ServeChange::Enable.args()) {
                    Ok(_) => {
                        let after = inspect()?;
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
                            ServeChange::Enable,
                            node,
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
                match output(ServeChange::Disable.args()) {
                    Ok(_) => {
                        let after = inspect()?;
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
                            ServeChange::Disable,
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
    use ox_desk_core::runtime::Runtime;
    let guide = [Runtime::Codex, Runtime::Claude]
        .into_iter()
        .filter(|r| !onboarding::missing(*r))
        .collect();
    Ok(Report {
        host,
        network,
        guide,
        denied,
        notice: None,
    })
}
pub fn guidance(project: &Path, runtime: ox_desk_core::runtime::Runtime) -> Result<(), String> {
    const PROMPT: &str = "Help me install and sign into Tailscale on this computer and my phone using the official Tailscale instructions for my operating system. Explain each step and ask me before installing software or changing system settings. I want private tailnet HTTPS through Tailscale Serve, with MagicDNS and HTTPS enabled. Do not enable Funnel. Once I am signed in, guide me back to OX Desktop → Settings → Remote access.";
    let path = child_path();
    let program = find(runtime.executable(), &path)
        .ok_or_else(|| format!("Install {} and add it to PATH.", runtime.name()))?;
    open_program_in_terminal(project, program, vec![PROMPT.into()], &path)
}

#[cfg(test)]
mod tests {
    use super::{Config, Node, PROXY, admit, configuration};
    use serde_json::{Value, json};
    #[test]
    fn funnel_in_serve_status_fixture_refuses_remote_access() {
        let cases: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/serve-status.json")).unwrap();
        let authority = "work.tail1234.ts.net:8443";
        let node = |case: &str| Node {
            origin: format!("https://{authority}"),
            login: "me@example.test".into(),
            state: configuration(&cases[case], authority),
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
        assert_eq!(configuration(&json!({}), key), Config::Free);
        assert_eq!(configuration(&owned, key), Config::Owned);
        let mut other = owned.clone();
        other["Web"][key]["Handlers"]["/"]["Proxy"] = "http://localhost:9999".into();
        assert_eq!(configuration(&other, key), Config::Conflict);
        let mut public = owned.clone();
        public["AllowFunnel"][key] = true.into();
        assert_eq!(configuration(&public, key), Config::Public);
        assert_eq!(
            configuration(&json!({"Foreground":{"transient":public}}), key),
            Config::Public
        );
        assert_eq!(
            configuration(&json!({"Foreground":{"transient":owned}}), key),
            Config::Conflict
        );
        assert_eq!(
            configuration(
                &json!({"AllowFunnel":{"unrelated:443":true},"TCP":{"443":{"HTTPS":true}}}),
                key
            ),
            Config::Free
        );
    }
}
