use super::Palette;
use crate::appearance::Step;
use gpui::{prelude::*, *};
use gpui_component::input::InputState;
use ox_desk_core::settings_protocol::model::{
    self, Cell, Changes, Key, ProviderEdit, ProviderRow, RowEdit, Values,
};

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Positive,
    List,
    Choices(&'static [&'static str]),
}
use Kind::*;
#[derive(Clone, Copy)]
pub struct Spec {
    key: Key,
    label: &'static str,
    kind: Kind,
    hint: &'static str,
}
const fn field(key: Key, label: &'static str, kind: Kind, hint: &'static str) -> Spec {
    Spec {
        key,
        label,
        kind,
        hint,
    }
}
/// One plain-language line for the choice fields whose values are raw config words.
fn help(key: Key) -> Option<&'static str> {
    Some(match key {
        Key::ChatReasoningEffort => {
            "Whether the reasoning-effort setting is passed on to this provider."
        }
        Key::MaxTokensField => {
            "Which request field carries the output limit; newer models want the second."
        }
        Key::DeveloperRole => {
            "Role used for instructions; pick Developer only if the provider supports it."
        }
        Key::AnthropicThinking => {
            "Extended thinking: off, chosen by the model, or a fixed token budget."
        }
        Key::OutputConfigEffort => "Send the reasoning effort as an output option.",
        _ => return None,
    })
}
pub const CONNECTION: &[Spec] = &[
    field(Key::Name, "Display name", Text, "Provider name"),
    field(
        Key::BaseUrl,
        "API endpoint",
        Text,
        "https://api.example.com/v1",
    ),
    field(
        Key::WireApi,
        "Protocol",
        Choices(&["responses", "chat_completions", "anthropic_messages"]),
        "responses",
    ),
    field(
        Key::CredentialEnv,
        "Key environment variable",
        Text,
        "EXAMPLE_API_KEY",
    ),
    field(
        Key::Discovery,
        "Discover models",
        Choices(&["true", "false"]),
        "true",
    ),
    field(
        Key::ModelsUrl,
        "Model catalog URL",
        Text,
        "Defaults to the provider’s models endpoint",
    ),
    field(
        Key::MaxOutputTokens,
        "Maximum output tokens",
        Positive,
        "Provider default",
    ),
];
pub const CHAT: &[Spec] = &[
    field(
        Key::MaxTokensField,
        "Output token field",
        Choices(&["max_tokens", "max_completion_tokens"]),
        "max_tokens",
    ),
    field(
        Key::ChatReasoningEffort,
        "Reasoning effort",
        Choices(&["drop", "forward"]),
        "drop",
    ),
    field(
        Key::DeveloperRole,
        "Developer role",
        Choices(&["system", "developer"]),
        "system",
    ),
    field(
        Key::IncludeStreamUsage,
        "Stream usage",
        Choices(&["true", "false"]),
        "true",
    ),
];
pub const ANTHROPIC: &[Spec] = &[
    field(Key::AnthropicVersion, "API version", Text, "YYYY-MM-DD"),
    field(
        Key::AnthropicThinking,
        "Thinking mode",
        Choices(&["off", "adaptive", "manual"]),
        "off",
    ),
    field(
        Key::AnthropicThinkingBudgetTokens,
        "Thinking token budget",
        Positive,
        "4096",
    ),
    field(
        Key::OutputConfigEffort,
        "Send output effort",
        Choices(&["false", "true"]),
        "false",
    ),
];
pub const MODEL: &[Spec] = &[
    field(
        Key::Id,
        "Upstream model ID",
        Text,
        "Model ID from your provider",
    ),
    field(Key::Name, "Display name", Text, "Defaults to model ID"),
    field(
        Key::Alias,
        "Model alias",
        Text,
        "Defaults to provider/model",
    ),
    field(
        Key::ContextWindow,
        "Context window",
        Positive,
        "Provider default",
    ),
    field(Key::InputModalities, "Input types", List, "text, image"),
    field(
        Key::ReasoningEfforts,
        "Reasoning levels",
        List,
        "low, medium, high",
    ),
];
pub const SEARCH: &[Spec] = &[
    field(
        Key::Enabled,
        "Brave Search",
        Choices(&["false", "true"]),
        "false",
    ),
    field(
        Key::CredentialEnv,
        "Key environment variable",
        Text,
        "BRAVE_SEARCH_API_KEY",
    ),
    field(Key::MaxResults, "Maximum results", Positive, "8"),
    field(Key::TimeoutSeconds, "Timeout in seconds", Positive, "30"),
];
pub const LEGACY: &[Spec] = &[
    field(Key::Name, "Display name", Text, "Provider name"),
    field(
        Key::BaseUrl,
        "API endpoint",
        Text,
        "https://api.example.com/v1",
    ),
    field(
        Key::ModelCatalogUrl,
        "Model catalog URL",
        Text,
        "Default catalog",
    ),
    field(
        Key::CredentialEnv,
        "Key environment variable",
        Text,
        "EXAMPLE_API_KEY",
    ),
    field(
        Key::WireApi,
        "Protocol",
        Choices(&["responses"]),
        "responses",
    ),
];

