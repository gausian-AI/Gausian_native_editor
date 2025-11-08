use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::time::Duration;
use uuid::Uuid;

use super::service::ScreenplaySessionHandle;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

fn build_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(REQUEST_TIMEOUT)
        .timeout_read(REQUEST_TIMEOUT)
        .timeout_write(REQUEST_TIMEOUT)
        .build()
}

fn apply_auth(mut req: ureq::Request, token: Option<&str>) -> ureq::Request {
    if let Some(t) = token {
        if !t.trim().is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", t.trim()));
        }
    }
    req
}

fn read_response(res: Result<ureq::Response, ureq::Error>) -> Result<String> {
    match res {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| anyhow!("Failed to read response body: {e}")),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Err(anyhow!("{code}: {body}"))
        }
        Err(other) => Err(anyhow!("HTTP error: {other}")),
    }
}

fn get_json<T: for<'de> Deserialize<'de>>(url: &str, token: Option<&str>) -> Result<T> {
    let agent = build_agent();
    let req = agent.get(url).set("Accept", "application/json");
    let req = apply_auth(req, token);
    let body = read_response(req.call())?;
    serde_json::from_str::<T>(&body).with_context(|| format!("Failed to parse JSON from {url}"))
}

fn post_json<T: for<'de> Deserialize<'de>>(
    url: &str,
    token: Option<&str>,
    body: Value,
) -> Result<T> {
    let payload = serde_json::to_string(&body)?;
    let agent = build_agent();
    let req = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json");
    let req = apply_auth(req, token);
    let body_text = read_response(req.send_string(&payload))?;
    serde_json::from_str::<T>(&body_text)
        .with_context(|| format!("Failed to parse JSON from {url}"))
}

pub enum ScreenplayEvent {
    SessionStarted(
        Result<(
            ScreenplaySession,
            Vec<ScreenplayQuestion>,
            Option<ScreenplaySessionHandle>,
        )>,
    ),
    MessagePosted(Result<(ScreenplaySession, Vec<ScreenplayQuestion>)>),
    ScreenplayGenerated(Result<ScreenplayDraft>),
    RevisionApplied(Result<ScreenplayDraft>),
}

