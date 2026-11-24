//! File search is scoped to the selected worktree. Clipboard images own their bytes
//! until the same thread’s turn or steer has been accepted.
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn token(text: &str, caret: usize) -> Option<Range<usize>> {
    let before = text.get(..caret)?;
    let start = before
        .rfind(char::is_whitespace)
        .map_or(0, |i| i + before[i..].chars().next().unwrap().len_utf8());
    before[start..].strip_prefix('@')?;
    let end = text[caret..]
        .find(char::is_whitespace)
        .map_or(text.len(), |n| caret + n);
    Some(start..end)
}
#[derive(Clone, Debug, Deserialize)]
pub struct FileMatch {
    pub root: PathBuf,
    pub path: PathBuf,
    pub match_type: String,
}
#[derive(Debug, Deserialize)]
pub struct Matches {
    pub files: Vec<FileMatch>,
}
impl FileMatch {
    pub fn in_project(&self, project: &Path) -> Option<&Path> {
        if self.root != project || self.match_type != "file" {
            return None;
        }
        let relative = if self.path.is_absolute() {
            self.path.strip_prefix(project).ok()?
        } else {
            self.path.as_path()
        };
        relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
            .then_some(relative)
    }
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ImageInput {
    File(PathBuf),
    Clipboard(String, Arc<str>),
}
impl std::fmt::Debug for ImageInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}
impl ImageInput {
    pub fn label(&self) -> String {
        match self {
            Self::File(p) => p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            Self::Clipboard(mime, _) => format!(
                "Pasted {}",
                mime.trim_start_matches("image/").to_uppercase()
            ),
        }
    }
    pub fn wire(&self) -> Value {
        match self {
            Self::File(p) => json!({"type":"localImage","path":p}),
            Self::Clipboard(_, data) => {
                json!({"type":"image","url":data.as_ref()})
            }
        }
    }
}
pub fn turn_input(text: String, images: Vec<ImageInput>) -> Vec<Value> {
    let mut input = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        input.push(json!({"type":"text","text":text}));
    }
    input.extend(images.iter().map(ImageInput::wire));
    input
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mention_uses_byte_offsets_and_ignores_email() {
        assert_eq!(token("hi 🌱 @src and", 11), Some(8..12));
        assert_eq!(token("a@b", 3), None);
    }
    #[test]
    fn mixed_input_retains_native_image_types() {
        let v = turn_input(
            "look".into(),
            vec![
                ImageInput::File("/a/b.png".into()),
                ImageInput::Clipboard("image/png".into(), Arc::from("data:image/png;base64,AQID")),
            ],
        );
        assert_eq!(v[0]["text"], "look");
        assert_eq!(v[1]["type"], "localImage");
        assert_eq!(v[2]["url"], "data:image/png;base64,AQID");
    }
}
