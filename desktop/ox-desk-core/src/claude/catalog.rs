//! Stock Claude Code has no session listing RPC. Inspect only project transcripts;
//! all mutations and resumes still go through the client that owns the session.
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug)]
pub struct Profiles {
    pub subscription: PathBuf,
    pub api: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    Subscription,
    Api,
    Ambiguous,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub project: PathBuf,
    pub origin: Origin,
    pub title: String,
    pub native_title: Option<String>,
    pub preview: String,
    /// Requested model recorded by the stock client. A provider echo is never substituted.
    pub model: Option<String>,
    pub updated_at: i64,
    path: PathBuf,
}

pub fn valid_session_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

const SAMPLE: u64 = 128 * 1024;
const MAX_RECORD: usize = 64 * 1024 * 1024;

fn open_transcript(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Expected a regular Claude transcript",
        ));
    }
    Ok(file)
}

fn directory_is_plain(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(m.is_dir()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn project_key(project: &Path) -> String {
    project
        .to_string_lossy()
        .encode_utf16()
        .map(|unit| {
            u8::try_from(unit)
                .ok()
                .filter(u8::is_ascii_alphanumeric)
                .map_or('-', char::from)
        })
        .collect()
}

fn candidates(profile: &Path, project: &Path) -> io::Result<Vec<PathBuf>> {
    let root = profile.join("projects");
    if !directory_is_plain(&root)? {
        return Ok(vec![]);
    }
    // JS encodes one dash per UTF-16 code unit. For paths over 200 units stock
    // retains the initial 200 and adds a hash. Always confirm the cwd in JSONL.
    let encoded = project_key(project);
    if encoded.len() <= 200 {
        let path = root.join(encoded);
        return Ok(if directory_is_plain(&path)? {
            vec![path]
        } else {
            vec![]
        });
    }
    let prefix = format!("{}-", &encoded[..200]);
    let entries = fs::read_dir(root)?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

fn user_title(row: &Value) -> Option<String> {
    if row["type"] != "user" || row["isMeta"] == true {
        return None;
    }
    let content = &row["message"]["content"];
    let text = content.as_str().or_else(|| {
        content
            .as_array()?
            .iter()
            .find_map(|b| (b["type"] == "text").then(|| b["text"].as_str()).flatten())
    })?;
    text.lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(|s| s.chars().take(256).collect())
}

fn is_message(row: &Value) -> bool {
    matches!(row["type"].as_str(), Some("user" | "assistant")) && row["isSidechain"] != true
}

struct Header<'a> {
    id: &'a str,
    cwd: Option<PathBuf>,
    first: Option<String>,
    title: Option<String>,
    ai_title: Option<String>,
    saw_assistant: bool,
    model: Option<String>,
}

impl Header<'_> {
    fn observe(&mut self, row: Value) {
        if row["sessionId"] != self.id {
            return;
        }
        if is_message(&row) {
            if let Some(cwd) = row["cwd"].as_str() {
                self.cwd.get_or_insert_with(|| PathBuf::from(cwd));
            }
            if self.first.is_none() {
                self.first = user_title(&row);
            }
            // An external TUI can switch models later. Establish the original
            // route from the first non-synthetic assistant response only.
            if !self.saw_assistant
                && row["type"] == "assistant"
                && row["message"]["model"] != "<synthetic>"
            {
                self.saw_assistant = true;
                self.model = row["requestedModel"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned);
            }
        } else if row["type"] == "ai-title" {
            self.ai_title = row["aiTitle"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.chars().take(256).collect());
        } else if row["type"] == "custom-title" {
            self.title = row["customTitle"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.chars().take(256).collect());
        }
    }
}

// Incomplete final writes and obsolete metadata do not invalidate earlier turns.
// Reject oversized complete lines explicitly when loading full history.
fn records(mut reader: impl BufRead, mut f: impl FnMut(Value) -> io::Result<()>) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader
            .by_ref()
            .take(MAX_RECORD as u64 + 1)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(());
        }
        if line.len() > MAX_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude transcript record exceeds 64 MiB",
            ));
        }
        if let Ok(value) = serde_json::from_slice(&line) {
            f(value)?;
        }
    }
}

