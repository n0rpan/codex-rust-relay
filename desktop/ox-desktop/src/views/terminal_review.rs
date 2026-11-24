//! A reviewed tool command runs in a local terminal only after the user chooses Run.
//! The host can confirm a denial, but cannot supply an executable or terminal input.
use super::workbench::Workbench;
use crate::terminal::{self, TerminalCommand};
use gpui::*;
use ox_desk_core::{
    host_protocol::Server,
    links::LinkEvent,
    runtime::RequestId,
    transcript::{Entry, ItemKey, item::Item},
};
use std::path::{Path, PathBuf};

/// Accept one literal argv invocation. Leave shell expansions, pipelines and multi-step
/// scripts to the user in a blank terminal: replaying them could repeat earlier effects.
fn words(script: &str) -> Option<Vec<String>> {
    let mut quoted = None;
    let mut escaped = false;
    for c in script.chars() {
        if c.is_control() {
            return None;
        }
        if escaped {
            escaped = false;
        } else if c == '\\' && quoted != Some('\'') {
            escaped = true;
        } else if Some(c) == quoted {
            quoted = None;
        } else if quoted.is_none() && matches!(c, '\'' | '"') {
            quoted = Some(c);
        } else if (quoted != Some('\'') && matches!(c, '$' | '`'))
            || (quoted.is_none() && ";&|<>*?()[]{}~#".contains(c))
        {
            return None;
        }
    }
    if quoted.is_some() || escaped {
        return None;
    }
    shlex::split(script).filter(|argv| !argv.is_empty())
}

fn invocation(script: &str) -> Option<Vec<String>> {
    let argv = words(script)?;
    let name = Path::new(&argv[0]).file_name()?.to_str()?;
    if matches!(name, "bash" | "zsh" | "sh" | "fish") {
        let [_, option, body] = argv.as_slice() else {
            return None;
        };
        if !matches!(option.as_str(), "-c" | "-lc") {
            return None;
        }
        return words(body);
    }
    Some(argv)
}

fn interactive_sudo(argv: &[String]) -> bool {
    if !argv
        .first()
        .is_some_and(|p| Path::new(p).file_name().is_some_and(|n| n == "sudo"))
    {
        return false;
    }
    let mut args = argv[1..].iter();
    let mut standalone = false;
    while let Some(word) = args.next() {
        if word == "--" {
            return args.next().is_some() || standalone;
        }
        if !word.starts_with('-') {
            return true;
        }
        if let Some(option) = word.strip_prefix("--") {
            let (name, value) = option
                .split_once('=')
                .map_or((option, None), |(k, v)| (k, Some(v)));
            match name {
                "non-interactive" | "stdin" | "askpass" | "help" | "version"
                | "remove-timestamp" => return false,
                "validate" | "list" | "shell" | "login" => standalone = true,
                "user" | "group" | "prompt" | "host" | "chdir" | "chroot" | "close-from"
                | "command-timeout" | "role" | "type" | "other-user" => {
                    if value.is_none() && args.next().is_none() {
                        return false;
                    }
                }
                "reset-timestamp" | "background" | "bell" | "edit" | "set-home"
                | "preserve-env" | "preserve-groups" => (),
                _ => return false,
            }
        } else {
            let mut options = word[1..].chars();
            while let Some(flag) = options.next() {
                match flag {
                    'n' | 'S' | 'A' | 'V' | 'K' => return false,
                    'v' | 'l' | 's' | 'i' => standalone = true,
                    'u' | 'g' | 'p' | 'h' | 'D' | 'R' | 'C' | 'T' | 'U' | 'r' | 't' => {
                        if options.as_str().is_empty() && args.next().is_none() {
                            return false;
                        }
                        break;
                    }
                    'k' | 'B' | 'b' | 'e' | 'E' | 'H' | 'P' => (),
                    _ => return false,
                }
            }
        }
    }
    standalone
}

