//! Pegasus footage analysis via the Twelve Labs video-understanding API.
//!
//! This is an **opt-in** helper for AI video production: given a clip (a public
//! URL or a previously uploaded asset id) and a natural-language prompt, it asks
//! Twelve Labs' Pegasus model to describe, summarise, or otherwise analyse the
//! footage and returns the generated text. Nothing here runs unless it is called
//! explicitly with an API key, so it does not affect any existing behaviour.
//!
//! The transport mirrors the screenplay LLM providers (a blocking `ureq` agent
//! with serde request/response models) so it slots into the existing HTTP style.
//!
//! ```no_run
//! use desktop::footage_analysis::{FootageAnalyzer, PegasusConfig, VideoSource};
//!
//! let analyzer = FootageAnalyzer::new(PegasusConfig::new(std::env::var("TWELVELABS_API_KEY").unwrap()))?;
//! let result = analyzer.analyze(
//!     VideoSource::url("https://example.com/clip.mp4"),
//!     "Summarise the action and list every distinct shot.",
//! )?;
//! println!("{}", result.text);
//! # Ok::<(), desktop::footage_analysis::FootageAnalysisError>(())
//! ```

use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

/// Base URL of the Twelve Labs REST API.
pub const TWELVELABS_API_BASE: &str = "https://api.twelvelabs.io/v1.3";
const ANALYZE_PATH: &str = "analyze";
const DEFAULT_MODEL: &str = "pegasus1.5";
const DEFAULT_MAX_TOKENS: u32 = 2048;
const DEFAULT_TEMPERATURE: f32 = 0.2;

/// Configuration for a [`FootageAnalyzer`].
#[derive(Clone, Debug)]
pub struct PegasusConfig {
    /// Twelve Labs API key (sent as the `x-api-key` header). Required.
    pub api_key: String,
    /// Pegasus model name. Defaults to `pegasus1.5`.
    pub model: String,
    /// Upper bound on generated tokens. Defaults to 2048.
    pub max_tokens: u32,
    /// Sampling temperature in `0.0..=1.0`. Defaults to 0.2.
    pub temperature: f32,
}

impl PegasusConfig {
    /// Build a config from an API key, using the documented defaults.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: DEFAULT_TEMPERATURE,
        }
    }
}

impl Default for PegasusConfig {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// The footage to analyse. Provide exactly one source.
#[derive(Clone, Debug)]
pub enum VideoSource {
    /// A direct, server-reachable `http(s)` link to a raw media file.
    /// Share links (YouTube/Drive/Dropbox) are not accepted by the API.
    Url(String),
    /// The id of a clip already uploaded to Twelve Labs as an asset.
    AssetId(String),
}

impl VideoSource {
    /// Analyse footage from a direct media URL.
    pub fn url(url: impl Into<String>) -> Self {
        VideoSource::Url(url.into())
    }

    /// Analyse a previously uploaded Twelve Labs asset.
    pub fn asset_id(id: impl Into<String>) -> Self {
        VideoSource::AssetId(id.into())
    }