/// Return the offset after the last complete record. Stock child files may end with a
/// complete JSON object and no newline; keep a partial object for the next read.
fn records_at(
    mut reader: impl BufRead,
    start: u64,
    child: bool,
    mut f: impl FnMut(Value) -> io::Result<()>,
) -> io::Result<u64> {
    let mut line = Vec::new();
    let mut at = start;
    loop {
        line.clear();
        if reader
            .by_ref()
            .take(MAX_RECORD as u64 + 1)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(at);
        }
        if line.len() > MAX_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude transcript record exceeds 64 MiB",
            ));
        }
        let terminated = line.last() == Some(&b'\n');
        if !terminated && !child {
            return Ok(at);
        }
        match serde_json::from_slice(&line) {
            Ok(value) => f(value)?,
            Err(_) if !terminated => return Ok(at),
            Err(_) => (),
        }
        at += line.len() as u64;
    }
}

fn inspect(
    path: PathBuf,
    id: &str,
    expected: Option<&Path>,
    origin: Origin,
) -> io::Result<Option<Session>> {
    let mut file = open_transcript(&path)?;
    let metadata = file.metadata()?;
    let mut header = Header {
        id,
        cwd: None,
        first: None,
        title: None,
        ai_title: None,
        saw_assistant: false,
        model: None,
    };
    records(BufReader::new((&file).take(SAMPLE)), |row| {
        header.observe(row);
        Ok(())
    })?;
    if metadata.len() > SAMPLE {
        // A large first prompt may span the entire initial sample. Fall back to
        // reading until the first complete conversation row establishes the cwd.
        if !header.saw_assistant {
            file.rewind()?;
            let limit = if expected.is_none() {
                2 * 1024 * 1024
            } else {
                u64::MAX
            };
            let mut stream = BufReader::new((&file).take(limit));
            let mut line = Vec::new();
            while !header.saw_assistant {
                line.clear();
                if stream
                    .by_ref()
                    .take(MAX_RECORD as u64 + 1)
                    .read_until(b'\n', &mut line)?
                    == 0
                    || line.len() > MAX_RECORD
                {
                    break;
                }
                if let Ok(value) = serde_json::from_slice(&line) {
                    header.observe(value);
                }
            }
        }
        // A tail sample can establish a recent title, never a first billing route.
        header.saw_assistant = true;
        let off = (metadata.len() - SAMPLE).max(SAMPLE);
        file.seek(SeekFrom::Start(off - 1))?;
        let mut tail = BufReader::new(file);
        tail.skip_until(b'\n')?;
        records(tail, |row| {
            header.observe(row);
            Ok(())
        })?;
    }
    let Some(project) = header.cwd else {
        return Ok(None);
    };
    if !project.is_absolute() || expected.is_some_and(|cwd| cwd != project) {
        return Ok(None);
    }
    if expected.is_none() {
        let Some(dir) = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
        else {
            return Ok(None);
        };
        let key = project_key(&project);
        let matches = if key.len() > 200 {
            dir.strip_prefix(&key[..200])
                .is_some_and(|suffix| suffix.starts_with('-'))
        } else {
            dir == key
        };
        if !matches {
            return Ok(None);
        }
    }
    let native_title = header.title.or(header.ai_title);
    let preview = header.first.unwrap_or_default();
    Ok(Some(Session {
        id: id.to_owned(),
        project,
        origin,
        title: native_title.clone().unwrap_or_else(|| {
            if preview.is_empty() {
                "Untitled Claude session".into()
            } else {
                preview.clone()
            }
        }),
        native_title,
        preview,
        model: header.model,
        updated_at: metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
        path,
    }))
}

impl Profiles {
    /// Read the native profile directories once. A duplicated UUID in two homes is
    /// intentionally returned twice so the global picker can display both origins.
    pub fn all(&self) -> io::Result<Vec<Session>> {
        self.scan(None)
    }

