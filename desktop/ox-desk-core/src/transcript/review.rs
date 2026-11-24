//! Turn-scoped changes and plans shared by conversation runtimes.
use super::item::{FileChange, Item};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Reviews {
    order: VecDeque<String>,
    turns: HashMap<String, TurnReview>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Step {
    pub text: String,
    pub status: String,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TurnReview {
    aggregate: Option<Vec<FileChange>>,
    // Multiple edits to one path are kept in request order.
    items: Vec<(String, Vec<FileChange>)>,
    pub explanation: Option<String>,
    pub steps: Vec<Step>,
    pub plan_text: String,
}

impl Reviews {
    pub(crate) fn turn(&mut self, id: &str, history: bool) -> &mut TurnReview {
        self.turns.entry(id.to_owned()).or_insert_with(|| {
            if history {
                self.order.push_back(id.to_owned());
            } else {
                self.order.push_front(id.to_owned());
            }
            TurnReview::default()
        })
    }

    pub(crate) fn initial_history_order<'a>(
        &mut self,
        page: impl Iterator<Item = &'a str> + Clone,
    ) {
        // Live notifications can mention an older turn while the first page is in flight.
        // History establishes their relative order. Newly streamed turns absent from it stay first.
        let included: HashSet<&str> = page.clone().collect();
        self.order.retain(|id| !included.contains(id.as_str()));
        self.order.extend(page.map(str::to_owned));
    }

    pub fn turns(&self) -> impl Iterator<Item = (&str, &TurnReview)> {
        self.order.iter().map(|id| (id.as_str(), &self.turns[id]))
    }

    pub fn get(&self, turn: &str) -> Option<&TurnReview> {
        self.turns.get(turn)
    }
}

impl TurnReview {
    pub fn has_content(&self) -> bool {
        self.explanation.is_some()
            || !self.steps.is_empty()
            || !self.plan_text.is_empty()
            || self.aggregate.as_ref().map_or_else(
                || self.items.iter().any(|(_, files)| !files.is_empty()),
                |files| !files.is_empty(),
            )
    }

    pub(crate) fn diff(&mut self, diff: String) {
        self.aggregate = Some(split_patch(&diff));
    }

    pub(crate) fn files(&mut self, item: &str, changes: &[FileChange], historical: bool) {
        if let Some((_, previous)) = self.items.iter_mut().find(|(id, _)| id == item) {
            if !historical {
                changes.clone_into(previous);
            }
        } else {
            self.items.push((item.into(), changes.to_owned()));
        }
    }

    pub(crate) fn observe_item(&mut self, id: &str, item: &Item, historical: bool) {
        match item {
            Item::FileChange { changes, status }
                if matches!(status.as_str(), "completed" | "inProgress") =>
            {
                self.files(id, changes, historical)
            }
            Item::FileChange { .. } if !historical => self.files(id, &[], false),
            Item::Plan { text } if !historical || self.plan_text.is_empty() => {
                self.plan_text.clone_from(text)
            }
            _ => {}
        }
    }

    pub fn patch(&self) -> String {
        if let Some(files) = &self.aggregate {
            return files.iter().map(|file| file.diff.as_ref()).collect();
        }
        let mut result = String::new();
        for file in self
            .items
            .iter()
            .flat_map(|(_, files)| files)
            .filter(|f| !f.diff.is_empty())
        {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&file.diff);
        }
        result
    }

    /// File groups borrow their patches so an unopened/closed file need not copy its text.
    pub fn file_groups(&self) -> Vec<(&str, Vec<&FileChange>)> {
        let patches: &mut dyn Iterator<Item = &FileChange> = match &self.aggregate {
            Some(files) => &mut files.iter(),
            None => &mut self.items.iter().flat_map(|(_, files)| files),
        };
        let mut groups = Vec::<(&str, Vec<&FileChange>)>::new();
        let mut index = HashMap::<&str, usize>::new();
        for patch in patches {
            let position = *index.entry(&patch.path).or_insert_with(|| {
                groups.push((&patch.path, Vec::new()));
                groups.len() - 1
            });
            groups[position].1.push(patch);
        }
        groups
    }
}

fn header_path(line: &str, prefix: &str) -> Option<String> {
    let raw = line
        .strip_prefix(prefix)?
        .trim_end_matches(['\r', '\n'])
        .split('\t')
        .next()?;
    let path = raw.trim_matches('"');
    if path.is_empty() || path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_owned(),
    )
}

fn section_name(section: &str) -> String {
    let headers = || section.lines().take_while(|line| !line.starts_with("@@"));
    headers()
        .find_map(|line| header_path(line, "+++ "))
        .or_else(|| headers().find_map(|line| header_path(line, "--- ")))
        .or_else(|| headers().find_map(|line| line.strip_prefix("rename to ").map(str::to_owned)))
        .or_else(|| {
            section
                .lines()
                .next()?
                .rsplit_once(" b/")
                .map(|(_, p)| p.trim_end_matches('"').to_owned())
        })
        .unwrap_or_else(|| "Patch".into())
}

