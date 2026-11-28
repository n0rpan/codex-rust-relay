//! ACP v1 session payloads and Antigravity's presentation of commands and checklists.
use crate::{
    runtime::Access,
    slash::Capabilities,
    store::routes_thread::ModelInfo,
    transcript::item::{FileChange, Item},
};
use serde_json::{Value, json};
use std::{collections::HashSet, path::PathBuf, sync::Arc};

#[derive(Default)]
pub(super) struct SessionOptions {
    modes: Option<Selector>,
    models: Option<Selector>,
    legacy_modes: Option<Selector>,
    legacy_models: Option<Selector>,
}

struct Selector {
    config_id: Option<String>,
    current: String,
    values: Vec<(String, String)>,
}

impl Selector {
    fn from_config(raw: &Value) -> Option<Self> {
        // Antigravity previously omitted the discriminator for select options.
        if raw["type"].as_str().is_some_and(|kind| kind != "select") {
            return None;
        }
        let options = raw["options"].as_array()?;
        let values = options.iter().flat_map(|entry| {
            entry["options"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_else(|| std::slice::from_ref(entry))
        });
        Some(Self {
            config_id: Some(raw["id"].as_str()?.to_owned()),
            current: raw["currentValue"].as_str()?.to_owned(),
            values: choices(values, "value"),
        })
    }

    fn from_legacy(raw: &Value, current: &str, available: &str, id: &str) -> Option<Self> {
        Some(Self {
            config_id: None,
            current: raw[current].as_str()?.to_owned(),
            values: choices(raw[available].as_array()?.iter(), id),
        })
    }

    fn setting(
        &self,
        session: &str,
        id: &str,
        method: &'static str,
        field: &str,
    ) -> Option<(&'static str, Value)> {
        if !self.values.iter().any(|(value, _)| value == id) {
            return None;
        }
        match &self.config_id {
            Some(config) => Some((
                "session/set_config_option",
                json!({"sessionId": session, "configId": config, "value": id}),
            )),
            None => Some((method, json!({"sessionId": session, (field): id}))),
        }
    }
}

fn choices<'a>(raw: impl Iterator<Item = &'a Value>, field: &str) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    raw.filter_map(|entry| {
        let value = entry[field].as_str().filter(|value| !value.is_empty())?;
        let name = entry["name"].as_str().unwrap_or(value);
        seen.insert(value).then(|| (value.into(), name.into()))
    })
    .collect()
}

fn select_config(options: &[Value], category: &str) -> Option<Selector> {
    options
        .iter()
        .filter(|option| {
            option["category"] == category
                || (option["category"].is_null() && option["id"] == category)
        })
        .find_map(Selector::from_config)
}

fn native_access(id: &str) -> Option<Access> {
    Some(match id {
        "default" => Access::Default,
        "plan" => Access::Plan,
        "accept-edits" => Access::AcceptEdits,
        _ => return None,
    })
}

impl SessionOptions {
    pub(super) fn apply(&mut self, response: &Value) {
        if let Some(options) = response["configOptions"].as_array() {
            // The response always carries the entire set, including unrelated options.
            self.modes = select_config(options, "mode");
            self.models = select_config(options, "model");
        }
        if let Some(modes) = response.get("modes") {
            self.legacy_modes =
                Selector::from_legacy(modes, "currentModeId", "availableModes", "id");
        }
        if let Some(models) = response.get("models") {
            self.legacy_models =
                Selector::from_legacy(models, "currentModelId", "availableModels", "modelId");
        }
        if let Some(id) = response["currentModeId"].as_str() {
            for modes in [&mut self.modes, &mut self.legacy_modes]
                .into_iter()
                .flatten()
            {
                modes.current = id.to_owned();
            }
        }
        if let Some(id) = response["currentModelId"].as_str() {
            for models in [&mut self.models, &mut self.legacy_models]
                .into_iter()
                .flatten()
            {
                models.current = id.to_owned();
            }
        }
    }

