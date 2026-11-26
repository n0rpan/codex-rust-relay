//! A foreground agent can write its journal before its caller receives a result.
//! Its launch metadata names the originating tool call.
use super::{MAX_RECORD, directory_is_plain, open_transcript};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    fs, io,
    io::{BufReader, Read},
    os::unix::fs::MetadataExt,
    path::Path,
};

pub(super) fn valid_agent_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 96 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

pub(super) fn agent_calls(row: &Value) -> impl Iterator<Item = &str> {
    row["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |block| {
            row["type"] == "assistant"
                && block["type"] == "tool_use"
                && matches!(block["name"].as_str(), Some("Agent" | "Task"))
        })
        .filter_map(|block| block["id"].as_str().filter(|id| !id.is_empty()))
}

#[derive(Deserialize)]
struct Launch {
    #[serde(rename = "toolUseId")]
    tool: String,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    change: (i64, i64),
}

impl From<&fs::Metadata> for Stamp {
    fn from(m: &fs::Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            change: (m.ctime(), m.ctime_nsec()),
        }
    }
}

#[derive(Default)]
struct Sidecar {
    stamp: Option<Stamp>,
    tool: Option<String>,
}

impl Sidecar {
    fn refresh(&mut self, path: &Path) {
        let Ok(meta) = fs::symlink_metadata(path) else {
            *self = Self::default();
            return;
        };
        let stamp = Stamp::from(&meta);
        if self.stamp.as_ref() == Some(&stamp) {
            return;
        }
        self.tool = None;
        self.stamp = None;
        if !meta.is_file() || meta.len() > MAX_RECORD as u64 {
            self.stamp = Some(stamp);
            return;
        }
        let Ok(file) = open_transcript(path) else {
            return;
        };
        let reader = BufReader::new(file.take(MAX_RECORD as u64 + 1));
        match serde_json::from_reader::<_, Launch>(reader) {
            Ok(launch) => self.tool = Some(launch.tool),
            Err(e) if e.is_io() => return,
            // The writer may be mid-JSON. Retry when this file changes.
            Err(_) => (),
        }
        self.stamp = Some(stamp);
    }
}

#[derive(Default)]
pub(super) struct Sidecars {
    directory: Option<Stamp>,
    by_agent: HashMap<String, Sidecar>,
}

impl Sidecars {
    pub(super) fn discover(
        &mut self,
        children: &Path,
        pending: &HashMap<String, Option<String>>,
    ) -> io::Result<Vec<(String, String)>> {
        if pending.is_empty() {
            return Ok(vec![]);
        }
        let Some(session) = children.parent() else {
            return Ok(vec![]);
        };
        if !directory_is_plain(session)? || !directory_is_plain(children)? {
            *self = Self::default();
            return Ok(vec![]);
        }
        let directory = Stamp::from(&fs::symlink_metadata(children)?);
        let changed = self.directory.as_ref() != Some(&directory);
        if changed {
            let mut entries = HashMap::new();
            for entry in fs::read_dir(children)? {
                let entry = entry?;
                let name = entry.file_name();
                if let Some(agent) = name
                    .to_str()
                    .and_then(|s| s.strip_prefix("agent-"))
                    .and_then(|s| s.strip_suffix(".meta.json"))
                    .filter(|id| valid_agent_id(id))
                    && entry.file_type().is_ok_and(|kind| kind.is_file())
                {
                    entries.insert(
                        agent.to_owned(),
                        self.by_agent.remove(agent).unwrap_or_default(),
                    );
                }
            }
            self.directory = Some(directory);
            self.by_agent = entries;
        }
        let mut matches = HashMap::new();
        for (agent, sidecar) in &mut self.by_agent {
            if changed || sidecar.tool.is_none() {
                sidecar.refresh(&children.join(format!("agent-{agent}.meta.json")));
            }
            if let Some(tool) = &sidecar.tool
                && pending.contains_key(tool)
            {
                // Only an unambiguous match identifies a child before its result arrives.
                matches
                    .entry(tool)
                    .and_modify(|found| *found = None)
                    .or_insert(Some(agent));
            }
        }
        Ok(matches
            .into_iter()
            .filter_map(|(tool, agent)| agent.map(|agent| (agent.clone(), tool.clone())))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;

    #[test]
    fn match_only_recorded_agent_invocations_and_retry_incomplete_metadata() {
        let root = tempfile::tempdir().unwrap();
        let children = root.path().join("session/subagents");
        let mut sidecars = Sidecars::default();
        let row = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Bash","id":"other"},
            {"type":"text","name":"Agent","id":"text"},
            {"type":"tool_use","name":"Agent","id":"foreground"},
            {"type":"tool_use","name":"Task","id":"legacy"}
        ]}});
        let pending: HashMap<_, _> = agent_calls(&row)
            .map(|tool| (tool.to_owned(), None))
            .collect();
        assert_eq!(pending.len(), 2);
        assert!(sidecars.discover(&children, &pending).unwrap().is_empty());
        fs::create_dir_all(&children).unwrap();
        let meta = children.join("agent-child.meta.json");
        fs::write(&meta, r#"{"toolUseId":"#).unwrap();
        fs::write(
            children.join("agent-other.meta.json"),
            r#"{"toolUseId":"other"}"#,
        )
        .unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, r#"{"toolUseId":"legacy"}"#).unwrap();
        symlink(outside, children.join("agent-symlink.meta.json")).unwrap();
        assert!(sidecars.discover(&children, &pending).unwrap().is_empty());
        fs::write(&meta, r#"{"toolUseId":"foreground"}"#).unwrap();
        let matches = vec![("child".to_owned(), "foreground".to_owned())];
        assert_eq!(sidecars.discover(&children, &pending).unwrap(), matches);
        assert_eq!(sidecars.discover(&children, &pending).unwrap(), matches);

        fs::write(
            children.join("agent-duplicate.meta.json"),
            r#"{"toolUseId":"foreground"}"#,
        )
        .unwrap();
        assert!(sidecars.discover(&children, &pending).unwrap().is_empty());
    }
}
