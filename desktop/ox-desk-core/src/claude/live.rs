//! Detect Claude Code sessions that a stock client is driving right now, outside OX.
//! Two cheap signals: the journal was written within `FRESH`, or a `claude` process on
//! this machine has the session id on its command line (Linux only).
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

/// A journal written this recently belongs to a client that is still running.
pub const FRESH: Duration = Duration::from_secs(90);

pub fn is_fresh(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified).is_ok_and(|age| age <= FRESH)
}

/// Session ids named on the command line of any running `claude` process. One pass
/// over /proc; other platforms report nothing and rely on the journal's age.
pub fn ids_in_processes() -> HashSet<String> {
    let mut ids = HashSet::new();
    #[cfg(target_os = "linux")]
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.as_encoded_bytes().iter().all(u8::is_ascii_digit) {
                continue;
            }
            if let Ok(raw) = std::fs::read(entry.path().join("cmdline")) {
                ids.extend(cmdline_ids(&raw));
            }
        }
    }
    ids
}

/// Whether a registered pid is still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    /// The platform cannot tell; callers keep the journal-age fallback.
    Unknown,
}

/// Portable liveness: `kill(pid, 0)` succeeds or fails with EPERM for a live process
/// and fails with ESRCH for a dead one. No procfs needed (macOS, BSD).
pub fn pid_liveness(pid: u32) -> Liveness {
    #[cfg(unix)]
    {
        // A pid outside the kernel's range (or 0, a process group) names no process.
        let Ok(raw) = libc::pid_t::try_from(pid) else {
            return Liveness::Dead;
        };
        if raw <= 0 {
            return Liveness::Dead;
        }
        // SAFETY: signal 0 only checks that the pid exists and may be signalled.
        if unsafe { libc::kill(raw, 0) } == 0 {
            return Liveness::Alive;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Liveness::Dead,
            Some(libc::EPERM) => Liveness::Alive,
            _ => Liveness::Unknown,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Liveness::Unknown
    }
}

/// What the sessions registry says. `authoritative` is false when any entry's liveness
/// could not be determined, so the caller keeps the journal-age fallback.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Registry {
    pub ids: HashSet<String>,
    pub authoritative: bool,
}

/// Sessions a running stock client registered under `<profile>/sessions/<pid>.json`.
/// A terminal client has no session id on its command line, so this is the signal that
/// still holds while it thinks or runs a long tool without writing its journal. Only a
/// live pid counts, and never OX's own headless entrypoint.
pub fn ids_in_registry(profile: &std::path::Path) -> Registry {
    registry_with(profile, pid_liveness)
}

pub fn registry_with(profile: &std::path::Path, probe: impl Fn(u32) -> Liveness) -> Registry {
    let mut out = Registry {
        ids: HashSet::new(),
        authoritative: false,
    };
    let Ok(entries) = std::fs::read_dir(profile.join("sessions")) else {
        return out;
    };
    out.authoritative = true;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(raw) = std::fs::read(&path) else {
            continue;
        };
        let Some(id) = registered_id(&raw) else {
            continue;
        };
        let pid = serde_json::from_slice::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v["pid"].as_u64())
            .and_then(|p| u32::try_from(p).ok());
        match pid.map_or(Liveness::Unknown, &probe) {
            Liveness::Alive => {
                out.ids.insert(id);
            }
            Liveness::Dead => {}
            Liveness::Unknown => out.authoritative = false,
        }
    }
    out
}
/// The session id of one registry file, unless it belongs to a headless client.
pub fn registered_id(raw: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
    let id = v["sessionId"].as_str()?;
    (super::catalog::valid_session_id(id) && v["entrypoint"].as_str() != Some(HEADLESS_ENTRYPOINT))
        .then(|| id.to_owned())
}

/// The entrypoint a headless OX-style client writes into every journal row.
pub const HEADLESS_ENTRYPOINT: &str = "sdk-cli";

