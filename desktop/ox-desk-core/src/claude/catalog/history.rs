//! Read the parent journal and the sidechains linked by its tool calls. A live viewer
//! keeps the link graph and one complete-line cursor per journal between refreshes.
use super::*;
use std::{collections::HashSet, os::unix::fs::MetadataExt, time::SystemTime};

#[derive(Clone, Copy, PartialEq)]
struct Stamp {
    device: u64,
    inode: u64,
    length: u64,
    modified: Option<SystemTime>,
}

impl From<&fs::Metadata> for Stamp {
    fn from(meta: &fs::Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            length: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}

#[derive(Clone, Copy)]
struct Position {
    stamp: Stamp,
    offset: u64,
}

struct Child {
    agent: String,
    tool: String,
    parent_tool: Option<String>,
    description: Value,
    position: Option<Position>,
}

#[derive(Default)]
struct Links {
    children: Vec<Child>,
    agents: HashSet<String>,
    /// Tool use ID -> enclosing agent tool use ID, while waiting for a sidecar or result.
    pending: HashMap<String, Option<String>>,
    sidecars: sidecars::Sidecars,
}

impl Links {
    fn child(
        &mut self,
        agent: String,
        tool: String,
        parent_tool: Option<String>,
        description: Value,
    ) {
        self.pending.remove(&tool);
        if self.agents.insert(agent.clone()) {
            self.children.push(Child {
                agent,
                tool,
                parent_tool,
                description,
                position: None,
            });
        }
    }
}

#[derive(Default)]
pub(crate) struct HistoryCursor {
    root: Option<Position>,
    links: Links,
}

impl HistoryCursor {
    pub(crate) fn last_child_write(&self) -> Option<SystemTime> {
        self.links
            .children
            .iter()
            .filter_map(|c| c.position?.stamp.modified)
            .max()
    }

    pub(crate) fn watches_children(&self) -> bool {
        !self.links.children.is_empty() || !self.links.pending.is_empty()
    }
}

impl Session {
    pub fn history(&self, consume: impl FnMut(Value) -> io::Result<()>) -> io::Result<()> {
        self.history_from(&mut HistoryCursor::default(), consume)
            .map(drop)
    }

    /// Returns false when any already-read file disappeared, shrank, or was replaced.
    /// Discard the in-progress projection in that case and reload from the beginning.
    pub(crate) fn history_from(
        &self,
        cursor: &mut HistoryCursor,
        mut consume: impl FnMut(Value) -> io::Result<()>,
    ) -> io::Result<bool> {
        if self.origin == Origin::Ambiguous {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Choose the original Claude profile before opening history",
            ));
        }
        let mut replay = Replay {
            session: self,
            links: &mut cursor.links,
            consume: &mut consume,
        };
        if !replay.read(&self.path, None, &mut cursor.root)? {
            return Ok(false);
        }
        if replay.links.children.is_empty() && replay.links.pending.is_empty() {
            return Ok(true);
        }
        let session_dir = self.path.with_extension("");
        let directory = session_dir.join("subagents");
        if !directory_is_plain(&session_dir)? || !directory_is_plain(&directory)? {
            return Ok(replay.links.children.iter().all(|c| c.position.is_none()));
        }
        let mut known = std::mem::take(&mut replay.links.children);
        let mut index = 0;
        loop {
            // Reading a child can discover additional nested jobs in this same directory.
            for child in &mut known[index..] {
                let path = directory.join(format!("agent-{}.jsonl", child.agent));
                let mut position = child.position;
                if !replay.read(&path, Some(child), &mut position)? {
                    return Ok(false);
                }
                child.position = position;
            }
            if !replay.links.pending.is_empty() {
                let found = replay
                    .links
                    .sidecars
                    .discover(&directory, &replay.links.pending)?;
                for (agent, tool) in found {
                    if let Some(parent_tool) = replay.links.pending.remove(&tool) {
                        replay.links.child(agent, tool, parent_tool, Value::Null);
                    }
                }
            }
            if replay.links.children.is_empty() {
                break;
            }
            index = known.len();
            known.append(&mut replay.links.children);
        }
        replay.links.children = known;
        Ok(true)
    }
}

struct Replay<'a> {
    session: &'a Session,
    links: &'a mut Links,
    consume: &'a mut dyn FnMut(Value) -> io::Result<()>,
}

