//! `ox ps` and `ox end`: the terminal view of the session host's live processes.
//!
//! The host (`desktop/ox-host`) listens on a per-user unix socket and speaks
//! newline-delimited JSON, one frame per line, with `"type"` as the tag. A client
//! sends `hello` first, then `list_processes` (answered by `processes`). To end a
//! session it sends `subscribe` with a null scope once, then `act` with the row's
//! scope and `{"end_session": thread}`, and waits for `ack` or `error`. This module
//! is a minimal client for those frames. It never starts the host.

use crate::provider::RelayError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthStr;

/// The protocol version this client speaks; the host's `hello` must match it.
const PROTOCOL_VERSION: u64 = 1;
/// The host's own frame limit.
const MAX_FRAME: u64 = 64 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const NOT_RUNNING: &str = "OX host is not running.";
const SHORT_ID: usize = 8;
const HEADERS: [&str; 7] = [
    "KIND", "MODEL", "PROJECT", "AGE", "IDLE", "STATE", "THREADS",
];

/// One `ProcessView` from the host. Unknown fields are ignored on purpose.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Row {
    pub kind: String,
    pub project: PathBuf,
    pub model: Option<String>,
    pub started_at: i64,
    pub last_activity: i64,
    pub running: bool,
    /// The blank process that keeps Claude Code warm for new sessions.
    #[serde(default)]
    pub draft: bool,
    pub threads: Vec<String>,
    /// Echoed back verbatim in `act`, so the host sees the scope it sent.
    pub scope: Value,
}

/// One thread that `ox end` can act on, with the scope of its process.
#[derive(Clone, Debug, PartialEq)]
struct Session {
    thread: String,
    scope: Value,
    label: String,
    running: bool,
}

#[derive(Debug, PartialEq)]
enum Selector {
    Prefix(String),
    Idle,
    All,
}

#[derive(Debug, PartialEq)]
struct EndArgs {
    selector: Selector,
    yes: bool,
}

pub(crate) fn run_ps(args: &[OsString]) -> Result<u8, RelayError> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => return Err("Usage: ox ps [--json]".into()),
    };
    let Some(mut host) = Host::connect()? else {
        println!("{NOT_RUNNING}");
        return Ok(0);
    };
    let raw = host.processes()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&raw)?);
        return Ok(0);
    }
    let rows = parse_rows(&raw)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    println!(
        "{}",
        render_table(&rows, now(), home.as_deref()).trim_end_matches('\n')
    );
    Ok(0)
}

pub(crate) fn run_end(args: &[OsString]) -> Result<u8, RelayError> {
    let args = parse_end_args(args)?;
    let Some(mut host) = Host::connect()? else {
        println!("{NOT_RUNNING}");
        return Ok(1);
    };
    let rows = parse_rows(&host.processes()?)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let sessions = sessions(&rows, home.as_deref());
    let width = id_width(sessions.iter().map(|s| s.thread.as_str()));
    let targets: Vec<Session> = match &args.selector {
        Selector::Prefix(prefix) => {
            let index = resolve_prefix(&sessions, prefix)?;
            vec![sessions[index].clone()]
        }
        Selector::Idle => sessions_of(&rows, home.as_deref(), |row| !row.running && !row.draft),
        Selector::All => sessions,
    };
    if targets.is_empty() {
        println!("No sessions to end.");
        return Ok(0);
    }
    let busy = targets.iter().filter(|s| s.running).count();
    let all = matches!(args.selector, Selector::All);
    if (all || busy > 0) && !args.yes {
        if !io::stdin().is_terminal() {
            return Err(if all {
                "ox end --all needs --yes when there is no terminal to confirm in"
            } else {
                "ox end of a running session needs --yes when there is no terminal to confirm in"
            }
            .into());
        }
        if busy > 0 {
            print!("{busy} running session(s) will be stopped. ");
        }
        print!("End {} session(s)? [y/N] ", targets.len());
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            println!("Nothing ended.");
            return Ok(0);
        }
    }
    host.subscribe_all()?;
    for session in &targets {
        let shown = id_prefix(&session.thread, width);
        host.end(&session.scope, &session.thread, session.running)
            .map_err(|e| format!("could not end {shown}: {e}"))?;
        println!("ended {shown} ({})", session.label);
    }
    Ok(0)
}