pub fn input(initial: &str, hint: &str, window: &mut Window, cx: &mut App) -> Entity<InputState> {
    crate::kit::field(initial, hint, false, window, cx)
}

pub struct Fields {
    base: Values,
    values: Vec<(Spec, String, Entity<InputState>)>,
}

pub fn display(cell: &Cell) -> String {
    match cell {
        Cell::Text(s) => s.clone(),
        Cell::Bool(b) => b.to_string(),
        Cell::Integer(n) => n.to_string(),
        Cell::List(list) => list.join(", "),
        Cell::Private => "Private value (unchanged)".into(),
    }
}
impl Fields {
    pub fn new(base: Values, groups: &[&[Spec]], window: &mut Window, cx: &mut App) -> Self {
        let values = groups
            .iter()
            .flat_map(|g| g.iter())
            .map(|spec| {
                let initial = base.get(&spec.key).map(display).unwrap_or_else(|| {
                    if matches!(spec.kind, Choices(_)) {
                        spec.hint.to_owned()
                    } else {
                        String::new()
                    }
                });
                let control = input(&initial, spec.hint, window, cx);
                (*spec, initial, control)
            })
            .collect();
        Self { base, values }
    }
    pub fn value(&self, key: Key, cx: &App) -> String {
        self.values
            .iter()
            .find(|(f, _, _)| f.key == key)
            .map(|(_, _, i)| i.read(cx).value().trim().to_owned())
            .unwrap_or_default()
    }
    pub fn patch(&self, groups: &[&[Spec]], new: bool, cx: &App) -> Result<Changes, String> {
        let mut edits = Changes::new();
        for (spec, initial, control) in &self.values {
            let base = self.base.get(&spec.key);
            if !groups.iter().any(|g| g.iter().any(|s| s.key == spec.key)) {
                if base.is_some() && !new {
                    edits.insert(spec.key, None);
                }
                continue;
            }
            let raw = control.read(cx).value();
            if raw.as_ref() == initial.as_str() {
                if new && let Some(cell) = base {
                    edits.insert(spec.key, Some(cell.clone()));
                }
                continue;
            }
            let text = raw.trim();
            let value = if text.is_empty() {
                None
            } else {
                Some(match spec.kind {
                    Positive => {
                        Cell::Integer(text.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(
                            || format!("{} must be a positive whole number.", spec.label),
                        )?)
                    }
                    List => Cell::List(
                        text.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    ),
                    Choices(_) if text == "true" || text == "false" => Cell::Bool(text == "true"),
                    _ => Cell::Text(text.into()),
                })
            };
            edits.insert(spec.key, value);
        }
        Ok(edits)
    }

    pub fn render(
        &self,
        specs: &[Spec],
        prefix: &str,
        p: Palette,
        cx: &mut Context<super::Preferences>,
    ) -> Div {
        let mut rows = div().flex().flex_col().gap_3();
        for (spec, _, editor) in &self.values {
            if !specs.iter().any(|s| s.key == spec.key) {
                continue;
            }
            let mut row = div().flex().items_center().gap_3().child(
                div()
                    .w(px(162.))
                    .flex_shrink_0()
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(p.muted))
                    .child(spec.label),
            );
            if let Choices(options) = spec.kind {
                let mut choices = div().flex().flex_wrap().gap_1();
                for option in options.iter().copied() {
                    let control = editor.clone();
                    let selected = editor.read(cx).value().as_ref() == option;
                    let caption = match option {
                        "responses" => "Responses",
                        "chat_completions" => "Chat Completions",
                        "anthropic_messages" => "Messages",
                        "true" => "On",
                        "false" => "Off",
                        "drop" => "Do not send",
                        "forward" => "Send to provider",
                        "max_tokens" => "max_tokens (classic)",
                        "max_completion_tokens" => "max_completion_tokens (newer)",
                        "system" => "System message",
                        "developer" => "Developer message",
                        "off" => "Off",
                        "adaptive" => "Adaptive",
                        "manual" => "Fixed budget",
                        other => other,
                    };
                    choices = choices.child(crate::kit::button(
                        p,
                        SharedString::from(format!("{prefix}-{}-{option}", spec.key.name())),
                        caption,
                        selected,
                        true,
                        cx.listener(move |_, _, window, cx| {
                            control.update(cx, |input, cx| input.set_value(option, window, cx));
                            cx.notify();
                        }),
                    ));
                }
                row = row.child(choices);
                if let Some(help) = help(spec.key) {
                    row = row.child(
                        div()
                            .min_w_0()
                            .text_size(p.text(Step::Label))
                            .text_color(rgb(p.muted))
                            .child(help),
                    );
                }
            } else {
                row = row.child(div().min_w_0().flex_1().child(crate::kit::input(editor)));
            }
            rows = rows.child(row);
        }
        rows
    }
}