fn hunk_size(line: &str) -> Option<(usize, usize)> {
    let mut fields = line.strip_prefix("@@ ")?.split_ascii_whitespace();
    fn count(field: &str) -> Option<usize> {
        field
            .split_once(',')
            .map_or(Some(1), |(_, length)| length.parse().ok())
    }
    Some((
        count(fields.next()?.strip_prefix('-')?)?,
        count(fields.next()?.strip_prefix('+')?)?,
    ))
}

fn split_patch(diff: &str) -> Vec<FileChange> {
    if diff.is_empty() {
        return vec![];
    }
    let mut boundaries = Vec::new();
    let mut offset = 0;
    let mut hunk = (0usize, 0usize);
    let mut lines = diff.split_inclusive('\n').peekable();
    let git = diff.lines().any(|line| line.starts_with("diff --git "));
    while let Some(line) = lines.next() {
        if (git && line.starts_with("diff --git "))
            || (!git
                && hunk == (0, 0)
                && line.starts_with("--- ")
                && lines.peek().is_some_and(|s| s.starts_with("+++ ")))
        {
            boundaries.push(offset);
        }
        if !git {
            if let Some(lengths) = hunk_size(line) {
                hunk = lengths;
            } else {
                match line.as_bytes().first() {
                    Some(b' ') => {
                        hunk.0 = hunk.0.saturating_sub(1);
                        hunk.1 = hunk.1.saturating_sub(1);
                    }
                    Some(b'-') => hunk.0 = hunk.0.saturating_sub(1),
                    Some(b'+') => hunk.1 = hunk.1.saturating_sub(1),
                    _ => {}
                }
            }
        }
        offset += line.len();
    }
    // Preserve leading format/preamble text when copying the first section.
    if let Some(first) = boundaries.first_mut() {
        *first = 0;
    } else {
        boundaries.push(0);
    }
    boundaries.push(diff.len());
    boundaries
        .windows(2)
        .map(|p| {
            let text = &diff[p[0]..p[1]];
            FileChange {
                path: section_name(text),
                diff: text.into(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;
    use serde_json::json;

    #[test]
    fn historical_files_and_plans_are_recovered_without_overwriting_live_patches() {
        let mut t = Transcript::default();
        t.reviews.turn("new", false).files(
            "a",
            &[FileChange {
                path: "first.rs".into(),
                diff: "current".into(),
            }],
            false,
        );
        t.add_history(serde_json::from_value(json!({"data":[
             {"id":"new","status":"inProgress","items":[{"type":"fileChange","id":"a","status":"inProgress","changes":[{"path":"first.rs","diff":"older snapshot"}]}]},
             {"id":"old","status":"completed","items":[
              {"type":"fileChange","id":"a","status":"completed","changes":[{"path":"same.rs","diff":"one"}]},
              {"type":"fileChange","id":"b","status":"completed","changes":[{"path":"same.rs","diff":"two"}]},
              {"type":"plan","id":"p","text":"Archived plan"}
             ]}
        ]})).unwrap()).unwrap();
        let mut turns = t.reviews.turns();
        let (new, r) = turns.next().unwrap();
        assert_eq!(new, "new");
        assert_eq!(&*r.file_groups()[0].1[0].diff, "current");
        let (old, r) = turns.next().unwrap();
        assert_eq!(old, "old");
        assert_eq!(r.plan_text, "Archived plan");
        assert_eq!(
            r.file_groups()[0]
                .1
                .iter()
                .map(|p| p.diff.as_ref())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn addition_deletion_and_rename_keep_exact_file_patches() {
        let add = "diff --git a/with space b/with space\n--- /dev/null\n+++ b/with space\n@@ -0,0 +1 @@\n+hello\n";
        let delete = "diff --git a/gone b/gone\n--- a/gone\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n";
        let rename =
            "diff --git a/old b/new\nsimilarity index 100%\nrename from old\nrename to new";
        let patches = split_patch(&format!("{add}{delete}{rename}"));
        assert_eq!(
            patches.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
            ["with space", "gone", "new"]
        );
        assert_eq!(
            patches.iter().map(|p| p.diff.as_ref()).collect::<Vec<_>>(),
            [add, delete, rename]
        );
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;
    #[test]
    fn old_notifications_do_not_displace_the_current_turn() {
        let mut state = Reviews::default();
        state.turn("old", false).diff("older patch".into());
        state.turn("current", true);
        state.initial_history_order(["current", "old"].into_iter());
        state.turn("old", false).diff("late replacement".into());
        assert_eq!(state.turns().next().unwrap().0, "current");
        assert!(!state.turns().next().unwrap().1.has_content());
        state.turn("background", false);
        state.initial_history_order(["current", "old"].into_iter());
        assert_eq!(
            state.turns().map(|(k, _)| k).collect::<Vec<_>>(),
            ["background", "current", "old"]
        );
    }
    #[test]
    fn header_shaped_content_in_unified_hunks_remains_in_its_file() {
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n--- old text\n+++ new text\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let sections = split_patch(patch);
        assert_eq!(
            sections.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(),
            ["a.txt", "b.txt"]
        );
        assert_eq!(
            sections.iter().map(|x| x.diff.as_ref()).collect::<String>(),
            patch
        );
    }
}
