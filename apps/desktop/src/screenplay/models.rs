use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct SessionInit {
    pub working_title: Option<String>,
    pub genre: Option<String>,
    pub synopsis: Option<String>,
    pub tone: Option<String>,
    pub additional_context: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct TurnInput {
    pub text: String,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct DialogTurn {
    pub assistant_text: String,
    pub follow_up_questions: Vec<String>,
    pub telemetry: Option<LlmResponseTelemetry>,
    pub metadata: Value,
}

#[derive(Clone, Debug, Default)]
pub struct LlmResponseTelemetry {
    pub provider: String,
    pub model: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub latency: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
pub struct DraftOptions {
    pub outline: Option<Vec<String>>,
    pub target_runtime_minutes: Option<u32>,
    pub include_style_notes: bool,
}

#[derive(Clone, Debug)]
pub enum RevisionScope {
    EntireDraft,
    Scenes(Vec<String>),
    Characters(Vec<String>),
    Beats(Vec<String>),
}

impl Default for RevisionScope {
    fn default() -> Self {
        RevisionScope::EntireDraft
    }
}

#[derive(Clone, Debug, Default)]
pub struct RevisionRequest {
    pub instructions: String,
    pub scope: RevisionScope,
}

#[derive(Clone, Debug, Default)]
pub struct LlmScreenplayDraft {
    pub script: String,
    pub synopsis: Option<String>,
    pub metadata: Value,
}

#[derive(Clone, Debug, Default)]
pub struct PromptBundle {
    pub positive: Vec<String>,
    pub negative: Vec<String>,
    pub metadata: Value,
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    TokenChunk(String),
    AssistantTurn(DialogTurn),
    Telemetry(LlmResponseTelemetry),
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct ProviderCapabilities {
    pub supports_revision: bool,
    pub supports_streaming: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Gemini,
    Mock,
    Custom(String),
}

impl ProviderKind {
    pub fn as_str(&self) -> &str {
        match self {
            ProviderKind::OpenAi => "openai",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Mock => "mock",
            ProviderKind::Custom(name) => name.as_str(),
        }
    }
}