impl std::fmt::Debug for ScreenplayEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScreenplayEvent::SessionStarted(_) => {
                f.write_str("ScreenplayEvent::SessionStarted(..)")
            }
            ScreenplayEvent::MessagePosted(_) => f.write_str("ScreenplayEvent::MessagePosted(..)"),
            ScreenplayEvent::ScreenplayGenerated(_) => {
                f.write_str("ScreenplayEvent::ScreenplayGenerated(..)")
            }
            ScreenplayEvent::RevisionApplied(_) => {
                f.write_str("ScreenplayEvent::RevisionApplied(..)")
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiQuestion {
    id: String,
    label: String,
    question: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiAnswerSummary {
    id: String,
    label: String,
    value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiMessage {
    id: String,
    role: String,
    text: String,
    #[serde(default)]
    created_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiPromptDefaults {
    #[serde(default)]
    negative_prompt: Option<String>,
    #[serde(default)]
    frame_rate: Option<f64>,
    #[serde(default)]
    width: Option<f64>,
    #[serde(default)]
    height: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiCharacter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    visual_design: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ApiShot {
    #[serde(default)]
    id: Option<f64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    time_of_day: Option<String>,
    #[serde(default)]
    camera: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    visual_description: Option<String>,
    #[serde(default)]
    text_to_video_prompt: Option<String>,
    #[serde(default)]
    negative_prompt: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    workflow: Option<ApiShotWorkflow>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct ApiShotWorkflow {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    inputs: Option<JsonMap<String, JsonValue>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiPromptShot {
    #[serde(default)]
    id: Option<f64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    negative_prompt: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    time_of_day: Option<String>,
    #[serde(default)]
    camera: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiPromptBundle {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    style_tags: Option<Vec<String>>,
    #[serde(default)]
    defaults: Option<ApiPromptDefaults>,
    #[serde(default)]
    shots: Option<Vec<ApiPromptShot>>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiScreenplay {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    genre: Option<String>,
    #[serde(default)]
    logline: Option<String>,
    #[serde(default)]
    visual_style: Option<String>,
    #[serde(default)]
    style_tags: Option<Vec<String>>,
    #[serde(default)]
    video_prompt_defaults: Option<ApiPromptDefaults>,
    #[serde(default)]
    characters: Option<Vec<ApiCharacter>>,
    #[serde(default)]
    shots: Option<Vec<ApiShot>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiSession {
    id: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
    #[serde(default)]
    completed: Option<bool>,
    #[serde(default)]
    question_index: Option<u32>,
    #[serde(default)]
    answers: Option<Vec<ApiAnswerSummary>>,
    #[serde(default)]
    messages: Option<Vec<ApiMessage>>,
    #[serde(default)]
    screenplay: Option<ApiScreenplay>,
    #[serde(default)]
    prompts: Option<ApiPromptBundle>,
}

#[derive(Debug, Clone, Deserialize)]
struct StartSessionResponse {
    session: ApiSession,
    #[serde(default)]
    questions: Option<Vec<ApiQuestion>>,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionResponse {
    session: ApiSession,
}

#[derive(Debug, Clone, Deserialize)]
struct ScreenplayResponse {
    screenplay: ApiScreenplay,
}

#[derive(Debug, Clone, Deserialize)]
struct PromptsResponse {
    prompts: ApiPromptBundle,
}

#[derive(Debug, Clone)]
pub struct ScreenplayQuestion {
    pub id: String,
    pub label: String,
    pub question: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    Assistant,
    User,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ScreenplayMessage {
    pub id: String,
    pub role: ChatRole,
    pub text: String,
    pub created_at_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ScreenplayAnswer {
    pub id: String,
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct ScreenplayCharacter {
    pub name: String,
    pub role: String,
    pub description: String,
    pub visual_design: String,
}

#[derive(Debug, Clone)]
pub struct ScreenplayShot {
    pub id: u32,
    pub title: String,
    pub duration: f32,
    pub location: String,
    pub time_of_day: String,
    pub camera: String,
    pub action: String,
    pub visual_description: String,
    pub prompt: String,
    pub negative_prompt: String,
    pub notes: String,
    pub workflow_key: Option<String>,
    pub workflow_id: Option<Uuid>,
    pub workflow_inputs: JsonMap<String, JsonValue>,
}

#[derive(Debug, Clone)]
pub struct ScreenplayAct {
    pub id: u32,
    pub title: String,
    pub summary: String,
    pub shots: Vec<ScreenplayShot>,
}

#[derive(Debug, Clone)]
pub struct PromptShot {
    pub id: u32,
    pub title: String,
    pub prompt: String,
    pub negative_prompt: String,
    pub duration: f32,
    pub location: String,
    pub time_of_day: String,
    pub camera: String,
}

#[derive(Debug, Clone)]
pub struct PromptBundle {
    pub title: String,
    pub style_tags: Vec<String>,
    pub defaults: PromptDefaults,
    pub shots: Vec<PromptShot>,
}

#[derive(Debug, Clone)]
pub struct PromptDefaults {
    pub negative_prompt: String,
    pub frame_rate: f32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct ScreenplayDraft {
    pub title: String,
    pub genre: String,
    pub logline: String,
    pub visual_style: String,
    pub style_tags: Vec<String>,
    pub duration_minutes: Option<f32>,
    pub defaults: PromptDefaults,
    pub characters: Vec<ScreenplayCharacter>,
    pub shots: Vec<ScreenplayShot>,
    pub acts: Vec<ScreenplayAct>,
}

#[derive(Debug, Clone)]
pub struct ScreenplaySession {
    pub id: String,
    pub project_id: Option<String>,
    pub created_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub completed: bool,
    pub question_index: usize,
    pub answers: Vec<ScreenplayAnswer>,
    pub messages: Vec<ScreenplayMessage>,
    pub screenplay: Option<ScreenplayDraft>,
    pub prompts: Option<PromptBundle>,
}

impl From<ApiQuestion> for ScreenplayQuestion {
    fn from(src: ApiQuestion) -> Self {
        Self {
            id: src.id,
            label: src.label,
            question: src.question,
        }
    }
}

fn to_role(role: &str) -> ChatRole {
    match role.to_ascii_lowercase().as_str() {
        "assistant" => ChatRole::Assistant,
        "system" => ChatRole::Assistant,
        "user" => ChatRole::User,
        _ => ChatRole::Unknown,
    }
}

fn convert_messages(src: Option<Vec<ApiMessage>>) -> Vec<ScreenplayMessage> {
    src.unwrap_or_default()
        .into_iter()
        .map(|msg| ScreenplayMessage {
            id: msg.id,
            role: to_role(&msg.role),
            text: msg.text,
            created_at_ms: msg.created_at,
        })
        .collect()
}

fn convert_answers(src: Option<Vec<ApiAnswerSummary>>) -> Vec<ScreenplayAnswer> {
    src.unwrap_or_default()
        .into_iter()
        .map(|ans| ScreenplayAnswer {
            id: ans.id,
            label: ans.label,
            value: ans.value,
        })
        .collect()
}

fn convert_defaults(src: Option<ApiPromptDefaults>) -> PromptDefaults {
    let defaults = src.unwrap_or(ApiPromptDefaults {
        negative_prompt: None,
        frame_rate: None,
        width: None,
        height: None,
    });
    PromptDefaults {
        negative_prompt: defaults
            .negative_prompt
            .unwrap_or_else(|| "blurry, low quality".to_string()),
        frame_rate: defaults.frame_rate.unwrap_or(24.0) as f32,
        width: defaults.width.unwrap_or(832.0) as u32,
        height: defaults.height.unwrap_or(480.0) as u32,
    }
}

fn convert_characters(src: Option<Vec<ApiCharacter>>) -> Vec<ScreenplayCharacter> {
    src.unwrap_or_default()
        .into_iter()
        .map(|c| ScreenplayCharacter {
            name: c.name.unwrap_or_else(|| "Unnamed character".to_string()),
            role: c.role.unwrap_or_else(|| "cast".to_string()),
            description: c.description.unwrap_or_default(),
            visual_design: c.visual_design.unwrap_or_default(),
        })
        .collect()
}

fn convert_shots(src: Option<Vec<ApiShot>>, defaults: &PromptDefaults) -> Vec<ScreenplayShot> {
    src.unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(idx, shot)| {
            let ApiShot {
                id,
                title,
                duration,
                location,
                time_of_day,
                camera,
                action,
                visual_description,
                text_to_video_prompt,
                negative_prompt,
                notes,
                workflow,
            } = shot;
            let id_val = id.unwrap_or((idx + 1) as f64);
            let visual_description = visual_description.unwrap_or_default();
            let prompt = text_to_video_prompt.unwrap_or_else(|| visual_description.clone());
            let (workflow_key, workflow_id, workflow_inputs) = workflow
                .map(|wf| {
                    let key = wf
                        .key
                        .map(|k| k.trim().to_string())
                        .filter(|k| !k.is_empty());
                    let workflow_id = wf.id.and_then(|raw| Uuid::parse_str(raw.trim()).ok());
                    let inputs = wf.inputs.unwrap_or_default();
                    (key, workflow_id, inputs)
                })
                .unwrap_or_else(|| (None, None, JsonMap::new()));
            ScreenplayShot {
                id: id_val.round().max(1.0) as u32,
                title: title.unwrap_or_else(|| format!("Shot {}", idx + 1)),
                duration: duration.unwrap_or(5.0) as f32,
                location: location.unwrap_or_default(),
                time_of_day: time_of_day.unwrap_or_default(),
                camera: camera.unwrap_or_default(),
                action: action.unwrap_or_default(),
                visual_description,
                prompt,
                negative_prompt: negative_prompt
                    .unwrap_or_else(|| defaults.negative_prompt.clone()),
                notes: notes.unwrap_or_default(),
                workflow_key,
                workflow_id,
                workflow_inputs,
            }
        })
        .collect()
}

fn convert_prompt_shots(
    src: Option<Vec<ApiPromptShot>>,
    defaults: &PromptDefaults,
) -> Vec<PromptShot> {
    src.unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(idx, shot)| {
            let id_val = shot.id.unwrap_or((idx + 1) as f64);
            PromptShot {
                id: id_val.round().max(1.0) as u32,
                title: shot.title.unwrap_or_else(|| format!("Shot {}", idx + 1)),
                prompt: shot.prompt.unwrap_or_default(),
                negative_prompt: shot
                    .negative_prompt
                    .unwrap_or_else(|| defaults.negative_prompt.clone()),
                duration: shot.duration.unwrap_or(5.0) as f32,
                location: shot.location.unwrap_or_default(),
                time_of_day: shot.time_of_day.unwrap_or_default(),
                camera: shot.camera.unwrap_or_default(),
            }
        })
        .collect()
}

fn convert_screenplay(api: ApiScreenplay) -> ScreenplayDraft {
    let defaults = convert_defaults(api.video_prompt_defaults);
    ScreenplayDraft {
        title: api.title.unwrap_or_else(|| "Untitled Film".to_string()),
        genre: api.genre.unwrap_or_else(|| "Drama".to_string()),
        logline: api.logline.unwrap_or_default(),
        visual_style: api.visual_style.unwrap_or_else(|| "Cinematic".to_string()),
        style_tags: api.style_tags.unwrap_or_default(),
        duration_minutes: None,
        defaults: defaults.clone(),
        characters: convert_characters(api.characters),
        shots: convert_shots(api.shots, &defaults),
        acts: Vec::new(),
    }
}

fn convert_prompts(api: ApiPromptBundle) -> PromptBundle {
    let defaults = convert_defaults(api.defaults);
    PromptBundle {
        title: api
            .title
            .unwrap_or_else(|| "Screenplay Prompts".to_string()),
        style_tags: api.style_tags.unwrap_or_default(),
        defaults: defaults.clone(),
        shots: convert_prompt_shots(api.shots, &defaults),
    }
}

fn convert_session(api: ApiSession) -> ScreenplaySession {
    let defaults_screenplay = api.screenplay.clone().map(convert_screenplay);
    let defaults_prompts = api.prompts.clone().map(convert_prompts);
    ScreenplaySession {
        id: api.id,
        project_id: api.project_id,
        created_at_ms: api.created_at,
        updated_at_ms: api.updated_at,
        completed: api.completed.unwrap_or(false),
        question_index: api.question_index.unwrap_or(0) as usize,
        answers: convert_answers(api.answers),
        messages: convert_messages(api.messages),
        screenplay: defaults_screenplay,
        prompts: defaults_prompts,
    }
}

pub fn start_session(
    base_url: &str,
    token: Option<&str>,
    project_id: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<(
    ScreenplaySession,
    Vec<ScreenplayQuestion>,
    Option<ScreenplaySessionHandle>,
)> {
    let url = format!("{}/sessions", base_url.trim_end_matches('/'));
    let mut body = json!({});
    if let Some(pid) = project_id {
        if !pid.trim().is_empty() {
            body["projectId"] = Value::String(pid.trim().to_string());
        }
    }
    if let Some(provider) = provider {
        body["provider"] = Value::String(provider.to_string());
    }
    if let Some(model) = model {
        body["model"] = Value::String(model.to_string());
    }
    let response: StartSessionResponse = post_json(&url, token, body)?;
    let questions = response
        .questions
        .unwrap_or_default()
        .into_iter()
        .map(ScreenplayQuestion::from)
        .collect();
    Ok((convert_session(response.session), questions, None))
}

pub fn post_message(
    base_url: &str,
    token: Option<&str>,
    session_id: &str,
    message: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<(ScreenplaySession, Vec<ScreenplayQuestion>)> {
    if message.trim().is_empty() {
        return Err(anyhow!("Message cannot be empty"));
    }
    let url = format!(
        "{}/sessions/{}/messages",
        base_url.trim_end_matches('/'),
        urlencoding::encode(session_id)
    );
    let mut body = json!({ "message": message });
    if let Some(provider) = provider {
        body["provider"] = Value::String(provider.to_string());
    }
    if let Some(model) = model {
        body["model"] = Value::String(model.to_string());
    }
    let response: SessionResponse = post_json(&url, token, body)?;
    Ok((convert_session(response.session), Vec::new()))
}

pub fn generate_screenplay(
    base_url: &str,
    token: Option<&str>,
    session_id: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<ScreenplayDraft> {
    let url = format!(
        "{}/sessions/{}/generate",
        base_url.trim_end_matches('/'),
        urlencoding::encode(session_id)
    );
    let mut body = json!({});
    if let Some(provider) = provider {
        body["provider"] = Value::String(provider.to_string());
    }
    if let Some(model) = model {
        body["model"] = Value::String(model.to_string());
    }
    let response: ScreenplayResponse = post_json(&url, token, body)?;
    Ok(convert_screenplay(response.screenplay))
}

pub fn fetch_prompts(
    base_url: &str,
    token: Option<&str>,
    session_id: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<PromptBundle> {
    let url = format!(
        "{}/sessions/{}/prompts",
        base_url.trim_end_matches('/'),
        urlencoding::encode(session_id)
    );
    let mut body = json!({});
    if let Some(provider) = provider {
        body["provider"] = Value::String(provider.to_string());
    }
    if let Some(model) = model {
        body["model"] = Value::String(model.to_string());
    }
    let response: PromptsResponse = post_json(&url, token, body)?;
    Ok(convert_prompts(response.prompts))
}

pub fn get_session(
    base_url: &str,
    token: Option<&str>,
    session_id: &str,
) -> Result<ScreenplaySession> {
    let url = format!(
        "{}/sessions/{}",
        base_url.trim_end_matches('/'),
        urlencoding::encode(session_id)
    );
    let response: SessionResponse = get_json(&url, token)?;
    Ok(convert_session(response.session))
}
