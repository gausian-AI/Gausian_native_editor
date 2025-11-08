use super::super::models::{
    DialogTurn, DraftOptions, LlmResponseTelemetry, LlmScreenplayDraft, ProviderCapabilities,
    ProviderKind, RevisionRequest, SessionInit, TurnInput,
};
use super::super::service::{ScreenplayLlmProvider, ScreenplayLlmSession};
use super::ProviderError;
use crate::screenplay;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_SYSTEM_PROMPT: &str = "You are a collaborative screenplay assistant helping filmmakers explore ideas, collect story details, and produce polished drafts and storyboards.";

#[derive(Clone, Debug)]
pub struct GeminiConfig {
    pub api_key: String,
    pub model: String,
    pub system_prompt: Option<String>,
    pub temperature: f32,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: "gemini-2.5-flash".to_string(),
            system_prompt: None,
            temperature: 0.7,
        }
    }
}

struct GeminiShared {
    config: GeminiConfig,
    agent: ureq::Agent,
}

impl GeminiShared {
    fn new(config: GeminiConfig) -> Result<Self, ProviderError> {
        if config.api_key.trim().is_empty() {
            return Err(ProviderError::Configuration(
                "Gemini API key is required.".to_string(),
            ));
        }
        if config.model.trim().is_empty() {
            return Err(ProviderError::Configuration(
                "Gemini model name is required.".to_string(),
            ));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(60))
            .timeout_write(Duration::from_secs(20))
            .build();
        Ok(Self { config, agent })
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/v1beta/models/{}:generateContent?key={}",
            GEMINI_API_BASE.trim_end_matches('/'),
            self.config.model.trim(),
            urlencoding::encode(self.config.api_key.trim())
        )
    }

    fn execute_chat(
        &self,
        system_prompt: Option<&str>,
        messages: &[ChatMessage],
    ) -> Result<(GeminiCandidate, LlmResponseTelemetry), ProviderError> {
        let mut content_messages = Vec::new();
        for msg in messages {
            match msg.role.as_str() {
                "user" => content_messages.push(json!({
                    "role": "user",
                    "parts": [{"text": msg.content }]
                })),
                "assistant" | "model" => content_messages.push(json!({
                    "role": "model",
                    "parts": [{"text": msg.content }]
                })),
                // system prompts handled separately
                _ => {}
            }
        }
        let mut payload = json!({
            "contents": content_messages,
            "generationConfig": {
                "temperature": self.config.temperature,
            }
        });
        if let Some(prompt) = system_prompt {
            payload["systemInstruction"] = json!({
                "role": "system",
                "parts": [{"text": prompt}]
            });
        }
        let start = Instant::now();
        let response = self
            .agent
            .post(&self.endpoint())
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .set("X-Goog-Api-Key", self.config.api_key.trim())
            .send_string(&payload.to_string())
            .map_err(|err| ProviderError::transport(format!("Gemini request failed: {err}")))?;
        let elapsed = start.elapsed();
        let body = response.into_string().map_err(|err| {
            ProviderError::transport(format!("Read Gemini response failed: {err}"))
        })?;
        let parsed: GeminiResponse = serde_json::from_str(&body).map_err(|err| {
            ProviderError::invalid_response(format!(
                "Invalid Gemini response JSON: {err}; raw: {body}"
            ))
        })?;
        let candidate =
            parsed.candidates.into_iter().next().ok_or_else(|| {
                ProviderError::invalid_response("Gemini response had no candidates.")
            })?;
        let telemetry = LlmResponseTelemetry {
            provider: ProviderKind::Gemini.as_str().to_string(),
            model: self.config.model.clone(),
            input_tokens: parsed
                .usage_metadata
                .as_ref()
                .and_then(|m| m.prompt_token_count),
            output_tokens: parsed
                .usage_metadata
                .as_ref()
                .and_then(|m| m.candidates_token_count),
            total_tokens: parsed
                .usage_metadata
                .as_ref()
                .and_then(|m| m.total_token_count),
            latency: Some(elapsed),
        };
        Ok((candidate, telemetry))
    }
}

#[derive(Clone, Debug)]
struct ChatMessage {
    role: String,
    content: String,
}

pub struct GeminiProvider {
    shared: Arc<GeminiShared>,
}

