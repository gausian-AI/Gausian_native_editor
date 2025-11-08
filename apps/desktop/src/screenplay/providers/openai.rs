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

const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";
const CHAT_COMPLETIONS_PATH: &str = "chat/completions";
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful creative writing assistant collaborating with a filmmaker to develop screenplays and storyboards. Ask clear questions, keep responses concise, and use industry-standard screenplay terminology.";

#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub model: String,
    pub organization: Option<String>,
    pub system_prompt: Option<String>,
    pub temperature: f32,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
            organization: None,
            system_prompt: None,
            temperature: 0.7,
        }
    }
}

struct OpenAiShared {
    config: OpenAiConfig,
    agent: ureq::Agent,
}

impl OpenAiShared {
    fn new(config: OpenAiConfig) -> Result<Self, ProviderError> {
        if config.api_key.trim().is_empty() {
            return Err(ProviderError::Configuration(
                "OpenAI API key is required.".to_string(),
            ));
        }
        if config.model.trim().is_empty() {
            return Err(ProviderError::Configuration(
                "OpenAI model name is required.".to_string(),
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
        format!("{DEFAULT_API_BASE}/{CHAT_COMPLETIONS_PATH}")
    }

    fn execute_chat(
        &self,
        messages: &[ChatMessage],
    ) -> Result<(ChatCompletionChoice, LlmResponseTelemetry), ProviderError> {
        let payload = json!({
            "model": self.config.model,
            "temperature": self.config.temperature,
            "messages": messages.iter().map(|m| json!({
                "role": m.role,
                "content": m.content,
            })).collect::<Vec<_>>(),
        });
        let start = Instant::now();
        tracing::info!(
            target: "screenplay",
            "OpenAI request start: model={}, messages={}",
            self.config.model,
            messages.len()
        );
        let mut request = self
            .agent
            .post(&self.endpoint())
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .set("Authorization", &format!("Bearer {}", self.config.api_key));
        if let Some(org) = self.config.organization.as_deref() {
            if !org.trim().is_empty() {
                request = request.set("OpenAI-Organization", org.trim());
            }
        }
        let response = request
            .send_string(&payload.to_string())
            .map_err(|err| ProviderError::transport(format!("OpenAI request failed: {err}")))?;
        let elapsed = start.elapsed();
        let response_body = response.into_string().map_err(|err| {
            ProviderError::transport(format!("Read OpenAI response failed: {err}"))
        })?;
        let parsed: ChatCompletionResponse =
            serde_json::from_str(&response_body).map_err(|err| {
                ProviderError::invalid_response(format!("Invalid OpenAI response JSON: {err}"))
            })?;
        let choice =
            parsed.choices.into_iter().next().ok_or_else(|| {
                ProviderError::invalid_response("OpenAI response had no choices.")
            })?;
        let telemetry = LlmResponseTelemetry {
            provider: ProviderKind::OpenAi.as_str().to_string(),
            model: self.config.model.clone(),
            input_tokens: parsed.usage.as_ref().and_then(|u| u.prompt_tokens),
            output_tokens: parsed.usage.as_ref().and_then(|u| u.completion_tokens),
            total_tokens: parsed.usage.as_ref().and_then(|u| u.total_tokens),
            latency: Some(elapsed),
        };
        tracing::info!(
            target: "screenplay",
            "OpenAI request completed in {:.2?} (input_tokens={:?}, output_tokens={:?})",
            elapsed,
            telemetry.input_tokens,
            telemetry.output_tokens
        );
        Ok((choice, telemetry))
    }
}

#[derive(Clone, Debug)]
struct ChatMessage {
    role: String,
    content: String,
}

pub struct OpenAiProvider {
    shared: Arc<OpenAiShared>,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Result<Self, ProviderError> {
        let shared = OpenAiShared::new(config)?;
        Ok(Self {
            shared: Arc::new(shared),
        })
    }

    fn compose_system_prompt(config: &OpenAiConfig, init: &SessionInit) -> String {
        let mut prompt = config
            .system_prompt
            .clone()
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
        if let Some(title) = init.working_title.as_deref() {
            if !title.trim().is_empty() {
                prompt.push_str(&format!("\nProject working title: \"{}\".", title.trim()));
            }
        }
        if let Some(genre) = init.genre.as_deref() {
            if !genre.trim().is_empty() {
                prompt.push_str(&format!("\nTarget genre: {}.", genre.trim()));
            }
        }
        if let Some(tone) = init.tone.as_deref() {
            if !tone.trim().is_empty() {
                prompt.push_str(&format!("\nDesired tone: {}.", tone.trim()));
            }
        }
        if let Some(synopsis) = init.synopsis.as_deref() {
            if !synopsis.trim().is_empty() {
                prompt.push_str(&format!("\nInitial synopsis:\n{}", synopsis.trim()));
            }
        }
        if let Some(context) = init.additional_context.as_deref() {
            if !context.trim().is_empty() {
                prompt.push_str(&format!(
                    "\nAdditional context provided by the user:\n{}",
                    context.trim()
                ));
            }
        }
        prompt.push_str("\n\nFocus on gathering only these essentials before drafting (skip everything else unless the user volunteers it):\n");
        prompt.push_str(screenplay::screenplay_requirements_prompt());
        prompt.push_str("\nAssume a short-form (1–5 minute) film. Do not ask about output format, draft scope, or continuation policy unless the user brings it up. If the user does not specify a title or synopsis, invent appropriate ones.");
        prompt.push_str("\n\nWhen proposing structures, reference these plot templates:\n");
        prompt.push_str(screenplay::plot_types_prompt());
        prompt.push_str("\n\nWhen drafting, return the screenplay as structured JSON only (see the format below). Do not wrap the output in prose or Markdown.\n");
        prompt.push_str(screenplay::screenplay_format_prompt());
        prompt
    }
}

impl ScreenplayLlmProvider for OpenAiProvider {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
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
        let mut messages = Vec::new();
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        });
        let session = OpenAiSession {
            shared: Arc::clone(&self.shared),
            session_id: Uuid::new_v4().to_string(),
            messages,
            greeted: false,
            init,
        };
        Ok(Box::new(session))
    }
}

