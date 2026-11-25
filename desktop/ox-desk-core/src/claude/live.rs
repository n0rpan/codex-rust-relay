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