impl GeminiProvider {
    pub fn new(config: GeminiConfig) -> Result<Self, ProviderError> {
        let shared = GeminiShared::new(config)?;
        Ok(Self {
            shared: Arc::new(shared),
        })
    }

    fn compose_system_prompt(config: &GeminiConfig, init: &SessionInit) -> String {
        let mut prompt = config
            .system_prompt
            .clone()
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
        if let Some(title) = init.working_title.as_deref() {
            if !title.trim().is_empty() {
                prompt.push_str(&format!("\nWorking title: \"{}\".", title.trim()));
            }
        }
        if let Some(genre) = init.genre.as_deref() {
            if !genre.trim().is_empty() {
                prompt.push_str(&format!("\nGenre focus: {}.", genre.trim()));
            }
        }
        if let Some(tone) = init.tone.as_deref() {
            if !tone.trim().is_empty() {
                prompt.push_str(&format!("\nDesired tone: {}.", tone.trim()));
            }
        }
        if let Some(synopsis) = init.synopsis.as_deref() {
            if !synopsis.trim().is_empty() {
                prompt.push_str(&format!("\nSynopsis:\n{}", synopsis.trim()));
            }
        }
        if let Some(context) = init.additional_context.as_deref() {
            if !context.trim().is_empty() {
                prompt.push_str(&format!("\nContext:\n{}", context.trim()));
            }
        }
        prompt.push_str("\n\nFocus on gathering only these essentials before drafting (skip everything else unless the user volunteers it):\n");
        prompt.push_str(screenplay::screenplay_requirements_prompt());
        prompt.push_str("\nAssume a short-form (1–5 minute) film. Do not ask about output format, draft scope, or continuation policy unless the user brings it up. If the user does not specify a title or synopsis, invent appropriate ones.");
        prompt.push_str("\n\nYou can recommend or adapt these plot structures:\n");
        prompt.push_str(screenplay::plot_types_prompt());
        prompt.push_str("\n\nWhen drafting, return the screenplay as structured JSON only (see the format below). Do not wrap the output in prose or Markdown.\n");
        prompt.push_str(screenplay::screenplay_format_prompt());
        prompt
    }
}

impl ScreenplayLlmProvider for GeminiProvider {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::Gemini
    }

    fn model_name(&self) -> &str {
        &self.shared.config.model
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_revision: true,
            supports_streaming: false,
        }
    }

    fn create_session(
        &self,
        init: SessionInit,
    ) -> Result<Box<dyn ScreenplayLlmSession>, ProviderError> {
        let system_prompt = Self::compose_system_prompt(&self.shared.config, &init);
        Ok(Box::new(GeminiSession {
            shared: Arc::clone(&self.shared),
            session_id: Uuid::new_v4().to_string(),
            system_prompt,
            messages: Vec::new(),
            greeted: false,
            init,
        }))
    }
}

struct GeminiSession {
    shared: Arc<GeminiShared>,
    session_id: String,
    system_prompt: String,
    messages: Vec<ChatMessage>,
    greeted: bool,
    init: SessionInit,
}

impl GeminiSession {
    fn request_completion(
        &mut self,
        transient_user: Option<&str>,
        persist_transient: bool,
    ) -> Result<(String, LlmResponseTelemetry), ProviderError> {
        let mut payload_messages = self.messages.clone();
        if let Some(content) = transient_user {
            let msg = ChatMessage {
                role: "user".to_string(),
                content: content.to_string(),
            };
            payload_messages.push(msg.clone());
            if persist_transient {
                self.messages.push(msg);
            }
        }
        let (candidate, telemetry) = self
            .shared
            .execute_chat(Some(&self.system_prompt), &payload_messages)?;
        let text = candidate
            .content
            .and_then(|c| {
                c.parts.map(|parts| {
                    parts
                        .into_iter()
                        .filter_map(|p| p.text)
                        .collect::<Vec<_>>()
                        .join("")
                })
            })
            .unwrap_or_default();
        self.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: text.clone(),
        });
        Ok((text, telemetry))
    }

    fn send_user_and_complete(
        &mut self,
        text: String,
    ) -> Result<(String, LlmResponseTelemetry), ProviderError> {
        self.messages.push(ChatMessage {
            role: "user".to_string(),
            content: text,
        });
        self.request_completion(None, false)
    }
}

