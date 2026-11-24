use crate::transcript::item::{FileChange, Item};
use serde_json::Value;
use std::fmt::Write;

pub(super) struct Tool {
    pub name: String,
    pub input: Value,
}
impl Tool {
    pub fn item(
        &self,
        session: &str,
        id: &str,
        status: &str,
        result: &Value,
        output: Option<&str>,
    ) -> Item {
        let arg = |key: &str| self.input[key].as_str().unwrap_or_default();
        match self.name.as_str() {
            "Bash" | "PowerShell" => {
                let combined = result["stdout"]
                    .as_str()
                    .into_iter()
                    .chain(result["stderr"].as_str())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                Item::CommandExecution {
                    cwd: None,
                    command: arg("command").into(),
                    status: status.into(),
                    aggregated_output: if combined.is_empty() {
                        output.map(str::to_owned)
                    } else {
                        Some(combined)
                    },
                    exit_code: result["exitCode"]
                        .as_i64()
                        .and_then(|n| i32::try_from(n).ok()),
                }
            }
            "Edit" | "Write" => {
                let path = result["filePath"]
                    .as_str()
                    .unwrap_or(arg("file_path"))
                    .to_owned();
                let staged = result["staged"].as_bool() == Some(true);
                let patch = if status == "completed" && !staged {
                    diff(result)
                } else {
                    String::new()
                };
                Item::FileChange {
                    status: if staged { "staged" } else { status }.into(),
                    changes: vec![FileChange {
                        path,
                        diff: patch.into(),
                    }],
                }
            }
            "Task" | "Agent" => Item::AgentOperation {
                tool: arg("description").to_owned(),
                status: status.into(),
                sender_thread_id: session.into(),
                receiver_thread_ids: vec![id.into()],
            },
            "WebSearch" => Item::WebSearch {
                query: arg("query").into(),
            },
            "TodoWrite" if status != "failed" => Item::Plan {
                text: self.input["todos"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|todo| {
                        let marker = match todo["status"].as_str()? {
                            "completed" => "[x]",
                            "in_progress" => "[>]",
                            "pending" => "[ ]",
                            _ => return None,
                        };
                        Some(format!("{marker} {}", todo["content"].as_str()?))
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
            name if name.starts_with("mcp__") && name[5..].contains("__") => {
                let (server, tool) = name[5..].split_once("__").unwrap();
                Item::McpToolCall {
                    server: server.into(),
                    tool: tool.into(),
                    status: status.into(),
                }
            }
            _ => Item::ToolCall {
                tool: self.name.clone(),
                status: status.into(),
            },
        }
    }
}
fn diff(result: &Value) -> String {
    let mut diff = String::new();
    for h in result["structuredPatch"].as_array().into_iter().flatten() {
        let (Some(old), Some(removed), Some(new), Some(added), Some(lines)) = (
            h["oldStart"].as_u64(),
            h["oldLines"].as_u64(),
            h["newStart"].as_u64(),
            h["newLines"].as_u64(),
            h["lines"].as_array(),
        ) else {
            continue;
        };
        let _ = writeln!(diff, "@@ -{old},{removed} +{new},{added} @@");
        for line in lines.iter().filter_map(Value::as_str) {
            let _ = writeln!(diff, "{line}");
        }
    }
    if diff.is_empty()
        && result["type"] == "create"
        && let Some(text) = result["content"].as_str().filter(|text| !text.is_empty())
    {
        let _ = writeln!(diff, "@@ -0,0 +1,{} @@", text.lines().count());
        for line in text.lines() {
            let _ = writeln!(diff, "+{line}");
        }
        if !text.ends_with('\n') {
            diff.push_str("\\ No newline at end of file\n");
        }
    }
    diff
}
pub(super) fn text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_owned());
    }
    let parts = content
        .as_array()?
        .iter()
        .filter(|v| v["type"] == "text")
        .filter_map(|v| v["text"].as_str())
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}