fn tty_diagnostic(argv: &[String], output: &str) -> bool {
    let Some(name) = Path::new(&argv[0]).file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if matches!(name, "printf" | "echo" | "cat") {
        return false;
    }
    let sudo = name == "sudo";
    if sudo && !interactive_sudo(argv) {
        return false;
    }
    output.lines().any(|line| {
        let msg = line.trim().to_ascii_lowercase();
        let prefix = format!("{name}:");
        let Some(text) = msg.strip_prefix(&prefix) else {
            return matches!(name, "docker" | "podman") && msg == "the input device is not a tty";
        };
        [
            "a terminal is required",
            "no tty present",
            "must have a tty",
            "must be run from a terminal",
            "requires a tty",
            "requires a terminal",
            "not a tty",
            "inappropriate ioctl for device",
            "cannot open /dev/tty",
            "can't open /dev/tty",
            "cannot open '/dev/tty'",
        ]
        .iter()
        .any(|s| text.contains(s))
            || (sudo && text.trim() == "a password is required")
    })
}

#[derive(Clone)]
pub(super) struct Review {
    command: String,
    argv: Vec<String>,
    directory: PathBuf,
    from_tool: bool,
}

impl Review {
    fn new(command: &str, argv: Vec<String>, cwd: Option<&Path>, project: &Path) -> Self {
        Self {
            command: command.to_owned(),
            argv,
            directory: cwd.unwrap_or(project).to_owned(),
            from_tool: cwd.is_some(),
        }
    }
    pub(super) fn approval(
        command: &ox_desk_core::asks::CommandApproval,
        project: &Path,
    ) -> Option<Self> {
        let argv = invocation(&command.command)?;
        interactive_sudo(&argv)
            .then(|| Self::new(&command.command, argv, command.cwd.as_deref(), project))
    }
    pub(super) fn failed(entry: &Entry, project: &Path) -> Option<Self> {
        Self::failed_item(&entry.item, &entry.output, project)
    }
    fn failed_item(item: &Item, output: &str, project: &Path) -> Option<Self> {
        let Item::CommandExecution {
            command,
            status,
            exit_code,
            aggregated_output,
            cwd,
        } = item
        else {
            return None;
        };
        if *exit_code == Some(0)
            || !(matches!(status.as_str(), "failed" | "error")
                || (matches!(status.as_str(), "completed" | "success")
                    && exit_code.is_some_and(|c| c != 0)))
        {
            return None;
        }
        let argv = invocation(command)?;
        let diagnostic = aggregated_output
            .as_deref()
            .is_some_and(|s| tty_diagnostic(&argv, s))
            || tty_diagnostic(&argv, output);
        diagnostic.then(|| Self::new(command, argv, cwd.as_deref(), project))
    }
    pub(super) fn directory_label(&self) -> String {
        let source = if self.from_tool {
            "Tool directory"
        } else {
            "Session project (tool directory unavailable)"
        };
        format!("{source} · {}", self.directory.display())
    }
    fn valid_directory(&self) -> Result<(), String> {
        if self.directory.is_absolute() && self.directory.is_dir() {
            Ok(())
        } else {
            Err(format!(
                "The command working directory is unavailable: {}",
                self.directory.display()
            ))
        }
    }
    fn run(&self, cx: &mut App) -> Result<(), String> {
        self.valid_directory()?;
        let name = &self.argv[0];
        let program = if name.contains('/') {
            self.directory.join(name)
        } else {
            crate::platform::find(name, &crate::platform::child_path())
                .ok_or_else(|| format!("Cannot find {name} on the desktop's executable path."))?
        };
        let command = TerminalCommand {
            program,
            args: self.argv[1..].iter().map(Into::into).collect(),
        };
        terminal::open(self.directory.clone(), Some(command), |_, _| {}, cx)
    }
}

#[derive(Clone)]
pub(super) enum Source {
    Failed { thread: String, item: ItemKey },
    Approval { id: RequestId, token: String },
}