impl ScreenplayLlmSession for GeminiSession {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::Gemini
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn greet(&mut self) -> Result<DialogTurn, ProviderError> {
        if self.greeted {
            return Err(ProviderError::Unsupported(
                "Greeting already delivered for this session.".to_string(),
            ));
        }
        let prompt = "Welcome the user and ask what kind of screenplay they would like to create.";
        let (content, telemetry) = self.request_completion(Some(prompt), false)?;
        self.greeted = true;
        Ok(DialogTurn {
            assistant_text: content.clone(),
            follow_up_questions: extract_follow_up_questions(&content),
            telemetry: Some(telemetry),
            metadata: Value::Null,
        })
    }

    fn send_user_message(&mut self, input: TurnInput) -> Result<DialogTurn, ProviderError> {
        let (content, telemetry) = self.send_user_and_complete(input.text)?;
        Ok(DialogTurn {
            follow_up_questions: extract_follow_up_questions(&content),
            assistant_text: content,
            telemetry: Some(telemetry),
            metadata: Value::Null,
        })
    }

    fn generate_screenplay(
        &mut self,
        options: DraftOptions,
    ) -> Result<LlmScreenplayDraft, ProviderError> {
        let mut prompt = String::from(
            "Using everything we have discussed so far, craft a complete screenplay as structured JSON matching the format described below. Populate each act and shot with concise, project-specific details.",
        );
        if let Some(outline) = options.outline {
            if !outline.is_empty() {
                prompt.push_str("\nFollow these story beats:");
                for beat in outline {
                    prompt.push_str(&format!("\n- {}", beat));
                }
            }
        }
        if let Some(runtime) = options.target_runtime_minutes {
            prompt.push_str(&format!(
                "\nAim for an approximate runtime of {} minutes.",
                runtime
            ));
        }
        prompt.push_str("\nRespond with JSON only—do not include prose, Markdown, or commentary.");
        prompt.push('\n');
        prompt.push_str(screenplay::screenplay_format_prompt());
        let (content, telemetry) = self.send_user_and_complete(prompt)?;
        let metadata = json!({
            "source": "gemini",
            "telemetry": telemetry_to_json(&telemetry),
        });
        Ok(LlmScreenplayDraft {
            script: content,
            synopsis: self.init.synopsis.clone(),
            metadata,
        })
    }

    fn revise(&mut self, request: RevisionRequest) -> Result<LlmScreenplayDraft, ProviderError> {
        if request.instructions.trim().is_empty() {
            return Err(ProviderError::Configuration(
                "Revision instructions cannot be empty.".to_string(),
            ));
        }
        let mut prompt = String::from(
            "Revise the screenplay according to these instructions, then return the full draft as structured JSON matching the format described below.\n",
        );
        prompt.push_str(&request.instructions);
        prompt.push_str("\nRespond with JSON only—no explanations or extra text.");
        prompt.push('\n');
        prompt.push_str(screenplay::screenplay_format_prompt());
        let (content, telemetry) = self.send_user_and_complete(prompt)?;
        let metadata = json!({
            "source": "gemini",
            "telemetry": telemetry_to_json(&telemetry),
        });
        Ok(LlmScreenplayDraft {
            script: content,
            synopsis: self.init.synopsis.clone(),
            metadata,
        })
    }
}

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsage>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContent>,
    #[allow(dead_code)]
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiContent {
    #[serde(default)]
    parts: Option<Vec<GeminiPart>>,
    #[allow(dead_code)]
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiPart {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiUsage {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: Option<u32>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: Option<u32>,
    #[serde(default, rename = "totalTokenCount")]
    total_token_count: Option<u32>,
}

fn extract_follow_up_questions(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.ends_with('?') && trimmed.len() > 3 {
                Some(
                    trimmed
                        .trim_matches(|c| c == '-' || c == '*' || c == '•')
                        .trim()
                        .to_string(),
                )
            } else {
                None
            }
        })
        .collect()
}

fn telemetry_to_json(telemetry: &LlmResponseTelemetry) -> Value {
    json!({
        "provider": telemetry.provider,
        "model": telemetry.model,
        "input_tokens": telemetry.input_tokens,
        "output_tokens": telemetry.output_tokens,
        "total_tokens": telemetry.total_tokens,
        "latency_ms": telemetry.latency.map(|d| d.as_millis() as u64),
    })
}
