//! Allowlisted settings operations for native clients. The helper alone handles file syntax.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

macro_rules! keys {
    ($($key:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone,Copy,Debug,PartialEq,Eq,Ord,PartialOrd,Serialize,Deserialize)]
        pub enum Key { $(#[serde(rename=$name)] $key),+ }
        impl Key { pub fn name(self) -> &'static str { match self { $(Self::$key => $name),+ } } }
    }
}
keys! {
    Name => "name",
    BaseUrl => "base_url",
    WireApi => "wire_api",
    CredentialEnv => "env_key",
    Discovery => "discovery",
    ModelsUrl => "models_url",
    MaxOutputTokens => "max_output_tokens",
    MaxTokensField => "max_tokens_field",
    ChatReasoningEffort => "chat_reasoning_effort",
    DeveloperRole => "developer_role",
    IncludeStreamUsage => "include_stream_usage",
    AnthropicVersion => "anthropic_version",
    AnthropicThinking => "anthropic_thinking",
    AnthropicThinkingBudgetTokens => "anthropic_thinking_budget_tokens",
    OutputConfigEffort => "output_config_effort",
    Id => "id",
    Alias => "alias",
    ContextWindow => "context_window",
    InputModalities => "input_modalities",
    ReasoningEfforts => "reasoning_efforts",
    DefaultReasoningEffort => "default_reasoning_effort",
    ReasoningMandatory => "reasoning_mandatory",
    Enabled => "enabled",
    MaxResults => "max_results",
    TimeoutSeconds => "timeout_seconds",
    ModelCatalogUrl => "model_catalog_url",
    Model => "model",
}
pub const PROVIDER_KEYS: &[Key] = &[
    Key::Name,
    Key::BaseUrl,
    Key::WireApi,
    Key::CredentialEnv,
    Key::Discovery,
    Key::ModelsUrl,
    Key::MaxOutputTokens,
    Key::MaxTokensField,
    Key::ChatReasoningEffort,
    Key::DeveloperRole,
    Key::IncludeStreamUsage,
    Key::AnthropicVersion,
    Key::AnthropicThinking,
    Key::AnthropicThinkingBudgetTokens,
    Key::OutputConfigEffort,
];
pub const MODEL_KEYS: &[Key] = &[
    Key::Id,
    Key::Name,
    Key::Alias,
    Key::ContextWindow,
    Key::InputModalities,
    Key::ReasoningEfforts,
    Key::DefaultReasoningEffort,
    Key::ReasoningMandatory,
];
pub const SEARCH_KEYS: &[Key] = &[
    Key::Enabled,
    Key::CredentialEnv,
    Key::MaxResults,
    Key::TimeoutSeconds,
];
pub const LEGACY_KEYS: &[Key] = &[
    Key::Name,
    Key::BaseUrl,
    Key::ModelCatalogUrl,
    Key::CredentialEnv,
    Key::WireApi,
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum Cell {
    Text(String),
    Integer(i64),
    Bool(bool),
    List(Vec<String>),
    Private,
}
pub type Values = BTreeMap<Key, Cell>;
pub type Changes = BTreeMap<Key, Option<Cell>>;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProviderRow {
    pub id: String,
    pub fields: Values,
    pub models: Vec<Values>,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Registry {
    pub providers: Vec<ProviderRow>,
    pub search: Values,
}
#[derive(Serialize, Deserialize)]
pub struct Legacy {
    pub id: String,
    pub model: Option<Cell>,
    pub fields: Values,
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Revisions {
    pub registry: String,
    pub favorites: String,
    pub legacy: String,
    pub keys: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub revisions: Revisions,
    pub registry: Option<Registry>,
    pub favorites: Vec<String>,
    pub legacy: Option<Legacy>,
    pub credentials: Vec<super::Credential>,
    pub models: Vec<String>,
    pub api_provider: Option<String>,
    pub chatgpt_account: Option<String>,
    pub claude_account: bool,
    pub presets: Vec<(String, ProviderRow)>,
    pub issues: Vec<String>,
    /// Projects whose sessions grant YOLO to their workers and handoffs by default.
    #[serde(default)]
    pub yolo_grant_projects: Vec<std::path::PathBuf>,
}

// A missing original creates a row. A missing destination removes the named original.
#[derive(Clone, Serialize, Deserialize)]
pub struct RowEdit {
    pub original: Option<String>,
    pub id: Option<String>,
    pub fields: Changes,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderEdit {
    pub row: RowEdit,
    pub models: Vec<RowEdit>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "section", rename_all = "snake_case")]
pub enum Edit {
    Providers { changes: Vec<ProviderEdit> },
    Search { fields: Changes },
    Favorites { models: Vec<String> },
    Legacy { id: String, fields: Changes },
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    ChangedOnDisk { revision: String },
}

/// Private ASCII credential buffer. It never implements Debug, Clone or Display.
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);
impl From<String> for Secret {
    fn from(text: String) -> Self {
        Self(text)
    }
}
impl Default for Secret {
    fn default() -> Self {
        Self(String::with_capacity(8192))
    }
}
impl Secret {
    pub fn into_string(mut self) -> String {
        std::mem::take(&mut self.0)
    }
    #[allow(dead_code)] // Desktop field displays the character count.
    pub fn expose(&self) -> &str {
        &self.0
    }
    #[allow(dead_code)] // Input editing is used by the desktop consumer of this shared type.
    pub fn append(&mut self, text: &str) {
        if self.0.len() + text.len() <= 8192 && text.bytes().all(|b| b.is_ascii_graphic()) {
            self.0.push_str(text)
        }
    }
    pub fn clear(&mut self) {
        // SAFETY: zero bytes are valid UTF-8. The immediately following clear truncates
        // the buffer. Volatile stores ensure clearing survives release optimization.
        unsafe {
            for b in self.0.as_mut_vec() {
                std::ptr::write_volatile(b, 0)
            }
        }
        self.0.clear()
    }
    #[allow(dead_code)] // Input editing is used by the desktop consumer of this shared type.
    pub fn backspace(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let last = self.0.char_indices().next_back().unwrap().0;
        // SAFETY: overwrite the final whole UTF-8 character then truncate on its boundary.
        unsafe {
            for b in &mut self.0.as_mut_vec()[last..] {
                std::ptr::write_volatile(b, 0)
            }
        }
        self.0.truncate(last)
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.clear()
    }
}
