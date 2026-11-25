//! The one behavioural contract of a Claude Code worker, shared by the
//! standalone builder (`workers/jobs.rs`) and the hosted launch
//! (`ox-desk-core` `links/claude.rs`, which includes this file by path).
//! Tools, auto-allowed tools, permission mode, sandbox settings, the
//! no-sandbox fallback notice and the Workflow completion rule live only here.
//! It depends on `std` and `serde_json` alone.
use serde_json::{Value, json};

/// Message shown when a write worker starts without a shell.
pub const FALLBACK_NOTICE: &str = "Starting the stock client without a shell: Claude Code's sandbox needs bwrap and socat on Linux, so this write worker can edit files but not run commands.";

/// Claude Code's sandbox needs bwrap and socat on Linux. It is verified only
/// there, so other platforms fall back to edits without a shell.
pub fn sandbox_supported(os: &str, on_path: impl Fn(&str) -> bool) -> bool {
    os == "linux" && on_path("bwrap") && on_path("socat")
}

/// Whether an executable of this name is on PATH. A directory scan, no spawn.
pub fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// The settings document that makes Claude Code's own sandbox confine the
/// shell to the workspace. Bash is auto-allowed only inside it.
pub fn sandbox_document() -> Value {
    json!({"sandbox": {"enabled": true, "autoAllowBashIfSandboxed": true}})
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Contract {
    pub writes: bool,
    pub sandboxed: bool,
    pub workflow: bool,
}

impl Contract {
    /// The contract on this machine: a write worker is sandboxed when the host can.
    pub fn detect(writes: bool, workflow: bool) -> Self {
        Self::with_support(
            writes,
            workflow,
            sandbox_supported(std::env::consts::OS, on_path),
        )
    }
    pub fn with_support(writes: bool, workflow: bool, supported: bool) -> Self {
        Self {
            writes,
            sandboxed: writes && supported,
            workflow,
        }
    }
    /// `--tools`. Bash is offered only to a write worker whose sandbox runs.
    pub fn tools(&self) -> &'static str {
        match (self.writes, self.sandboxed) {
            (true, true) => "Read,Glob,Grep,Edit,Write,Bash,Workflow",
            (true, false) => "Read,Glob,Grep,Edit,Write,Workflow",
            (false, _) => "Read,Glob,Grep,Workflow",
        }
    }
    /// `--allowedTools`: what needs no prompt.
    pub fn allowed(&self) -> &'static str {
        match (self.writes, self.sandboxed, self.workflow) {
            (true, true, false) => "Read,Glob,Grep,Bash",
            (true, true, true) => "Read,Glob,Grep,Bash,Workflow",
            (true, false, true) | (false, _, true) => "Read,Glob,Grep,Workflow",
            (_, _, false) => "Read,Glob,Grep",
        }
    }
    /// `--disallowedTools`, only for a read worker.
    pub fn disallowed(&self) -> Option<&'static str> {
        (!self.writes).then_some("Bash,Edit,Write,MultiEdit,NotebookEdit")
    }
    /// `--permission-mode` of the standalone worker. A hosted worker starts in
    /// the same mode and the operator may change it afterwards.
    pub fn permission_mode(&self) -> &'static str {
        if self.writes {
            "acceptEdits"
        } else {
            "dontAsk"
        }
    }
    /// The `--settings` value, present only for a sandboxed write worker.
    pub fn settings(&self) -> Option<String> {
        self.sandboxed.then(|| sandbox_document().to_string())
    }
    /// Set when a write worker has no shell because the sandbox cannot run here.
    pub fn fallback_notice(&self) -> Option<&'static str> {
        (self.writes && !self.sandboxed).then_some(FALLBACK_NOTICE)
    }
    /// The policy arguments both paths pass, in order.
    pub fn args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(list) = self.disallowed() {
            out.extend(["--disallowedTools".into(), list.into()]);
        }
        out.extend([
            "--tools".into(),
            self.tools().into(),
            "--allowedTools".into(),
            self.allowed().into(),
        ]);
        if let Some(settings) = self.settings() {
            out.extend(["--settings".into(), settings]);
        }
        out
    }
    /// A Workflow worker completes only if it called the Workflow tool.
    pub fn check_workflow(&self, used_workflow: bool) -> Result<(), &'static str> {
        if self.workflow && !used_workflow {
            Err(WORKFLOW_NOT_RUN)
        } else {
            Ok(())
        }
    }
}

pub const WORKFLOW_NOT_RUN: &str = "Claude Code did not run the Workflow tool. Check the selected model, Claude Code version, and workflow permissions.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_matrix() {
        let w = Contract::with_support(true, false, true);
        assert_eq!(w.tools(), "Read,Glob,Grep,Edit,Write,Bash,Workflow");
        assert_eq!(w.allowed(), "Read,Glob,Grep,Bash");
        assert_eq!(w.permission_mode(), "acceptEdits");
        assert!(w.settings().is_some() && w.fallback_notice().is_none());
        let fallback = Contract::with_support(true, false, false);
        assert_eq!(fallback.tools(), "Read,Glob,Grep,Edit,Write,Workflow");
        assert_eq!(fallback.allowed(), "Read,Glob,Grep");
        assert!(fallback.settings().is_none() && fallback.fallback_notice().is_some());
        let read = Contract::with_support(false, false, true);
        assert_eq!(read.tools(), "Read,Glob,Grep,Workflow");
        assert_eq!(read.permission_mode(), "dontAsk");
        assert!(read.settings().is_none() && read.disallowed().is_some());
        assert!(read.fallback_notice().is_none());
        assert_eq!(
            Contract::with_support(true, true, true).allowed(),
            "Read,Glob,Grep,Bash,Workflow"
        );
        assert_eq!(
            Contract::with_support(false, true, true).allowed(),
            "Read,Glob,Grep,Workflow"
        );
    }

    #[test]
    fn workflow_completes_only_when_the_tool_ran() {
        let c = Contract::with_support(true, true, true);
        assert!(c.check_workflow(false).is_err());
        assert!(c.check_workflow(true).is_ok());
        assert!(
            Contract::with_support(true, false, true)
                .check_workflow(false)
                .is_ok()
        );
    }

    #[test]
    fn sandbox_needs_bwrap_and_socat_on_linux() {
        let with = |names: &'static [&'static str]| move |name: &str| names.contains(&name);
        assert!(sandbox_supported("linux", with(&["bwrap", "socat"])));
        assert!(!sandbox_supported("linux", with(&["bwrap"])));
        assert!(!sandbox_supported("macos", with(&["bwrap", "socat"])));
    }
}