pub(super) struct Pending {
    receipt: String,
    review: Review,
}
impl Pending {
    fn outcome(&self, batch: &[LinkEvent]) -> Option<bool> {
        batch.iter().find_map(|event| match event {
            LinkEvent::Host(f) => match f.as_ref() {
                Server::Ack { id } if id == &self.receipt => Some(true),
                Server::Error { id: Some(id), .. } if id == &self.receipt => Some(false),
                _ => None,
            },
            LinkEvent::Closed { .. } => Some(false),
            _ => None,
        })
    }
}
impl Workbench {
    pub(super) fn review_tool_command(
        &mut self,
        source: Source,
        review: Review,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let result = review.valid_directory().and_then(|()| match source {
            Source::Failed { thread, item } => {
                let valid = store
                    .transcript(&thread)
                    .filter(|t| t.active_turn().is_none())
                    .and_then(|t| t.entry(&item))
                    .and_then(|e| Review::failed(e, &store.profile().project))
                    .is_some_and(|current| {
                        current.command == review.command && current.directory == review.directory
                    });
                if valid {
                    review.run(cx)
                } else {
                    Err("This tool changed or the agent is still running.".into())
                }
            }
            Source::Approval { id, token } => {
                if self.pending_local_command.is_some() {
                    return Err("Another terminal handoff is pending.".into());
                }
                store
                    .decline_reviewed_command(&id, &token)
                    .map(|receipt| {
                        self.pending_local_command = Some(Pending { receipt, review });
                    })
                    .map_err(|e| e.to_string())
            }
        });
        if let Err(e) = result {
            self.launch_error = Some(e);
        }
        cx.notify();
    }
    pub(super) fn finish_local_command(&mut self, outcome: Option<bool>, cx: &mut Context<Self>) {
        if let Some(accepted) = outcome
            && let Some(pending) = self.pending_local_command.take()
            && accepted
            && let Err(e) = pending.review.run(cx)
        {
            self.launch_error = Some(e);
            cx.notify();
        }
    }
    pub(super) fn local_command_outcome(&self, batch: &[LinkEvent]) -> Option<bool> {
        self.pending_local_command.as_ref()?.outcome(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::{Item, LinkEvent, Path, Pending, Review, Server};
    use ox_desk_core::asks::CommandApproval;
    const NEED_TTY: &str = "sudo: a terminal is required to read the password; either use the -S option to read from standard input or configure an askpass helper";
    fn candidate(text: &str) -> Option<Review> {
        Review::approval(
            &CommandApproval {
                command: text.to_owned(),
                cwd: None,
                decline: 1,
            },
            Path::new("/demo"),
        )
    }
    fn failed(command: &str, status: &str, code: Option<i32>, output: &str) -> Option<Review> {
        Review::failed_item(
            &Item::CommandExecution {
                command: command.into(),
                status: status.into(),
                cwd: None,
                aggregated_output: Some(output.into()),
                exit_code: code,
            },
            "",
            Path::new("/demo"),
        )
    }
    #[test]
    fn identifies_the_sudo_executable_and_option_boundary() {
        for text in [
            "sudo id",
            "/usr/bin/sudo -u root -- id -n",
            "sudo -k echo -S",
            "sudo -ufoo -v",
            "sudo --preserve-env=LANG --user root echo --askpass",
            "/bin/bash -lc 'sudo id'",
            "sudo -p 'Password for %u: ' -v",
        ] {
            assert!(candidate(text).is_some(), "{text}");
        }
        for text in [
            "sudo",
            "sudo -k",
            "sudo --version",
            "sudo --help",
            "/usr/bin/sudo -An id",
            "sudo -Eu root -n id",
            "sudo -u root --stdin id",
            "sudo -AS id",
            "sudo --askpass id",
            "sudo --non-interactive id",
            "printf 'sudo id'",
            "echo \"sudo id\"",
            "rustc --help",
            "sudo-wrapper sudo id",
            "/bin/sh -c 'echo sudo id'",
            "true && sudo id",
            "echo pw | sudo -S cat",
            "sudo id; echo again",
            "sudo cat $HOME/file",
            "sudo id\ntrue",
        ] {
            assert!(candidate(text).is_none(), "{text}");
        }
        assert_eq!(
            candidate(r#"/bin/bash -lc "sudo -u root printf '%s' 'two words' ';echo other' ''""#)
                .unwrap()
                .argv,
            [
                "sudo",
                "-u",
                "root",
                "printf",
                "%s",
                "two words",
                ";echo other",
                ""
            ]
        );
    }
    #[test]
    fn requires_a_terminal_failure_and_excludes_live_or_successful_commands() {
        assert!(failed("sudo id", "failed", Some(1), NEED_TTY).is_some());
        assert!(
            failed(
                "sudo id",
                "completed",
                Some(1),
                "sudo: no tty present and no askpass program specified"
            )
            .is_some()
        );
        assert!(
            failed(
                "stty -a",
                "failed",
                None,
                "stty: 'standard input': Inappropriate ioctl for device"
            )
            .is_some()
        );
        for (command, status, code, output) in [
            ("sudo id", "completed", Some(0), NEED_TTY),
            ("sudo id", "running", None, "[sudo] password for alice:"),
            ("sudo id", "inProgress", Some(1), NEED_TTY),
            ("sudo id", "failed", Some(0), NEED_TTY),
            (
                "sudo -n id",
                "failed",
                Some(1),
                "sudo: a password is required",
            ),
            ("sudo id", "failed", Some(1), "sudo: id: command not found"),
            (
                "sudo id",
                "failed",
                Some(1),
                "the user is not in the sudoers file",
            ),
            ("printf 'sudo id'", "failed", Some(1), NEED_TTY),
            ("sudo id", "declined", None, NEED_TTY),
            ("sudo id", "completed", None, NEED_TTY),
        ] {
            assert!(
                failed(command, status, code, output).is_none(),
                "{command} {status} {code:?}"
            );
        }
    }
    #[test]
    fn keeps_native_cwd_or_labels_the_session_project_and_refuses_stale_paths() {
        let root = tempfile::tempdir().unwrap();
        let explicit = root.path().join("native tool directory");
        std::fs::create_dir(&explicit).unwrap();
        let mut command = CommandApproval {
            command: "sudo id".into(),
            cwd: Some(explicit.clone()),
            decline: 1,
        };
        let review = Review::approval(&command, root.path()).unwrap();
        assert_eq!(review.directory, explicit);
        assert!(review.from_tool && review.valid_directory().is_ok());
        std::fs::remove_dir(&explicit).unwrap();
        assert!(review.valid_directory().is_err());
        command.cwd = None;
        let review = Review::approval(&command, root.path()).unwrap();
        assert_eq!(review.directory, root.path());
        assert!(!review.from_tool && review.valid_directory().is_ok());
        command.cwd = Some("relative".into());
        assert!(
            Review::approval(&command, root.path())
                .unwrap()
                .valid_directory()
                .is_err()
        );
    }
    #[test]
    fn only_the_local_refusal_receipt_unlocks_the_terminal_review() {
        let pending = Pending {
            receipt: "local-decline".into(),
            review: candidate("sudo id").unwrap(),
        };
        let response = |s| LinkEvent::Host(Box::new(s));
        assert_eq!(
            pending.outcome(&[response(Server::Ack {
                id: "other-device".into()
            })]),
            None
        );
        assert_eq!(
            pending.outcome(&[response(Server::Error {
                id: Some("local-decline".into()),
                message: "already answered".into()
            })]),
            Some(false)
        );
        assert_eq!(
            pending.outcome(&[LinkEvent::Closed {
                reason: "lost".into(),
                stderr_tail: vec![]
            }]),
            Some(false)
        );
        assert_eq!(
            pending.outcome(&[response(Server::Ack {
                id: "local-decline".into()
            })]),
            Some(true)
        );
    }
}