struct OpenAiSession {
    shared: Arc<OpenAiShared>,
    session_id: String,
    messages: Vec<ChatMessage>,
    greeted: bool,
    init: SessionInit,
}

impl OpenAiSession {
    fn request_completion(
        &mut self,
        transient_user: Option<&str>,
        persist_transient: bool,
    ) -> Result<(String, LlmResponseTelemetry), ProviderError> {
        let mut payload = self.messages.clone();
        if let Some(content) = transient_user {
            let msg = ChatMessage {
                role: "user".to_string(),
                content: content.to_string(),
            };
            payload.push(msg.clone());
            if persist_transient {
                self.messages.push(msg);
            }
        }
        let (choice, telemetry) = self.shared.execute_chat(&payload)?;
        let message = choice.message.ok_or_else(|| {
            ProviderError::invalid_response("OpenAI choice missing assistant message.")
        })?;
        let assistant_content = message.content.unwrap_or_default();
        self.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: assistant_content.clone(),
        });
        Ok((assistant_content, telemetry))
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

impl ScreenplayLlmSession for OpenAiSession {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn greet(&mut self) -> Result<DialogTurn, ProviderError> {
        if self.greeted {
            return Err(ProviderError::Unsupported(
                "Greeting has already been delivered for this session.".to_string(),
            ));
        }
        let prompt = "Please greet the user warmly, briefly confirm you are here to help craft a screenplay, and ask what kind of story they would like to create.";
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
        let followups = extract_follow_up_questions(&content);
        Ok(DialogTurn {
            assistant_text: content,
            follow_up_questions: followups,
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
                prompt.push_str("\nFocus on hitting these beats (in order):");
                for beat in outline {
                    prompt.push_str(&format!("\n- {}", beat));
                }
            }
        }
        if let Some(runtime) = options.target_runtime_minutes {
            prompt.push_str(&format!(
                "\nTarget an approximate runtime of {} minutes.",
                runtime
            ));
        }
        if options.include_style_notes {
            prompt.push_str("\nRespect the tone and genre guidance discussed earlier.");
        }
        prompt.push_str("\nRespond with JSON only—do not include prose, Markdown, or commentary.");
        prompt.push('\n');
        prompt.push_str(screenplay::screenplay_format_prompt());
        let (content, telemetry) = self.send_user_and_complete(prompt)?;
        let metadata = json!({
            "source": "openai",
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
            "Revise the screenplay according to these instructions, then return the entire draft as structured JSON matching the format described below.\n",
        );
        prompt.push_str(&request.instructions);
        use super::super::models::RevisionScope;
        match request.scope {
            RevisionScope::EntireDraft => {
                prompt.push_str("\nApply changes across the entire screenplay.");
            }
            RevisionScope::Scenes(ref scenes) if !scenes.is_empty() => {
                prompt.push_str("\nFocus specifically on these scenes:\n");
                for scene in scenes {
                    prompt.push_str(&format!("- {}\n", scene));
                }
            }
            RevisionScope::Characters(ref names) if !names.is_empty() => {
                prompt
                    .push_str("\nEnsure the following characters reflect the requested changes:\n");
                for name in names {
                    prompt.push_str(&format!("- {}\n", name));
                }
            }
            RevisionScope::Beats(ref beats) if !beats.is_empty() => {
                prompt.push_str("\nPay special attention to these beats:\n");
                for beat in beats {
                    prompt.push_str(&format!("- {}\n", beat));
                }
            }
            _ => {}
        }
        prompt.push_str("\nRespond with JSON only—no explanations or extra text.");
        prompt.push('\n');
        prompt.push_str(screenplay::screenplay_format_prompt());
        let (content, telemetry) = self.send_user_and_complete(prompt)?;
        let metadata = json!({
            "source": "openai",
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
struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChoice {
    message: Option<ChatCompletionMessage>,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionMessage {
    role: Option<String>,
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    total_tokens: Option<u32>,
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