    fn to_request_value(&self) -> Value {
        match self {
            VideoSource::Url(url) => json!({ "type": "url", "url": url }),
            VideoSource::AssetId(id) => json!({ "type": "asset", "asset_id": id }),
        }
    }
}

/// The result of a Pegasus footage-analysis call.
#[derive(Clone, Debug)]
pub struct FootageAnalysis {
    /// The generated analysis text.
    pub text: String,
    /// Why generation stopped (e.g. `"stop"`), when reported by the API.
    pub finish_reason: Option<String>,
    /// Number of output tokens billed, when reported by the API.
    pub output_tokens: Option<u32>,
}

/// Errors that can occur while analysing footage.
#[derive(Debug)]
pub enum FootageAnalysisError {
    /// A configuration value (API key, prompt) is missing or invalid.
    Configuration(String),
    /// The HTTP request itself failed (network, TLS, timeout, non-2xx status).
    Transport(String),
    /// The response could not be parsed into the expected shape.
    InvalidResponse(String),
}

impl fmt::Display for FootageAnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FootageAnalysisError::Configuration(msg)
            | FootageAnalysisError::Transport(msg)
            | FootageAnalysisError::InvalidResponse(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for FootageAnalysisError {}

/// A thin client over the Twelve Labs `analyze` endpoint.
pub struct FootageAnalyzer {
    config: PegasusConfig,
    agent: ureq::Agent,
}

impl FootageAnalyzer {
    /// Build an analyzer, validating the config.
    pub fn new(config: PegasusConfig) -> Result<Self, FootageAnalysisError> {
        if config.api_key.trim().is_empty() {
            return Err(FootageAnalysisError::Configuration(
                "Twelve Labs API key is required.".to_string(),
            ));
        }
        if config.model.trim().is_empty() {
            return Err(FootageAnalysisError::Configuration(
                "Pegasus model name is required.".to_string(),
            ));
        }
        // Video analysis can take a while server-side; give reads a generous budget.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(300))
            .timeout_write(Duration::from_secs(20))
            .build();
        Ok(Self { config, agent })
    }

    /// The Pegasus model this analyzer is configured to use.
    pub fn model_name(&self) -> &str {
        &self.config.model
    }

    fn endpoint(&self) -> String {
        format!("{TWELVELABS_API_BASE}/{ANALYZE_PATH}")
    }

    /// Build the JSON request body for an analysis call. Kept separate from the
    /// network call so the wiring can be unit-tested without an API key.
    fn build_payload(&self, source: &VideoSource, prompt: &str) -> Value {
        json!({
            "video": source.to_request_value(),
            "prompt": prompt,
            "model_name": self.config.model,
            "max_tokens": self.config.max_tokens,
            "temperature": self.config.temperature,
            // Request a single JSON body rather than the default token stream.
            "stream": false,
        })
    }

    /// Analyse `source` with `prompt` and return the generated text.
    pub fn analyze(
        &self,
        source: VideoSource,
        prompt: impl AsRef<str>,
    ) -> Result<FootageAnalysis, FootageAnalysisError> {
        let prompt = prompt.as_ref();
        if prompt.trim().is_empty() {
            return Err(FootageAnalysisError::Configuration(
                "Analysis prompt cannot be empty.".to_string(),
            ));
        }
        let payload = self.build_payload(&source, prompt);

        tracing::info!(
            target: "footage_analysis",
            "Pegasus analyze request: model={}, max_tokens={}",
            self.config.model,
            self.config.max_tokens
        );

        let response = self
            .agent
            .post(&self.endpoint())
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .set("x-api-key", &self.config.api_key)
            .send_string(&payload.to_string())
            .map_err(|err| {
                FootageAnalysisError::Transport(format!("Twelve Labs request failed: {err}"))
            })?;

        let body = response.into_string().map_err(|err| {
            FootageAnalysisError::Transport(format!("Read Twelve Labs response failed: {err}"))
        })?;

        parse_analysis(&body)
    }
}

#[derive(Debug, Deserialize)]
struct AnalyzeResponse {
    data: Option<String>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    output_tokens: Option<u32>,
}

/// Parse the non-streaming `analyze` response body into a [`FootageAnalysis`].
fn parse_analysis(body: &str) -> Result<FootageAnalysis, FootageAnalysisError> {
    let parsed: AnalyzeResponse = serde_json::from_str(body).map_err(|err| {
        FootageAnalysisError::InvalidResponse(format!("Invalid Twelve Labs response JSON: {err}"))
    })?;
    let text = parsed
        .data
        .ok_or_else(|| {
            FootageAnalysisError::InvalidResponse(
                "Twelve Labs response did not include analysis text.".to_string(),
            )
        })?
        .trim()
        .to_string();
    Ok(FootageAnalysis {
        text,
        finish_reason: parsed.finish_reason,
        output_tokens: parsed.usage.and_then(|u| u.output_tokens),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_api_key() {
        let result = FootageAnalyzer::new(PegasusConfig::new("   "));
        assert!(matches!(
            result,
            Err(FootageAnalysisError::Configuration(_))
        ));
    }

    #[test]
    fn rejects_empty_prompt() {
        let analyzer = FootageAnalyzer::new(PegasusConfig::new("test-key")).unwrap();
        let err = analyzer
            .analyze(VideoSource::url("https://example.com/clip.mp4"), "  ")
            .unwrap_err();
        assert!(matches!(err, FootageAnalysisError::Configuration(_)));
    }

    #[test]
    fn payload_wires_url_source_and_defaults() {
        let analyzer = FootageAnalyzer::new(PegasusConfig::new("test-key")).unwrap();
        let payload = analyzer.build_payload(
            &VideoSource::url("https://example.com/clip.mp4"),
            "Describe it",
        );
        assert_eq!(payload["video"]["type"], "url");
        assert_eq!(payload["video"]["url"], "https://example.com/clip.mp4");
        assert_eq!(payload["prompt"], "Describe it");
        assert_eq!(payload["model_name"], "pegasus1.5");
        assert_eq!(payload["max_tokens"], 2048);
        assert_eq!(payload["stream"], false);
    }

    #[test]
    fn payload_wires_asset_source() {
        let analyzer = FootageAnalyzer::new(PegasusConfig::new("test-key")).unwrap();
        let payload = analyzer.build_payload(&VideoSource::asset_id("asset_123"), "Describe it");
        assert_eq!(payload["video"]["type"], "asset");
        assert_eq!(payload["video"]["asset_id"], "asset_123");
    }

    #[test]
    fn parses_successful_response() {
        let body = r#"{"id":"abc","data":"\n\nA wide shot of a forest.","finish_reason":"stop","usage":{"output_tokens":7}}"#;
        let result = parse_analysis(body).unwrap();
        assert_eq!(result.text, "A wide shot of a forest.");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
        assert_eq!(result.output_tokens, Some(7));
    }

    #[test]
    fn missing_data_is_invalid_response() {
        let body = r#"{"id":"abc","finish_reason":"stop"}"#;
        let err = parse_analysis(body).unwrap_err();
        assert!(matches!(err, FootageAnalysisError::InvalidResponse(_)));
    }
}