impl Replay<'_> {
    fn read(
        &mut self,
        path: &Path,
        child: Option<&Child>,
        position: &mut Option<Position>,
    ) -> io::Result<bool> {
        // One stat for a quiet journal, including a partially written last line.
        match fs::symlink_metadata(path) {
            Ok(m) if m.is_file() => {
                if position.is_some_and(|p| p.stamp == Stamp::from(&m)) {
                    return Ok(true);
                }
            }
            Ok(_) if child.is_some() => return Ok(position.is_none()),
            Err(e) if child.is_some() && e.kind() == io::ErrorKind::NotFound => {
                return Ok(position.is_none());
            }
            Err(e) => return Err(e),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Expected a regular Claude transcript",
                ));
            }
        }
        let mut file = match open_transcript(path) {
            Err(e) if child.is_some() && e.kind() == io::ErrorKind::NotFound => {
                return Ok(position.is_none());
            }
            other => other?,
        };
        // Use the identity and size of the opened file. Appends during this read belong to
        // the next tick, so a writer splitting a JSON record cannot advance our cursor.
        let stamp = Stamp::from(&file.metadata()?);
        if position.is_some_and(|p| {
            p.stamp.device != stamp.device
                || p.stamp.inode != stamp.inode
                || stamp.length <= p.stamp.length && stamp != p.stamp
        }) {
            return Ok(false);
        }
        let start = position.map_or(0, |p| p.offset);
        if start > 0 {
            file.seek(SeekFrom::Start(start))?;
        }
        if position.is_none()
            && let Some(c) = child
        {
            (self.consume)(
                serde_json::json!({"type":"system","subtype":"task_started","task_type":"local_agent",
                "task_id":c.agent, "session_id":self.session.id, "tool_use_id":c.tool,
                "description":c.description, "parent_tool_use_id":c.parent_tool}),
            )?;
        }
        let mut confirmed = start > 0;
        let reader = BufReader::new(file.take(stamp.length - start));
        let offset = records_at(reader, start, child.is_some(), |mut row| {
            if !matches!(row["type"].as_str(), Some("user" | "assistant"))
                || row["sessionId"] != self.session.id
            {
                return Ok(());
            }
            let sidechain = row["isSidechain"] == true;
            if sidechain != child.is_some() {
                return Ok(());
            }
            if let Some(child) = child {
                if row["agentId"] != child.agent {
                    return Ok(());
                }
                row["parent_tool_use_id"] = Value::String(child.tool.clone());
                row["agent_id"] = Value::String(child.agent.clone());
            }
            // The project is the cwd of the first conversation row of the main
            // journal (where Claude Code started), the same rule `Header` uses.
            // Later rows follow `cd`, and subagents run in their own directories.
            if !confirmed
                && child.is_none()
                && let Some(cwd) = row["cwd"].as_str()
            {
                if Path::new(cwd) != self.session.project {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Claude session project changed",
                    ));
                }
                confirmed = true;
            }
            row["session_id"] = Value::String(self.session.id.clone());
            if let Some(result) = row.as_object_mut().and_then(|r| r.remove("toolUseResult")) {
                row["tool_use_result"] = result;
            }
            // Follow only the named child of this tool result. The outputFile
            // field may point outside the profile and must never be opened.
            let spawned = row["tool_use_result"]["agentId"]
                .as_str()
                .filter(|id| sidecars::valid_agent_id(id))
                .zip(
                    row["message"]["content"]
                        .as_array()
                        .and_then(|blocks| blocks.iter().find_map(|b| b["tool_use_id"].as_str())),
                )
                .map(|(agent, tool)| {
                    (
                        agent.to_owned(),
                        tool.to_owned(),
                        row["tool_use_result"]["description"].clone(),
                    )
                });
            for tool in sidecars::agent_calls(&row) {
                self.links
                    .pending
                    .insert(tool.to_owned(), child.map(|c| c.tool.clone()));
            }
            (self.consume)(row)?;
            if let Some((agent, tool, description)) = spawned {
                self.links
                    .child(agent, tool, child.map(|c| c.tool.clone()), description);
            }
            Ok(())
        })?;
        *position = Some(Position { stamp, offset });
        Ok(true)
    }
}
