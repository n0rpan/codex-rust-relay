//! Data displayed by a transcript, independent of the CLI that produced it.
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct Content {
    pub text: Option<String>,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct FileChange {
    pub path: String,
    #[serde(deserialize_with = "patch_text")]
    pub diff: std::sync::Arc<str>,
}

fn patch_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<std::sync::Arc<str>, D::Error> {
    String::deserialize(d).map(Into::into)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Item {
    UserMessage {
        content: Vec<Content>,
    },
    AgentMessage {
        text: String,
        is_update: bool,
    },
    Reasoning {
        summary: Vec<String>,
    },
    CommandExecution {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<std::path::PathBuf>,
        command: String,
        status: String,
        aggregated_output: Option<String>,
        exit_code: Option<i32>,
    },
    FileChange {
        status: String,
        changes: Vec<FileChange>,
    },
    AgentOperation {
        tool: String,
        status: String,
        sender_thread_id: String,
        receiver_thread_ids: Vec<String>,
    },
    McpToolCall {
        status: String,
        server: String,
        tool: String,
    },
    ToolCall {
        status: String,
        tool: String,
    },
    WebSearch {
        query: String,
    },
    Plan {
        text: String,
    },
    Unknown {
        kind: String,
        status: String,
    },
}

impl Item {
    pub(crate) fn user_prompt_line(&self) -> Option<&str> {
        let Self::UserMessage { content } = self else {
            return None;
        };
        content
            .iter()
            .filter_map(|part| part.text.as_deref())
            .find_map(first_line)
    }
}

pub(crate) fn first_line(text: &str) -> Option<&str> {
    text.lines().find_map(nonblank)
}
pub(crate) fn nonblank(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty()).then_some(text)
}