    fn modes(&self) -> Option<&Selector> {
        self.modes.as_ref().or(self.legacy_modes.as_ref())
    }

    fn models(&self) -> Option<&Selector> {
        self.models.as_ref().or(self.legacy_models.as_ref())
    }

    pub(super) fn access(&self) -> Option<Access> {
        native_access(&self.modes()?.current)
    }

    pub(super) fn available_access(&self) -> Vec<Access> {
        self.modes()
            .into_iter()
            .flat_map(|modes| modes.values.iter())
            .filter_map(|(id, _)| native_access(id))
            .collect()
    }

    pub(super) fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities {
            provider: super::PROVIDER.into(),
            ..Capabilities::default()
        };
        if let Some(models) = self.models() {
            caps.model.clone_from(&models.current);
            caps.models = Arc::new(
                models
                    .values
                    .iter()
                    .enumerate()
                    .map(|(rank, (id, label))| {
                        (
                            id.clone(),
                            ModelInfo {
                                label: label.clone(),
                                rank: u32::try_from(rank).ok(),
                                ..ModelInfo::default()
                            },
                        )
                    })
                    .collect(),
            );
        }
        caps
    }

    pub(super) fn set_access(
        &self,
        session: &str,
        access: Access,
    ) -> Option<(&'static str, Value)> {
        let modes = self.modes()?;
        let (id, _) = modes
            .values
            .iter()
            .find(|(id, _)| native_access(id) == Some(access))?;
        modes.setting(session, id, "session/set_mode", "modeId")
    }

    pub(super) fn set_model(&self, session: &str, model: &str) -> Option<(&'static str, Value)> {
        self.models()?
            .setting(session, model, "session/set_model", "modelId")
    }

    pub(super) fn confirm_legacy_access(&mut self, access: Access) {
        if let Some(modes) = &mut self.legacy_modes
            && let Some((id, _)) = modes
                .values
                .iter()
                .find(|(id, _)| native_access(id) == Some(access))
        {
            modes.current.clone_from(id);
        }
    }

    pub(super) fn confirm_legacy_model(&mut self, model: &str) {
        if let Some(models) = &mut self.legacy_models {
            models.current = model.into();
        }
    }
}

pub(super) enum Update {
    AgentText {
        message_id: Option<String>,
        text: String,
    },
    Thought {
        message_id: Option<String>,
        text: String,
    },
    UserText {
        message_id: Option<String>,
        text: String,
    },
    Plan(String),
    Tool(ToolUpdate),
    Options(Value),
    Metadata {
        title: Option<String>,
    },
    Usage {
        used: u64,
        size: u64,
    },
}

pub(super) fn update(params: Value) -> Option<(String, Update)> {
    let session = params["sessionId"].as_str()?.to_owned();
    let mut data = params;
    let update = data.get_mut("update")?.take();
    let tag = update["sessionUpdate"].as_str()?;
    let decoded = match tag {
        "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
            let content = &update["content"];
            if tag == "user_message_chunk" && content["resource"]["uri"] == super::TURN_CONTEXT_URI
            {
                // Sender and pinned memory are model context, not the operator's draft.
                return None;
            }
            let message_id = update["messageId"].as_str().map(str::to_owned);
            let text = content_text(content)?.to_owned();
            match tag {
                "agent_message_chunk" => Update::AgentText { message_id, text },
                "agent_thought_chunk" => Update::Thought { message_id, text },
                _ => Update::UserText { message_id, text },
            }
        }
        "plan" => Update::Plan(checklist(&update["entries"], "content")?),
        "tool_call" | "tool_call_update" => Update::Tool(ToolUpdate::new(update)?),
        "config_option_update" | "current_mode_update" | "current_model_update" => {
            Update::Options(update)
        }
        "session_info_update" => Update::Metadata {
            title: match update.get("title")? {
                Value::String(text) => Some(text.clone()),
                Value::Null => None,
                _ => return None,
            },
        },
        "usage_update" => Update::Usage {
            used: update["used"].as_u64()?,
            size: update["size"].as_u64()?,
        },
        _ => return None,
    };
    Some((session, decoded))
}