    fn scan(&self, project: Option<&Path>) -> io::Result<Vec<Session>> {
        let mut rows = Vec::new();
        for (profile, origin) in [
            (&self.subscription, Origin::Subscription),
            (&self.api, Origin::Api),
        ] {
            let directories = if let Some(p) = project {
                candidates(profile, p)?
            } else {
                let root = profile.join("projects");
                if !directory_is_plain(&root).unwrap_or(false) {
                    continue;
                }
                let mut directories = Vec::new();
                let Ok(entries) = fs::read_dir(root) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        directories.push(entry.path());
                    }
                }
                directories
            };
            for directory in directories {
                let Ok(entries) = fs::read_dir(directory) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if !entry.file_type().is_ok_and(|t| t.is_file()) {
                        continue;
                    }
                    let name = entry.file_name();
                    let Some(id) = name
                        .to_str()
                        .and_then(|s| s.strip_suffix(".jsonl"))
                        .filter(|id| valid_session_id(id))
                    else {
                        continue;
                    };
                    match inspect(entry.path(), id, project, origin) {
                        Ok(Some(row)) => rows.push(row),
                        Ok(None) => (),
                        Err(_) => (), // Removed, inaccessible or malformed individual session.
                    }
                }
            }
        }
        Ok(rows)
    }

    pub fn list(&self, project: &Path) -> io::Result<Vec<Session>> {
        let project = project.canonicalize()?;
        let mut by_id = HashMap::<String, Session>::new();
        for row in self.scan(Some(&project))? {
            by_id
                .entry(row.id.clone())
                .and_modify(|existing| {
                    if existing.origin != row.origin {
                        existing.origin = Origin::Ambiguous;
                    }
                })
                .or_insert(row);
        }
        let mut rows: Vec<_> = by_id.into_values().collect();
        rows.sort_unstable_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(rows)
    }

    /// When the journal was last written, without reading it.
    pub fn modified(
        &self,
        project: &Path,
        id: &str,
        origin: Origin,
    ) -> io::Result<std::time::SystemTime> {
        let profile = match origin {
            Origin::Subscription => &self.subscription,
            Origin::Api => &self.api,
            Origin::Ambiguous => return Err(io::ErrorKind::InvalidInput.into()),
        };
        if !valid_session_id(id) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        for dir in candidates(profile, project)? {
            match fs::symlink_metadata(dir.join(format!("{id}.jsonl"))) {
                Ok(m) if m.is_file() => return m.modified(),
                _ => (),
            }
        }
        Err(io::ErrorKind::NotFound.into())
    }

    /// The `entrypoint` the last conversation row was written by: `sdk-cli` for a headless
    /// stream-json client such as OX, `cli` for a terminal. Read from the journal tail, so
    /// it survives restarts of whoever drove the session.
    pub fn last_entrypoint(&self, project: &Path, id: &str, origin: Origin) -> Option<String> {
        let profile = match origin {
            Origin::Subscription => &self.subscription,
            Origin::Api => &self.api,
            Origin::Ambiguous => return None,
        };
        if !valid_session_id(id) {
            return None;
        }
        let dir = candidates(profile, project).ok()?.into_iter().find(|d| {
            fs::symlink_metadata(d.join(format!("{id}.jsonl"))).is_ok_and(|m| m.is_file())
        })?;
        let mut file = open_transcript(&dir.join(format!("{id}.jsonl"))).ok()?;
        let len = file.metadata().ok()?.len();
        let start = len.saturating_sub(SAMPLE);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut tail = BufReader::new(file);
        if start > 0 {
            tail.skip_until(b'\n').ok()?;
        }
        let mut last = None;
        let _ = records(tail, |row| {
            if is_message(&row)
                && row["sessionId"] == id
                && let Some(entry) = row["entrypoint"].as_str()
            {
                last = Some(entry.to_owned());
            }
            Ok(())
        });
        last
    }

    pub fn session(&self, project: &Path, id: &str, origin: Origin) -> io::Result<Session> {
        if !valid_session_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid Claude session ID",
            ));
        }
        let profile = match origin {
            Origin::Subscription => &self.subscription,
            Origin::Api => &self.api,
            Origin::Ambiguous => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Choose a Claude profile",
                ));
            }
        };
        let project = project.canonicalize()?;
        for dir in candidates(profile, &project)? {
            match inspect(dir.join(format!("{id}.jsonl")), id, Some(&project), origin) {
                Ok(Some(row)) => return Ok(row),
                Ok(None) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Claude session does not belong to the selected project and profile",
        ))
    }
}

mod history;
pub(crate) use history::HistoryCursor;
mod sidecars;

mod bindings;
pub use bindings::Bindings;
#[cfg(test)]
mod tests;