fn parse_end_args(args: &[OsString]) -> Result<EndArgs, RelayError> {
    const USAGE: &str = "Usage: ox end <thread-id-prefix> | ox end --idle | ox end --all [--yes]";
    let mut selector = None;
    let mut yes = false;
    for arg in args {
        let text = arg.to_str().ok_or(USAGE)?;
        let next = match text {
            "--idle" => Selector::Idle,
            "--all" => Selector::All,
            "--yes" | "-y" => {
                yes = true;
                continue;
            }
            "-h" | "--help" => return Err(USAGE.into()),
            flag if flag.starts_with('-') => return Err(USAGE.into()),
            prefix => Selector::Prefix(prefix.to_owned()),
        };
        if selector.replace(next).is_some() {
            return Err(USAGE.into());
        }
    }
    match selector {
        Some(selector) => Ok(EndArgs { selector, yes }),
        None => Err(USAGE.into()),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_rows(raw: &Value) -> Result<Vec<Row>, RelayError> {
    Ok(serde_json::from_value(raw.clone())?)
}

/// Every thread of every row, skipping a thread that appears twice.
fn sessions(rows: &[Row], home: Option<&Path>) -> Vec<Session> {
    sessions_of(rows, home, |_| true)
}

fn sessions_of(rows: &[Row], home: Option<&Path>, keep: impl Fn(&Row) -> bool) -> Vec<Session> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for row in rows.iter().filter(|&row| keep(row)) {
        for thread in &row.threads {
            if seen.insert(thread.clone()) {
                out.push(Session {
                    thread: thread.clone(),
                    scope: row.scope.clone(),
                    label: format!("{} {}", row.kind, project_label(&row.project, home)),
                    running: row.running,
                });
            }
        }
    }
    out
}

/// An exact thread id wins. Otherwise the prefix must match exactly one session.
fn resolve_prefix(sessions: &[Session], prefix: &str) -> Result<usize, String> {
    if prefix.is_empty() {
        return Err("give a thread id prefix, or use --idle or --all".into());
    }
    if let Some(index) = sessions.iter().position(|s| s.thread == prefix) {
        return Ok(index);
    }
    let matches: Vec<usize> = sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| s.thread.starts_with(prefix))
        .map(|(i, _)| i)
        .collect();
    match matches.as_slice() {
        [] => Err(format!(
            "no session matches {prefix:?}\n  fix: ox ps lists the running sessions"
        )),
        [index] => Ok(*index),
        many => {
            let mut message = format!(
                "{prefix:?} matches {} sessions, so it is ambiguous. Use a longer prefix:",
                many.len()
            );
            let width = id_width(sessions.iter().map(|s| s.thread.as_str()));
            for &index in many {
                let s = &sessions[index];
                message.push_str(&format!("\n  {}  {}", id_prefix(&s.thread, width), s.label));
            }
            Err(message)
        }
    }
}

/// The shortest id prefix, at least `SHORT_ID` long, that tells every given thread
/// apart from the others. Distinct ids always differ somewhere, so this never
/// exceeds the full id.
fn id_width<'a>(threads: impl IntoIterator<Item = &'a str>) -> usize {
    let threads: Vec<&str> = threads.into_iter().collect();
    let mut width = SHORT_ID;
    for (i, a) in threads.iter().enumerate() {
        for b in &threads[i + 1..] {
            let shared = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
            width = width.max(shared + 1);
        }
    }
    width
}

fn id_prefix(thread: &str, width: usize) -> String {
    thread.chars().take(width).collect()
}

/// Seconds as `45s`, `12m`, `2h05m` or `3d04h`. Negative spans (clock skew) show as zero.
fn human(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600)
    }
}

