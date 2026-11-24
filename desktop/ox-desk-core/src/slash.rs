//! Resolve slash input against the installed protocol, feature flags and active model.
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    pub methods: Arc<BTreeSet<String>>,
    pub goals: bool,
    pub session_commands: Option<Arc<std::collections::BTreeMap<String, String>>>,
    pub speed: bool,
    pub models: Arc<HashMap<String, crate::store::routes_thread::ModelInfo>>,
    pub catalog: Arc<HashMap<String, Vec<Tier>>>,
    pub model: String,
    pub tier: Option<String>,
    pub provider: String,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Tier {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

impl Capabilities {
    pub fn read_schema(&mut self, schema: &Value) {
        *Arc::make_mut(&mut self.methods) = schema["oneOf"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                v.pointer("/properties/method/enum/0")?
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        self.speed = self.methods.contains("thread/settings/update")
            && schema
                .pointer("/definitions/ThreadSettingsUpdateParams/properties/serviceTier")
                .is_some();
    }
    pub fn add_model(&mut self, model: crate::codex::commands::Model) {
        let id = model.model.unwrap_or(model.id);
        Arc::make_mut(&mut self.models).insert(
            id.clone(),
            crate::store::routes_thread::ModelInfo {
                label: model.display_name,
                efforts: model.supported_reasoning_efforts,
                default_effort: model.default_reasoning_effort,
                default_tier: model.default_service_tier.clone(),
                input_modalities: model.input_modalities,
            },
        );
        if model.is_default {
            self.model.clone_from(&id);
            self.tier = model.default_service_tier;
        }
        let tiers = if model.service_tiers.is_empty() {
            model
                .additional_speed_tiers
                .into_iter()
                .map(|name| Tier {
                    id: name.clone(),
                    name,
                    description: String::new(),
                })
                .collect()
        } else {
            model.service_tiers
        };
        Arc::make_mut(&mut self.catalog).insert(id, tiers);
    }
    pub fn tiers(&self) -> &[Tier] {
        self.catalog
            .get(&self.model)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
    pub fn supports_goal(&self) -> bool {
        self.goals
            && ["get", "set", "clear"]
                .iter()
                .all(|v| self.methods.contains(&format!("thread/goal/{v}")))
    }
    pub fn speed_choice(&self, name: &str) -> Result<Option<&str>, String> {
        if name.eq_ignore_ascii_case("default") {
            return Ok(None);
        }
        self.tiers()
            .iter()
            .find(|tier| tier.id.eq_ignore_ascii_case(name) || tier.name.eq_ignore_ascii_case(name))
            .map(|tier| Some(tier.id.as_str()))
            .ok_or_else(|| {
                format!(
                    "Codex does not offer “{name}” for {}. Type /speed to see available tiers.",
                    self.model
                )
            })
    }

    pub fn choices(&self) -> Vec<(String, String)> {
        let mut rows = vec![
            ("/new", "Start a new session"),
            ("/resume", "Choose a saved session"),
            ("/status", "Show this session’s billing route and model"),
        ];
        if self.supports_goal() {
            rows.push(("/goal", "View or change this session’s goal"));
        }
        if self.speed {
            rows.push(("/speed", "Choose the model’s service tier"));
        }
        if self.methods.contains("thread/compact/start") {
            rows.push(("/compact", "Compact this session’s context"));
        }
        if self.methods.contains("thread/name/set") {
            rows.push(("/rename", "Rename this session"));
        }
        rows.push(("/terminal", "Open this session in the stock terminal"));
        let mut result: Vec<_> = rows
            .into_iter()
            .map(|(cmd, hint)| (cmd.to_owned(), hint.to_owned()))
            .collect();
        if let Some(commands) = &self.session_commands {
            for (name, hint) in commands.iter() {
                if !result.iter().any(|(cmd, _)| cmd == name) {
                    result.push((name.clone(), hint.clone()));
                }
            }
        }
        result
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeCommand {
    Goal(Goal),
    Speed(Option<String>),
    Status,
    Compact,
    Rename(String),
}

impl NativeCommand {
    pub fn allowed_during_turn(&self) -> bool {
        matches!(self, Self::Status | Self::Speed(_) | Self::Goal(_))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Goal {
    Show,
    Set(String),
    Pause,
    Resume,
    Clear,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Native(NativeCommand),
    New,
    Resume,
    Terminal,
    SessionCommand,
    Hint(String),
}

impl Action {
    pub fn starts_thread(&self) -> bool {
        matches!(
            self,
            Self::SessionCommand
                | Self::Native(NativeCommand::Goal(Goal::Set(_)) | NativeCommand::Speed(Some(_)))
        )
    }
}
pub fn parse(text: &str, caps: &Capabilities) -> Option<Action> {
    let text = text.trim();
    text.strip_prefix('/')?;
    let (command, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let args = args.trim();
    Some(match (command, args) {
        ("/terminal", "") => Action::Terminal,
        (command, _)
            if caps
                .session_commands
                .as_ref()
                .is_some_and(|commands| commands.contains_key(command)) =>
        {
            Action::SessionCommand
        }
        ("/", "") => Action::Hint("Choose a command below.".into()),
        ("/new", "") => Action::New,
        ("/resume", "") => Action::Resume,
        ("/status", "") => Action::Native(NativeCommand::Status),
        ("/compact", "") if caps.methods.contains("thread/compact/start") => {
            Action::Native(NativeCommand::Compact)
        }
        ("/rename", name) if caps.methods.contains("thread/name/set") => {
            if name.is_empty() {
                Action::Hint("Enter /rename followed by the new session name.".into())
            } else {
                Action::Native(NativeCommand::Rename(name.into()))
            }
        }
        ("/speed", s) if caps.speed => {
            if s.is_empty() {
                Action::Native(NativeCommand::Speed(None))
            } else if let Err(err) = caps.speed_choice(s) {
                Action::Hint(err)
            } else {
                Action::Native(NativeCommand::Speed(Some(s.into())))
            }
        }
        ("/goal", arg) if caps.supports_goal() => {
            let goal = match arg {
                "" => Goal::Show,
                "pause" => Goal::Pause,
                "resume" => Goal::Resume,
                "clear" => Goal::Clear,
                "edit" => {
                    return Some(Action::Hint(
                        "Use /goal edit followed by the revised objective.".into(),
                    ));
                }
                text => {
                    let objective = text.strip_prefix("edit ").unwrap_or(text).trim();
                    if objective.chars().count() > 4000 {
                        return Some(Action::Hint(
                            "Codex goals can contain up to 4,000 characters.".into(),
                        ));
                    }
                    Goal::Set(objective.into())
                }
            };
            Action::Native(NativeCommand::Goal(goal))
        }
        _ => Action::Terminal,
    })
}

/// Items in the composer's command menu are derived from the discovered methods,
/// enabled features and the model applied to this particular thread.
pub fn completions(text: &str, caps: &Capabilities) -> Vec<(String, String)> {
    let word = text.trim_start();
    if word.trim_end() == "/speed" && caps.speed {
        let mut rows = vec![("/speed default".into(), "thread/settings/update".into())];
        rows.extend(caps.tiers().iter().map(|t| {
            (
                format!("/speed {}", t.id),
                format!("{} · thread/settings/update", t.name),
            )
        }));
        return rows;
    }
    if word.trim_end() == "/goal" && caps.supports_goal() {
        return [
            ("/goal", "thread/goal/get"),
            ("/goal pause", "thread/goal/set"),
            ("/goal resume", "thread/goal/set"),
            ("/goal clear", "thread/goal/clear"),
        ]
        .map(|(text, method)| (text.into(), method.into()))
        .to_vec();
    }
    caps.choices()
        .into_iter()
        .filter(|(name, _)| name.starts_with(word))
        .map(|(name, hint)| {
            let method = match name.as_str() {
                "/goal" => "thread/goal/get",
                "/rename" => "thread/name/set",
                "/speed" => "thread/settings/update",
                "/compact" => "thread/compact/start",
                "/resume" => "thread/list",
                _ => &hint,
            };
            (name, method.into())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn additional_commands_always_stay_out_of_inference() {
        let caps = Capabilities::default();
        for value in [
            "/a-future-command",
            "/goal build",
            "/compact",
            "/speed fast",
            "/new named",
        ] {
            assert_eq!(parse(value, &caps), Some(Action::Terminal), "{value}");
        }
        assert_eq!(parse("review /tmp", &caps), None);
        assert_eq!(parse("/new", &caps), Some(Action::New));
    }

    #[test]
    fn native_actions_follow_methods_feature_state_and_model_tiers() {
        let mut caps = Capabilities::default();
        let methods = [
            "thread/goal/set",
            "thread/goal/get",
            "thread/goal/clear",
            "thread/settings/update",
        ];
        let schema = json!({"oneOf": methods.map(|m|json!({"properties":{"method":{"enum":[m]}}})),
            "definitions":{"ThreadSettingsUpdateParams":{"properties":{"serviceTier":{}}}}});
        caps.read_schema(&schema);
        assert_eq!(parse("/goal", &caps), Some(Action::Terminal));
        caps.goals = true;
        assert_eq!(
            parse("/goal edit Build v2", &caps),
            Some(Action::Native(NativeCommand::Goal(Goal::Set(
                "Build v2".into()
            ))))
        );
        assert_eq!(
            parse("/goal clear", &caps),
            Some(Action::Native(NativeCommand::Goal(Goal::Clear)))
        );
        Arc::make_mut(&mut caps.catalog).insert(
            "model-a".into(),
            vec![Tier {
                id: "priority".into(),
                name: "Fast".into(),
                description: "2x speed".into(),
            }],
        );
        caps.model = "model-a".into();
        assert_eq!(caps.speed_choice("fast"), Ok(Some("priority")));
        assert_eq!(caps.speed_choice("default"), Ok(None));
        caps.model = "model-b".into();
        assert!(matches!(parse("/speed fast", &caps), Some(Action::Hint(_))));
    }
}
