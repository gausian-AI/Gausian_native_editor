pub mod gemini;
pub mod mock;
pub mod openai;

use std::fmt;

#[derive(Clone, Debug)]
pub enum ProviderConfig {
    OpenAi(openai::OpenAiConfig),
    Gemini(gemini::GeminiConfig),
    Mock(mock::MockConfig),
}

#[derive(Debug)]
pub enum ProviderError {
    Configuration(String),
    Authentication(String),
    RateLimited(String),
    Unsupported(String),
    Transport(String),
    InvalidResponse(String),
    Other(String),
}

impl ProviderError {
    pub fn configuration(msg: impl Into<String>) -> Self {
        ProviderError::Configuration(msg.into())
    }

    pub fn transport(msg: impl Into<String>) -> Self {
        ProviderError::Transport(msg.into())
    }

    pub fn invalid_response(msg: impl Into<String>) -> Self {
        ProviderError::InvalidResponse(msg.into())
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Configuration(msg)
            | ProviderError::Authentication(msg)
            | ProviderError::RateLimited(msg)
            | ProviderError::Unsupported(msg)
            | ProviderError::Transport(msg)
            | ProviderError::InvalidResponse(msg)
            | ProviderError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ProviderError {}