/// The project path, with the home directory shown as `~`.
fn project_label(project: &Path, home: Option<&Path>) -> String {
    if let Some(rest) = home.and_then(|h| project.strip_prefix(h).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".to_owned();
        }
        return format!("~/{}", rest.display());
    }
    project.display().to_string()
}

fn cells(row: &Row, now: i64, home: Option<&Path>, width: usize) -> [String; 7] {
    [
        row.kind.clone(),
        row.model.clone().unwrap_or_else(|| "-".to_owned()),
        project_label(&row.project, home),
        human(now - row.started_at),
        human(now - row.last_activity),
        (if row.running {
            "running"
        } else if row.draft {
            "ready (idle)"
        } else {
            "idle"
        })
        .to_owned(),
        row.threads
            .iter()
            .map(|t| id_prefix(t, width))
            .collect::<Vec<_>>()
            .join(","),
    ]
}

/// The `ox ps` table. Columns are padded to their widest cell, and the last
/// column is not padded. Empty input gives a single line saying so.
fn render_table(rows: &[Row], now: i64, home: Option<&Path>) -> String {
    if rows.is_empty() {
        return "No live sessions.\n".to_owned();
    }
    let width = id_width(
        rows.iter()
            .flat_map(|row| row.threads.iter().map(String::as_str)),
    );
    let mut table: Vec<[String; 7]> = vec![HEADERS.map(str::to_owned)];
    table.extend(rows.iter().map(|row| cells(row, now, home, width)));
    let mut widths = [0usize; 7];
    for line in &table {
        for (i, cell) in line.iter().enumerate() {
            widths[i] = widths[i].max(UnicodeWidthStr::width(cell.as_str()));
        }
    }
    let mut out = String::new();
    for line in &table {
        let mut text = String::new();
        for (i, cell) in line.iter().enumerate() {
            if i == line.len() - 1 {
                text.push_str(cell);
            } else {
                let pad = widths[i] - UnicodeWidthStr::width(cell.as_str());
                text.push_str(cell);
                text.push_str(&" ".repeat(pad + 2));
            }
        }
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

/// The host's socket path, computed as `ox-host-service` (`Paths::discover`) computes it.
/// The single copy in this crate; `host_lifetime` uses it too.
fn socket_path_with(get: &dyn Fn(&str) -> Option<OsString>, uid: u32) -> Option<PathBuf> {
    let absolute = |name: &str| get(name).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(path) = absolute("OX_HOST_SOCKET") {
        return Some(path);
    }
    let state_dir = match absolute("OX_HOST_STATE_DIR") {
        Some(root) => root,
        None => absolute("XDG_STATE_HOME")
            .or_else(|| {
                absolute("HOME").map(|h| {
                    h.join(if cfg!(target_os = "macos") {
                        "Library/Application Support"
                    } else {
                        ".local/state"
                    })
                })
            })?
            .join("ox/host"),
    };
    let hash = state_dir
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |v, b| {
            (v ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    let name = format!("ox-host-{uid}-{hash:016x}");
    absolute("XDG_RUNTIME_DIR")
        .map(|p| p.join(&name).join("host.sock"))
        .filter(|p| p.as_os_str().as_encoded_bytes().len() < 100)
        .or_else(|| Some(Path::new("/tmp").join(&name).join("host.sock")))
}

pub(crate) fn socket_path() -> Option<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    socket_path_with(&|name: &str| std::env::var_os(name), uid)
}

/// A connected, greeted host session.
struct Host {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    requests: u64,
}

impl Host {
    /// `Ok(None)` when no host answers on the socket. Never starts one.
    fn connect() -> Result<Option<Self>, RelayError> {
        let Some(path) = socket_path() else {
            return Ok(None);
        };
        let stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(e) => {
                return Err(format!("could not reach the OX host: {e}").into());
            }
        };
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        let writer = stream.try_clone()?;
        let mut host = Self {
            reader: BufReader::new(stream),
            writer,
            requests: 0,
        };
        host.send(&json!({"type": "hello", "version": PROTOCOL_VERSION}))?;
        let frame = host.read_frame()?;
        match frame["type"].as_str() {
            Some("hello") if frame["version"].as_u64() == Some(PROTOCOL_VERSION) => Ok(Some(host)),
            Some("hello") => Err(format!(
                "the OX host speaks protocol {}, and this ox speaks {PROTOCOL_VERSION}. Run `ox host restart`.",
                frame["version"]
            )
            .into()),
            Some("error") => Err(error_message(&frame).into()),
            _ => Err("the OX host sent an unexpected hello.".into()),
        }
    }

    /// The raw `processes` array, newest first, as the host sends it.
    fn processes(&mut self) -> Result<Value, RelayError> {
        let id = self.next_id();
        self.send(&json!({"type": "list_processes", "id": id}))?;
        loop {
            let frame = self.read_frame()?;
            if answers(&frame, &id) {
                match frame["type"].as_str() {
                    Some("processes") => return Ok(frame["processes"].clone()),
                    Some("error") => return Err(error_message(&frame).into()),
                    _ => {}
                }
            }
        }
    }

    /// Subscribe to every scope, which `act` needs before it accepts an action.
    /// The host sends a snapshot in reply. Its frames are skipped by `end`.
    fn subscribe_all(&mut self) -> Result<(), RelayError> {
        self.send(&json!({"type": "subscribe", "scope": null, "after": null}))
    }

    fn end(&mut self, scope: &Value, thread: &str, force: bool) -> Result<(), RelayError> {
        let action = if force {
            json!({"force_end_session": thread})
        } else {
            json!({"end_session": thread})
        };
        let id = self.next_id();
        self.send(&json!({
            "type": "act",
            "id": id,
            "scope": scope,
            "action": action,
            "approval": null,
        }))?;
        loop {
            let frame = self.read_frame()?;
            if answers(&frame, &id) {
                match frame["type"].as_str() {
                    Some("ack") => return Ok(()),
                    Some("error") => return Err(error_message(&frame).into()),
                    _ => {}
                }
            }
        }
    }

    fn next_id(&mut self) -> String {
        self.requests += 1;
        format!("ox-cli-{}-{}", std::process::id(), self.requests)
    }

    fn send(&mut self, frame: &Value) -> Result<(), RelayError> {
        let mut bytes = serde_json::to_vec(frame)?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes)?;
        Ok(())
    }

    fn read_frame(&mut self) -> Result<Value, RelayError> {
        let mut line = Vec::new();
        let read = (&mut self.reader)
            .take(MAX_FRAME)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Err("the OX host closed the connection.".into());
        }
        if line.last() != Some(&b'\n') {
            return Err("the OX host sent an incomplete or oversized frame.".into());
        }
        Ok(serde_json::from_slice(&line)?)
    }
}

