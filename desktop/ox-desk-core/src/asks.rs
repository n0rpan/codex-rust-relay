//! Runtime-neutral approval sheets, choices, and form fields.
use crate::runtime::RequestId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io};

#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct Answer(pub(crate) Value);

#[derive(Debug)]
pub struct Decision {
    pub label: String,
    pub detail: Option<String>,
    pub(crate) answer: Answer,
}
impl Decision {
    pub(crate) fn new(label: impl Into<String>, answer: Value) -> Self {
        Self {
            label: label.into(),
            detail: None,
            answer: Answer(answer),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub question: String,
    pub header: String,
    #[serde(default)]
    pub is_other: bool,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default)]
    pub multi_select: bool,
    pub options: Option<Vec<Choice>>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Choice {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CommandApproval {
    pub command: String,
    pub cwd: Option<std::path::PathBuf>,
    pub decline: usize,
}

#[derive(Debug)]
pub struct Ask {
    pub command: Option<CommandApproval>,
    pub id: RequestId,
    pub thread: Option<String>,
    pub turn: Option<String>,
    pub item: Option<String>,
    pub(crate) title: String,
    pub detail: String,
    pub questions: Vec<Question>,
    pub(crate) decisions: Vec<Decision>,
}
/// Request-id prefix of the approval the host raises for a worker that waits
/// for YOLO acceptance on its parent session (see the host's yolo module).
pub const YOLO_REQUEST_PREFIX: &str = "ox-yolo-";

impl Ask {
    pub fn decisions(&self) -> &[Decision] {
        &self.decisions
    }
    /// Whether this approval is a worker waiting for YOLO acceptance.
    pub fn waits_for_yolo(&self) -> bool {
        matches!(&self.id, RequestId::Text(id) if id.starts_with(YOLO_REQUEST_PREFIX))
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub(crate) fn form_answer(&self, values: &[String]) -> io::Result<Answer> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Choose an answer for every question",
            )
        };
        if self.questions.is_empty() || self.questions.len() != values.len() {
            return Err(invalid());
        }
        let mut answers = BTreeMap::new();
        for (q, value) in self.questions.iter().zip(values) {
            if value.trim().is_empty()
                || (!q.is_other
                    && q.options.as_ref().is_some_and(|opts| {
                        !opts.is_empty() && !opts.iter().any(|opt| opt.label == *value)
                    }))
            {
                return Err(invalid());
            }
            answers.insert(&q.id, json!({"answers":[value]}));
        }
        Ok(Answer(json!({"answers":answers})))
    }
}