/// The session ids in one NUL-separated argv, when it is a `claude` invocation.
pub fn cmdline_ids(raw: &[u8]) -> Vec<String> {
    let args: Vec<&str> = raw
        .split(|b| *b == 0)
        .filter_map(|a| std::str::from_utf8(a).ok())
        .filter(|a| !a.is_empty())
        .collect();
    // `claude ...` or `node /path/claude ...`: the program is among the first two words.
    let is_claude = args.iter().take(2).any(|a| {
        std::path::Path::new(a)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "claude" || n == "claude.exe")
    });
    // OX drives Claude headlessly; a terminal session never has stream-json on its argv.
    if !is_claude || args.contains(&"stream-json") {
        return Vec::new();
    }
    args.iter()
        .skip(1)
        .flat_map(|a| a.split('='))
        .filter(|a| super::catalog::valid_session_id(a))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "01234567-abcd-4567-abcd-0123456789ab";

    #[test]
    fn a_registry_file_names_its_interactive_session_only() {
        let row = |entry: &str| format!(r#"{{"pid":1,"sessionId":"{ID}","entrypoint":"{entry}"}}"#);
        assert_eq!(registered_id(row("cli").as_bytes()).as_deref(), Some(ID));
        assert_eq!(registered_id(row("sdk-cli").as_bytes()), None);
        assert_eq!(registered_id(b"{"), None);
    }

    #[test]
    fn registry_liveness_alive_dead_and_indeterminate_keep_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let other = "11111111-abcd-4567-abcd-0123456789ab";
        for (pid, id) in [(10, ID), (20, other)] {
            std::fs::write(
                sessions.join(format!("{pid}.json")),
                format!(r#"{{"pid":{pid},"sessionId":"{id}","entrypoint":"cli"}}"#),
            )
            .unwrap();
        }
        // No procfs on this fixture platform: the injected probe is all there is.
        let r = registry_with(dir.path(), |pid| {
            if pid == 10 {
                Liveness::Alive
            } else {
                Liveness::Dead
            }
        });
        assert_eq!(r.ids, HashSet::from([ID.to_owned()]));
        assert!(r.authoritative);
        let r = registry_with(dir.path(), |_| Liveness::Dead);
        assert!(r.ids.is_empty() && r.authoritative);
        let r = registry_with(dir.path(), |pid| {
            if pid == 10 {
                Liveness::Alive
            } else {
                Liveness::Unknown
            }
        });
        assert_eq!(r.ids, HashSet::from([ID.to_owned()]));
        assert!(
            !r.authoritative,
            "indeterminate liveness keeps the journal fallback"
        );
        // No registry directory at all: not authoritative either.
        assert!(!registry_with(&dir.path().join("none"), |_| Liveness::Alive).authoritative);
    }

    #[test]
    fn real_probe_knows_this_process_and_a_dead_pid() {
        assert_eq!(pid_liveness(std::process::id()), Liveness::Alive);
        #[cfg(unix)]
        {
            let mut c = std::process::Command::new("true").spawn().unwrap();
            let pid = c.id();
            c.wait().unwrap();
            assert_eq!(pid_liveness(pid), Liveness::Dead);
        }
    }

    #[test]
    fn only_claude_processes_name_sessions() {
        let argv = |s: &[&str]| s.join("\0").into_bytes();
        assert_eq!(cmdline_ids(&argv(&["claude", "--resume", ID])), [ID]);
        assert_eq!(
            cmdline_ids(&argv(&[
                "node",
                "/usr/bin/claude",
                &format!("--resume={ID}")
            ])),
            [ID]
        );
        assert!(cmdline_ids(&argv(&["vim", ID])).is_empty());
        assert!(cmdline_ids(&argv(&["claude", "--continue"])).is_empty());
        // A process OX launched is not a foreign client.
        assert!(
            cmdline_ids(&argv(&[
                "claude",
                "--resume",
                ID,
                "--input-format",
                "stream-json"
            ]))
            .is_empty()
        );
    }

    #[test]
    fn freshness_window() {
        let now = SystemTime::now();
        assert!(is_fresh(now - Duration::from_secs(30), now));
        assert!(!is_fresh(now - Duration::from_secs(120), now));
        assert!(!is_fresh(
            now + Duration::from_secs(5),
            now - Duration::from_secs(10)
        ));
    }
}
