//! Wire types for opening a thread and paging backward from its newest turn.
use super::items::ThreadItem;
use serde::{Deserialize, Deserializer};
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct ReadThread {
    pub thread: Box<ThreadRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumedThread {
    #[serde(flatten, deserialize_with = "super::access::Access::from_settings")]
    pub access: super::access::Access,
    pub thread: ThreadRow,
    pub model_provider: String,
    pub model: String,
    pub service_tier: Option<String>,
    pub reasoning_effort: Option<String>,
    pub initial_turns_page: Option<TurnPage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnPage {
    pub data: Vec<Turn>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Turn {
    pub id: String,
    pub status: String,
    #[serde(default, deserialize_with = "items")]
    pub items: Vec<ThreadItem>,
}

fn items<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<ThreadItem>, D::Error> {
    use serde::de::{SeqAccess, Visitor};
    struct Items;
    impl<'de> Visitor<'de> for Items {
        type Value = Vec<ThreadItem>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("thread items")
        }
        fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
            let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(value) = seq.next_element::<serde_json::Value>()? {
                if let Some(item) = ThreadItem::decode(&value) {
                    items.push(item);
                }
            }
            Ok(items)
        }
    }
    de.deserialize_seq(Items)
}
#[derive(Clone, Debug, PartialEq, Deserialize, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ThreadRow {
    #[serde(skip)]
    pub worker: bool,
    pub parent_thread_id: Option<String>,
    pub agent_nickname: Option<String>,
    pub id: String,
    pub cwd: PathBuf,
    pub model_provider: String,
    pub model: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub preview: String,
    #[serde(skip)]
    pub generated_title: Option<String>,
    #[serde(default)]
    pub updated_at: i64,
    pub status: Option<super::notice::Status>,
    #[serde(skip)]
    pub unread: bool,
    #[serde(skip)]
    pub activity_order: u128,
}
impl ThreadRow {
    pub fn title(&self) -> &str {
        self.name
            .as_deref()
            .and_then(nonblank)
            .or_else(|| self.agent_nickname.as_deref().and_then(nonblank))
            .or_else(|| self.generated_title.as_deref().and_then(nonblank))
            .or_else(|| first_line(&self.preview))
            .unwrap_or("Untitled thread")
    }
}

pub(crate) use crate::transcript::item::{first_line, nonblank};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPage {
    #[serde(deserialize_with = "thread_rows")]
    pub data: Vec<ThreadRow>,
    pub next_cursor: Option<String>,
}

// A stale or partially migrated row must not discard the other sessions on this page.
fn thread_rows<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<ThreadRow>, D::Error> {
    let rows = Vec::<serde_json::Value>::deserialize(de)?;
    Ok(rows
        .into_iter()
        .filter_map(|r| serde_json::from_value(r).ok())
        .collect())
}