/// A reply belongs to a request when its id matches. An error with no id ends the
/// connection, so it answers any request.
fn answers(frame: &Value, id: &str) -> bool {
    frame["id"].as_str() == Some(id) || (frame["type"] == "error" && frame["id"].is_null())
}

fn error_message(frame: &Value) -> String {
    frame["message"]
        .as_str()
        .unwrap_or("the OX host refused the request.")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, model: Option<&str>, running: bool, threads: &[&str]) -> Row {
        Row {
            kind: kind.into(),
            project: PathBuf::from("/home/oscar/Work/ox"),
            model: model.map(str::to_owned),
            started_at: 1_000,
            last_activity: 1_900,
            running,
            draft: false,
            threads: threads.iter().map(|t| (*t).to_owned()).collect(),
            scope: json!({"project": "/home/oscar/Work/ox", "route": {}}),
        }
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn durations_read_as_one_unit_and_never_go_negative() {
        assert_eq!(human(-5), "0s");
        assert_eq!(human(45), "45s");
        assert_eq!(human(12 * 60 + 9), "12m");
        assert_eq!(human(2 * 3600 + 5 * 60), "2h05m");
        assert_eq!(human(3 * 86_400 + 4 * 3600), "3d04h");
    }

    #[test]
    fn table_aligns_columns_and_shows_short_ids() {
        let rows = [
            row("claude", Some("sonnet"), true, &["abcdef0123456789"]),
            row(
                "codex",
                None,
                false,
                &["fedcba9876543210", "0011223344556677"],
            ),
        ];
        let text = render_table(&rows, 2_000, Some(Path::new("/home/oscar")));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "KIND    MODEL   PROJECT    AGE  IDLE  STATE    THREADS"
        );
        assert_eq!(
            lines[1],
            "claude  sonnet  ~/Work/ox  16m  1m    running  abcdef01"
        );
        assert_eq!(
            lines[2],
            "codex   -       ~/Work/ox  16m  1m    idle     fedcba98,00112233"
        );
    }

    #[test]
    fn table_says_so_when_nothing_runs() {
        assert_eq!(render_table(&[], 0, None), "No live sessions.\n");
    }

    #[test]
    fn project_label_shortens_home_and_keeps_other_paths() {
        let home = Path::new("/home/oscar");
        assert_eq!(project_label(Path::new("/home/oscar"), Some(home)), "~");
        assert_eq!(project_label(Path::new("/home/oscar/x"), Some(home)), "~/x");
        assert_eq!(project_label(Path::new("/srv/app"), Some(home)), "/srv/app");
        assert_eq!(project_label(Path::new("/srv/app"), None), "/srv/app");
    }

    #[test]
    fn prefix_matches_one_session() {
        let rows = [
            row("claude", None, false, &["abc123", "abd456"]),
            row("codex", None, false, &["zzz000"]),
        ];
        let all = sessions(&rows, None);
        assert_eq!(resolve_prefix(&all, "zz").unwrap(), 2);
        assert_eq!(resolve_prefix(&all, "abc1").unwrap(), 0);
    }

    #[test]
    fn exact_id_wins_even_when_it_prefixes_another_id() {
        let rows = [row("claude", None, false, &["abc", "abcd"])];
        let all = sessions(&rows, None);
        assert_eq!(resolve_prefix(&all, "abc").unwrap(), 0);
        assert_eq!(resolve_prefix(&all, "abcd").unwrap(), 1);
    }

    #[test]
    fn ambiguous_prefix_lists_candidates() {
        let rows = [row("claude", None, false, &["abc123", "abd456"])];
        let all = sessions(&rows, None);
        let error = resolve_prefix(&all, "ab").unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("abc123"), "{error}");
        assert!(error.contains("abd456"), "{error}");
        assert!(resolve_prefix(&all, "abc").is_ok());
    }

    #[test]
    fn ids_sharing_eight_characters_are_shown_long_enough_to_pick() {
        let rows = [row(
            "codex",
            None,
            false,
            &[
                "0196abcd-1111-7000-8000-000000000001",
                "0196abcd-2222-7000-8000-000000000002",
            ],
        )];
        let all = sessions(&rows, None);
        let error = resolve_prefix(&all, "0196abcd").unwrap_err();
        assert!(error.contains("0196abcd-1  codex"), "{error}");
        assert!(error.contains("0196abcd-2  codex"), "{error}");
        let table = render_table(&rows, 0, None);
        assert!(table.contains("0196abcd-1,0196abcd-2"), "{table}");
        assert_eq!(resolve_prefix(&all, "0196abcd-1").unwrap(), 0);
        assert_eq!(resolve_prefix(&all, "0196abcd-2").unwrap(), 1);
    }

    #[test]
    fn unknown_or_empty_prefix_is_an_error() {
        let rows = [row("claude", None, false, &["abc123"])];
        let all = sessions(&rows, None);
        assert!(
            resolve_prefix(&all, "q")
                .unwrap_err()
                .contains("no session")
        );
        assert!(resolve_prefix(&all, "").is_err());
    }

    #[test]
    fn idle_selection_keeps_only_rows_not_running() {
        let rows = [
            row("claude", None, true, &["r1"]),
            row("codex", None, false, &["i1", "i2"]),
        ];
        let idle = sessions_of(&rows, None, |r| !r.running);
        let threads: Vec<&str> = idle.iter().map(|s| s.thread.as_str()).collect();
        assert_eq!(threads, ["i1", "i2"]);
    }

    #[test]
    fn ready_draft_is_labelled_and_left_out_of_idle_selection() {
        let mut draft = row("claude", Some("default"), false, &["d1"]);
        draft.draft = true;
        let rows = [draft, row("claude", None, false, &["i1"])];
        let idle = sessions_of(&rows, None, |r| !r.running && !r.draft);
        assert_eq!(idle.len(), 1);
        assert_eq!(cells(&rows[0], 2_000, None, 8)[5], "ready (idle)");
    }

    #[test]
    fn end_arguments_need_exactly_one_selector() {
        assert_eq!(
            parse_end_args(&os(&["abc"])).unwrap(),
            EndArgs {
                selector: Selector::Prefix("abc".into()),
                yes: false
            }
        );
        assert_eq!(
            parse_end_args(&os(&["--all", "--yes"])).unwrap(),
            EndArgs {
                selector: Selector::All,
                yes: true
            }
        );
        assert_eq!(
            parse_end_args(&os(&["--idle"])).unwrap(),
            EndArgs {
                selector: Selector::Idle,
                yes: false
            }
        );
        assert!(parse_end_args(&os(&[])).is_err());
        assert!(parse_end_args(&os(&["--idle", "--all"])).is_err());
        assert!(parse_end_args(&os(&["abc", "def"])).is_err());
        assert!(parse_end_args(&os(&["--bogus"])).is_err());
    }

    #[test]
    fn rows_parse_from_the_host_payload_and_ignore_extra_fields() {
        let raw = json!([{
            "kind": "codex",
            "scope": {"project": "/p", "route": {"runtime": "codex", "billing": "subscription"}},
            "project": "/p",
            "threads": ["t1"],
            "route": "codex",
            "model": null,
            "started_at": 5,
            "last_activity": 9,
            "running": false,
            "future_field": true
        }]);
        let rows = parse_rows(&raw).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "codex");
        assert_eq!(rows[0].model, None);
        assert_eq!(rows[0].threads, ["t1"]);
    }

    #[test]
    fn socket_path_follows_the_host_rules() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        let explicit = env(&[("OX_HOST_SOCKET", "/run/x/host.sock"), ("HOME", "/home/o")]);
        assert_eq!(
            socket_path_with(&explicit, 1000),
            Some(PathBuf::from("/run/x/host.sock"))
        );
        let relative = env(&[
            ("OX_HOST_SOCKET", "rel.sock"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HOME", "/home/o"),
        ]);
        let path = socket_path_with(&relative, 1000).unwrap();
        assert!(path.starts_with("/run/user/1000"), "{path:?}");
        assert!(path.ends_with("host.sock"));
        assert!(path.to_string_lossy().contains("ox-host-1000-"));
        let home_only = env(&[("HOME", "/home/o")]);
        let fallback = socket_path_with(&home_only, 1000).unwrap();
        assert!(fallback.starts_with("/tmp"), "{fallback:?}");
        assert!(socket_path_with(&env(&[]), 1000).is_none());
    }

    #[test]
    fn replies_match_on_id_and_id_less_errors_end_the_request() {
        let ok = json!({"type": "ack", "id": "req-1"});
        assert!(answers(&ok, "req-1"));
        assert!(!answers(&ok, "req-2"));
        let snapshot = json!({"type": "snapshot", "cursor": {}, "scopes": []});
        assert!(!answers(&snapshot, "req-1"));
        assert!(answers(
            &json!({"type": "error", "id": null, "message": "no"}),
            "req-1"
        ));
        assert_eq!(
            error_message(&json!({"type": "error"})),
            "the OX host refused the request."
        );
    }
}