pub struct Provider {
    pub original: Option<String>,
    pub id: Entity<InputState>,
    pub fields: Fields,
    pub models: Vec<Fields>,
    original_models: Vec<String>,
}
fn model_id(fields: &Values) -> Option<String> {
    match fields.get(&Key::Id) {
        Some(Cell::Text(s)) => Some(s.clone()),
        _ => None,
    }
}
impl Provider {
    pub fn new(row: ProviderRow, new: bool, w: &mut Window, cx: &mut App) -> Self {
        let original = (!new).then(|| row.id.clone());
        let original_models = row.models.iter().filter_map(model_id).collect();
        let models = row
            .models
            .into_iter()
            .map(|m| Fields::new(m, &[MODEL], w, cx))
            .collect();
        Self {
            original,
            id: input(&row.id, "Provider ID", w, cx),
            fields: Fields::new(row.fields, &[CONNECTION, CHAT, ANTHROPIC], w, cx),
            models,
            original_models,
        }
    }
    pub fn wire_fields(&self, cx: &App) -> &'static [Spec] {
        match self.fields.value(Key::WireApi, cx).as_str() {
            "chat_completions" => CHAT,
            "anthropic_messages" => ANTHROPIC,
            _ => &[],
        }
    }
    pub fn edit(&self, cx: &App) -> Result<Option<ProviderEdit>, String> {
        let id = self.id.read(cx).value().trim().to_owned();
        if id.is_empty() {
            return Err("Enter a provider ID.".into());
        }
        let new = self.original.is_none();
        let fields = self
            .fields
            .patch(&[CONNECTION, self.wire_fields(cx)], new, cx)?;
        let mut models = vec![];
        if !new {
            for old in &self.original_models {
                if !self
                    .models
                    .iter()
                    .any(|f| model_id(&f.base).as_ref() == Some(old))
                {
                    models.push(RowEdit {
                        original: Some(old.clone()),
                        id: None,
                        fields: Changes::new(),
                    })
                }
            }
        }
        for model in &self.models {
            let original = if new { None } else { model_id(&model.base) };
            let id = model.value(Key::Id, cx);
            if id.is_empty() {
                return Err("Enter a model ID.".into());
            }
            let fields = model.patch(&[MODEL], original.is_none(), cx)?;
            if original.as_ref() != Some(&id) || !fields.is_empty() {
                models.push(RowEdit {
                    original,
                    id: Some(id),
                    fields,
                })
            }
        }
        if self.original.as_ref() == Some(&id) && fields.is_empty() && models.is_empty() {
            return Ok(None);
        }
        Ok(Some(ProviderEdit {
            row: RowEdit {
                original: self.original.clone(),
                id: Some(id),
                fields,
            },
            models,
        }))
    }
}
pub struct Registry {
    pub providers: Vec<Provider>,
    pub search: Fields,
    original: Vec<String>,
}
impl Registry {
    pub fn new(rows: model::Registry, w: &mut Window, cx: &mut App) -> Self {
        let original = rows.providers.iter().map(|p| p.id.clone()).collect();
        Self {
            providers: rows
                .providers
                .into_iter()
                .map(|p| Provider::new(p, false, w, cx))
                .collect(),
            search: Fields::new(rows.search, &[SEARCH], w, cx),
            original,
        }
    }
    pub fn edit(&self, cx: &App) -> Result<model::Edit, String> {
        let mut changes = vec![];
        for old in &self.original {
            if !self
                .providers
                .iter()
                .any(|p| p.original.as_ref() == Some(old))
            {
                changes.push(ProviderEdit {
                    row: RowEdit {
                        original: Some(old.clone()),
                        id: None,
                        fields: Changes::new(),
                    },
                    models: vec![],
                })
            }
        }
        for p in &self.providers {
            if let Some(edit) = p.edit(cx)? {
                changes.push(edit)
            }
        }
        Ok(model::Edit::Providers { changes })
    }
}
