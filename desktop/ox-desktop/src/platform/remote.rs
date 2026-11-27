//! Desktop controls for the host's private Tailscale Serve route.
use super::*;
use ox_host_web::access;
pub use ox_host_web::access::{Operation, PermissionHelp};

pub struct Report {
    pub access: access::Report,
    pub guide: Vec<ox_desk_core::runtime::Runtime>,
}
impl From<access::Report> for Report {
    fn from(access: access::Report) -> Self {
        use ox_desk_core::runtime::Runtime;
        let guide = [Runtime::Codex, Runtime::Claude]
            .into_iter()
            .filter(|r| !onboarding::missing(*r))
            .collect();
        Self { access, guide }
    }
}
pub fn run(op: Operation) -> Result<Report, String> {
    let paths = ox_host_service::Paths::discover().map_err(|e| e.to_string())?;
    access::run(&paths, op).map(Report::from)
}
pub fn guidance(project: &Path, runtime: ox_desk_core::runtime::Runtime) -> Result<(), String> {
    const PROMPT: &str = "Help me install and sign into Tailscale on this computer and my phone using the official Tailscale instructions for my operating system. Explain each step and ask me before installing software or changing system settings. I want private tailnet HTTPS through Tailscale Serve, with MagicDNS and HTTPS enabled. Do not enable Funnel. Once I am signed in, guide me back to OX Desktop → Settings → Remote access.";
    let path = child_path();
    let program = find(runtime.executable(), &path)
        .ok_or_else(|| format!("Install {} and add it to PATH.", runtime.name()))?;
    open_program_in_terminal(project, program, vec![PROMPT.into()], &path)
}
