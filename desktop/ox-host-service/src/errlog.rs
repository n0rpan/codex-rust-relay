//! A small, private, rotating log of host errors for `ox host doctor`.
//!
//! Lines are `<unix seconds> <message>`. Messages are host error strings;
//! callers never pass prompts, commands, tool arguments or file contents.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

pub const FILE: &str = "host-errors.log";
const KEEP_BYTES: u64 = 64 * 1024;
static LOG: OnceLock<Mutex<PathBuf>> = OnceLock::new();

/// Record errors under this state directory from now on.
pub fn init(state_dir: &Path) {
    let _ = LOG.set(Mutex::new(state_dir.join(FILE)));
}

/// Print to stderr (the service manager's log) and append to the error log.
pub fn record(message: &str) {
    eprintln!("ox-host: {message}");
    let Some(lock) = LOG.get() else { return };
    let Ok(path) = lock.lock() else { return };
    let _ = append(&path, message);
}

fn append(path: &Path, message: &str) -> io::Result<()> {
    if fs::metadata(path).is_ok_and(|m| m.len() > KEEP_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let line: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(2000)
        .collect();
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?
        .write_all(format!("{at} {line}\n").as_bytes())
}

/// The last `n` recorded errors, oldest first, as (unix seconds, message).
pub fn tail(state_dir: &Path, n: usize) -> Vec<(u64, String)> {
    let path = state_dir.join(FILE);
    let mut text = fs::read_to_string(path.with_extension("log.1")).unwrap_or_default();
    text.push_str(&fs::read_to_string(&path).unwrap_or_default());
    let mut lines: Vec<_> = text
        .lines()
        .filter_map(|l| {
            let (at, msg) = l.split_once(' ')?;
            Some((at.parse().ok()?, msg.to_owned()))
        })
        .collect();
    let skip = lines.len().saturating_sub(n);
    lines.drain(..skip);
    lines
}

#[cfg(test)]
mod tests {
    #[test]
    fn keeps_the_last_errors_across_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(super::FILE);
        for i in 0..3000 {
            super::append(&path, &format!("error {i}\nwith a newline")).unwrap();
        }
        let tail = super::tail(dir.path(), 3);
        let messages: Vec<_> = tail.iter().map(|(_, m)| m.as_str()).collect();
        assert_eq!(
            messages,
            [
                "error 2997 with a newline",
                "error 2998 with a newline",
                "error 2999 with a newline"
            ]
        );
        assert!(std::fs::metadata(&path).unwrap().len() <= super::KEEP_BYTES + 100);
    }
}