fn content_text(content: &Value) -> Option<&str> {
    match content["type"].as_str()? {
        "text" => content["text"].as_str(),
        "resource_link" | "image" | "audio" => content["uri"].as_str(),
        "resource" => content["resource"]["text"].as_str(),
        _ => None,
    }
}

fn checklist(items: &Value, field: &str) -> Option<String> {
    Some(
        items
            .as_array()?
            .iter()
            .filter_map(|step| {
                let text = step[field].as_str()?;
                let mark = match step["status"].as_str() {
                    Some("completed" | "done") => "[x]",
                    Some("in_progress") => "[ ] **In progress** —",
                    _ => "[ ]",
                };
                Some(format!("- {mark} {text}"))
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

pub(super) struct ToolUpdate {
    id: String,
    fields: serde_json::Map<String, Value>,
}

impl ToolUpdate {
    fn new(update: Value) -> Option<Self> {
        let Value::Object(fields) = update else {
            return None;
        };
        let id = fields.get("toolCallId")?.as_str()?.to_owned();
        Some(Self { id, fields })
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn merge(&mut self, next: Self) {
        debug_assert_eq!(self.id, next.id);
        // Null on an optional step leaves the previous field intact.
        self.fields.extend(
            next.fields
                .into_iter()
                .filter(|(_, value)| !value.is_null()),
        );
    }

    fn field(&self, name: &str) -> &Value {
        self.fields.get(name).unwrap_or(&Value::Null)
    }

    fn title(&self) -> &str {
        self.field("title")
            .as_str()
            .or_else(|| self.field("name").as_str())
            .unwrap_or("Tool")
    }

    fn command(&self) -> Option<&str> {
        field_text(
            self.field("rawInput"),
            &["CommandLine", "command_line", "commandLine", "command"],
        )
        .or_else(|| field_text(self.field("rawOutput"), &["commandLine", "command_line"]))
    }

    fn command_label(&self) -> Option<&str> {
        self.command()
            .or_else(|| (self.field("kind") == "execute").then(|| self.title()))
    }

    fn cwd(&self) -> Option<PathBuf> {
        field_text(
            self.field("rawInput"),
            &[
                "Cwd",
                "WorkingDirectory",
                "working_dir",
                "workingDir",
                "cwd",
            ],
        )
        .or_else(|| field_text(self.field("rawOutput"), &["workingDir", "working_dir"]))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    }

    pub(super) fn is_plan(&self) -> bool {
        // agy publishes its checklist through the standard tool_call update stream.
        matches!(self.field("kind").as_str(), None | Some("other"))
            && (self.field("name") == "update_steps" || self.title() == "Run update_steps")
    }

    pub(super) fn plan(&self) -> Option<String> {
        checklist(&self.field("rawInput")["steps"], "title")
    }

    pub(super) fn output(&self) -> Option<String> {
        if self.command_label().is_some() {
            return None;
        }
        self.text_output()
    }

    fn text_output(&self) -> Option<String> {
        let output = self.field("rawOutput");
        if let Some(text) = field_text(output, &["combinedOutput", "combined_output"]) {
            return Some(text.to_owned());
        }
        let text = self
            .field("content")
            .as_array()?
            .iter()
            .filter_map(|part| {
                (part["type"] == "content")
                    .then(|| content_text(&part["content"]))
                    .flatten()
            })
            .collect::<Vec<_>>();
        (!text.is_empty()).then(|| text.join("\n"))
    }

    pub(super) fn item(&self) -> Item {
        let status = match self.field("status").as_str() {
            Some("in_progress") => "inProgress",
            Some(status) => status,
            None => "pending",
        };
        if let Some(command) = self.command_label() {
            let raw = self.field("rawOutput");
            return Item::CommandExecution {
                cwd: self.cwd(),
                command: command.into(),
                status: status.into(),
                aggregated_output: self.text_output(),
                exit_code: raw["exitCode"]
                    .as_i64()
                    .or_else(|| raw["exit_code"].as_i64())
                    .and_then(|value| value.try_into().ok()),
            };
        }
        let changes: Vec<FileChange> = self
            .field("content")
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| file_change(part, self.field("locations")))
            .collect();
        if !changes.is_empty() {
            return Item::FileChange {
                status: status.into(),
                changes,
            };
        }
        Item::ToolCall {
            status: status.into(),
            tool: self.title().into(),
        }
    }
}

fn field_text<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|field| value[*field].as_str().filter(|text| !text.is_empty()))
}

fn file_change(part: &Value, locations: &Value) -> Option<FileChange> {
    if part["type"] != "diff" {
        return None;
    }
    let path = part["path"].as_str()?;
    // Keep provider-controlled filenames from impersonating patch headers.
    if path.is_empty() || path.contains(['\n', '\r', '\t', '\0']) {
        return None;
    }
    let old = part["oldText"].as_str();
    let new = part["newText"].as_str()?;
    if old == Some(new) {
        return None;
    }
    let old_lines: Vec<&str> = old.unwrap_or_default().split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();
    let shared = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(old, new)| old == new)
        .count();
    let tail = old_lines[shared..]
        .iter()
        .rev()
        .zip(new_lines[shared..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let before = shared.saturating_sub(3);
    let after = tail.saturating_sub(3);
    let old_end = old_lines.len() - after;
    let new_end = new_lines.len() - after;
    let old_count = old_end - before;
    let new_count = new_end - before;
    let line = locations
        .as_array()
        .into_iter()
        .flatten()
        .filter(|location| old.is_some() && location["path"] == path)
        .find_map(|location| location["line"].as_u64())
        .unwrap_or(1)
        .max(1);
    let hunk_start = |count| {
        line.saturating_add(before as u64)
            .saturating_sub(u64::from(count == 0))
    };
    let name = path.trim_start_matches('/');
    let source = old.map_or_else(|| "/dev/null".into(), |_| format!("a/{name}"));
    let old_start = hunk_start(old_count);
    let new_start = hunk_start(new_count);
    let mut patch = format!(
        "--- {source}\n+++ b/{name}\n@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
    );
    let mut push = |prefix, lines: &[&str]| {
        for line in lines {
            patch.push(prefix);
            patch.push_str(line);
            if !line.ends_with('\n') {
                patch.push_str("\n\\ No newline at end of file\n");
            }
        }
    };
    push(' ', &old_lines[before..shared]);
    push('-', &old_lines[shared..old_lines.len() - tail]);
    push('+', &new_lines[shared..new_lines.len() - tail]);
    push(' ', &old_lines[old_lines.len() - tail..old_end]);
    Some(FileChange {
        path: path.into(),
        diff: patch.into(),
    })
}

pub(super) struct Permission {
    pub session: String,
    pub tool_id: String,
    pub title: String,
    pub detail: String,
    pub command: Option<(String, Option<PathBuf>)>,
    pub choices: Vec<PermissionChoice>,
    pub is_question: bool,
}

pub(super) struct PermissionChoice {
    pub id: String,
    pub label: String,
    pub reject: bool,
    pub detail: Option<String>,
}

pub(super) fn permission(mut params: Value) -> Option<Permission> {
    let session = params["sessionId"].as_str()?.to_owned();
    let tool = ToolUpdate::new(params.get_mut("toolCall")?.take())?;
    // Antigravity's legacy tool interaction uses the same ACP request for multiple choice questions.
    let is_question = tool.id.starts_with("interaction_");
    let mut seen = HashSet::new();
    let choices = params["options"]
        .as_array()?
        .iter()
        .filter_map(|option| {
            let kind = option["kind"].as_str();
            let reject = matches!(kind, Some("reject_once" | "reject_always"));
            let id = option["optionId"].as_str()?.to_owned();
            if id.is_empty() || !seen.insert(id.clone()) {
                return None;
            }
            let label = option["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or(&id)
                .to_owned();
            let warning = &option["_meta"]["agy.security.warning"];
            let detail = field_text(warning, &["message", "title"]).map(str::to_owned);
            Some(PermissionChoice {
                id,
                label,
                reject,
                detail,
            })
        })
        .collect();
    let command = tool
        .command()
        .map(|command| (command.to_owned(), tool.cwd()));
    Some(Permission {
        session,
        title: tool.title().into(),
        detail: command
            .as_ref()
            .map(|(command, _)| command.clone())
            .or_else(|| tool.output())
            .unwrap_or_default(),
        tool_id: tool.id,
        command,
        choices,
        is_question,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_does_not_show_ox_context_as_user_input() {
        assert!(
            update(json!({"sessionId":"s", "update": {
                "sessionUpdate":"user_message_chunk",
                "content":{"type":"resource", "resource":{
                    "uri":super::super::TURN_CONTEXT_URI, "text":"Private pinned memory"
                }}
            }}))
            .is_none()
        );
    }

    #[test]
    fn config_options_drive_selection_and_replace_the_full_catalog() {
        let mut options = SessionOptions::default();
        let mode = json!({
            "type":"select", "id":"agent-mode", "category":"mode", "currentValue":"plan",
            "options":[{"value":"default", "name":"Default"}, {"value":"plan", "name":"Plan"}]
        });
        options.apply(&json!({"configOptions":[mode, {
            "id":"provider-model", "category":"model", "currentValue":"gemini-a",
            "options":[{"group":"google", "name":"Google", "options":[
                {"value":"gemini-a", "name":"Gemini A"}, {"value":"gemini-b", "name":"Gemini B"}
            ]}]
        }]}));
        assert_eq!(options.access(), Some(Access::Plan));
        assert_eq!(options.available_access(), [Access::Default, Access::Plan]);
        assert!(options.set_access("s", Access::AcceptEdits).is_none());
        assert_eq!(
            options.set_access("s", Access::Default),
            Some((
                "session/set_config_option",
                json!({"sessionId":"s", "configId":"agent-mode", "value":"default"}),
            ))
        );
        assert_eq!(
            options.set_model("s", "gemini-b"),
            Some((
                "session/set_config_option",
                json!({"sessionId":"s", "configId":"provider-model", "value":"gemini-b"}),
            ))
        );
        assert_eq!(options.capabilities().model, "gemini-a");
        assert_eq!(options.capabilities().model_name("gemini-b"), "Gemini B");
        assert!(options.set_model("s", "not-advertised").is_none());
        options.apply(&json!({"configOptions":[mode]}));
        assert!(options.capabilities().models.is_empty());
    }

    #[test]
    fn legacy_mode_and_model_use_their_native_ids() {
        let mut options = SessionOptions::default();
        options.apply(&json!({
            "modes":{"currentModeId":"default", "availableModes":[{"id":"default"},{"id":"accept-edits"}]},
            "models":{"currentModelId":"one", "availableModels":[{"modelId":"one"},{"modelId":"two"}]}
        }));
        assert_eq!(
            options.set_access("s", Access::AcceptEdits),
            Some((
                "session/set_mode",
                json!({"sessionId":"s", "modeId":"accept-edits"}),
            ))
        );
        assert_eq!(
            options.set_model("s", "two"),
            Some((
                "session/set_model",
                json!({"sessionId":"s", "modelId":"two"}),
            ))
        );
        options.confirm_legacy_access(Access::AcceptEdits);
        options.confirm_legacy_model("two");
        assert_eq!(options.access(), Some(Access::AcceptEdits));
        assert_eq!(options.capabilities().model, "two");
        options.apply(&json!({"currentModeId":"unclassified"}));
        assert_eq!(options.access(), None);
    }

    #[test]
    fn streams_and_plan_updates_are_bound_to_their_session() {
        let envelope = |update| json!({"sessionId":"s", "update":update});
        let (session, delta) = update(envelope(json!({
            "sessionUpdate":"agent_message_chunk", "messageId":"answer-2",
            "content":{"type":"text", "text":"Hello"}
        })))
        .unwrap();
        assert_eq!(session, "s");
        assert!(
            matches!(delta, Update::AgentText { message_id: Some(id), text } if id == "answer-2" && text == "Hello")
        );
        let (_, plan) = update(envelope(json!({
            "sessionUpdate":"plan",
            "entries":[{"content":"Build", "status":"pending", "priority":"high"}]
        })))
        .unwrap();
        assert!(matches!(plan, Update::Plan(text) if text == "- [ ] Build"));
        let (_, metadata) = update(envelope(
            json!({"sessionUpdate":"session_info_update", "title":null}),
        ))
        .unwrap();
        assert!(matches!(metadata, Update::Metadata { title: None }));
    }

    #[test]
    fn partial_calls_keep_their_native_command_and_display_new_results() {
        let mut command = ToolUpdate::new(json!({
            "toolCallId":"shell-1", "title":"Run ls", "kind":"execute",
            "status":"in_progress", "rawInput":{"CommandLine":"ls -a", "Cwd":"/work"}
        }))
        .unwrap();
        command.merge(
            ToolUpdate::new(json!({
                "toolCallId":"shell-1", "status":"completed", "title":null,
                "rawOutput":{"combinedOutput":".git\nREADME.md\n", "exitCode":0}
            }))
            .unwrap(),
        );
        assert_eq!(command.id(), "shell-1");
        assert!(matches!(command.item(), Item::CommandExecution {
            command, cwd: Some(cwd), status, aggregated_output: Some(output), exit_code: Some(0),
        } if command == "ls -a" && cwd == std::path::Path::new("/work") && status == "completed" && output.contains("README.md")));
        assert_eq!(command.output(), None);
        let mut plan =
            ToolUpdate::new(json!({"toolCallId":"step", "title":"Run update_steps"})).unwrap();
        plan.merge(ToolUpdate::new(json!({
            "toolCallId":"step", "rawInput":{"steps":[{"title":"Compile", "status":"done"}, {"title":"Review"}]}
        })).unwrap());
        assert!(plan.is_plan());
        assert_eq!(plan.plan().as_deref(), Some("- [x] Compile\n- [ ] Review"));
    }

    #[test]
    fn diff_content_preserves_context_location_and_final_newline() {
        let diff = file_change(
            &json!({"type":"diff", "path":"/work/foo.rs", "oldText":"first\nold\n", "newText":"first\nnew"}),
            &json!([{"path":"/work/foo.rs", "line":8}]),
        ).unwrap();
        assert_eq!(diff.path, "/work/foo.rs");
        assert!(
            diff.diff
                .ends_with("@@ -8,2 +8,2 @@\n first\n-old\n+new\n\\ No newline at end of file\n")
        );
    }

    #[test]
    fn permission_decoding_keeps_only_choices_supplied_by_the_agent() {
        let request = permission(json!({
            "sessionId":"s", "toolCall":{"toolCallId":"shell", "title":"List files", "rawInput":{"CommandLine":"ls"}},
            "options":[{"optionId":"once", "name":"Allow this time", "kind":"allow_once"},
                {"optionId":"stop", "name":"Reject", "kind":"reject_once"},
                {"optionId":"always", "name":"Allow for this thread", "kind":"allow_always", "_meta":{"agy.security.warning":{"message":"Apply to later commands too"}}}]
        })).unwrap();
        assert_eq!(request.session, "s");
        assert_eq!(request.tool_id, "shell");
        assert_eq!(request.title, "List files");
        assert_eq!(request.detail, "ls");
        assert_eq!(request.command, Some(("ls".into(), None)));
        assert!(!request.is_question);
        assert_eq!(request.choices[0].id, "once");
        assert_eq!(request.choices[0].label, "Allow this time");
        assert!(request.choices[1].reject);
        assert_eq!(
            request.choices[2].detail.as_deref(),
            Some("Apply to later commands too")
        );
    }
}
