use super::models::{
    DialogTurn, DraftOptions, LlmScreenplayDraft, ProviderCapabilities, ProviderKind,
    RevisionRequest, SessionInit, TurnInput,
};
use super::providers::{ProviderConfig, ProviderError};
use std::sync::Arc;

pub trait ScreenplayLlmSession: Send {
    fn provider_kind(&self) -> ProviderKind;
    fn session_id(&self) -> &str;
    fn greet(&mut self) -> Result<DialogTurn, ProviderError>;
    fn send_user_message(&mut self, input: TurnInput) -> Result<DialogTurn, ProviderError>;
    fn generate_screenplay(
        &mut self,
        options: DraftOptions,
    ) -> Result<LlmScreenplayDraft, ProviderError>;
    fn revise(&mut self, request: RevisionRequest) -> Result<LlmScreenplayDraft, ProviderError>;
}

pub trait ScreenplayLlmProvider: Send + Sync {
    fn provider_kind(&self) -> ProviderKind;
    fn model_name(&self) -> &str;
    fn capabilities(&self) -> ProviderCapabilities;
    fn create_session(
        &self,
        init: SessionInit,
    ) -> Result<Box<dyn ScreenplayLlmSession>, ProviderError>;
}

pub struct ScreenplaySessionHandle {
    inner: Box<dyn ScreenplayLlmSession>,
}

impl ScreenplaySessionHandle {
    pub fn provider_kind(&self) -> ProviderKind {
        self.inner.provider_kind()
    }

    pub fn session_id(&self) -> &str {
        self.inner.session_id()
    }

    pub fn greet(&mut self) -> Result<DialogTurn, ProviderError> {
        self.inner.greet()
    }

    pub fn send_user_message(&mut self, input: TurnInput) -> Result<DialogTurn, ProviderError> {
        self.inner.send_user_message(input)
    }

    pub fn generate_screenplay(
        &mut self,
        options: DraftOptions,
    ) -> Result<LlmScreenplayDraft, ProviderError> {
        self.inner.generate_screenplay(options)
    }

    pub fn revise(
        &mut self,
        request: RevisionRequest,
    ) -> Result<LlmScreenplayDraft, ProviderError> {
        self.inner.revise(request)
    }
}

impl From<Box<dyn ScreenplayLlmSession>> for ScreenplaySessionHandle {
    fn from(inner: Box<dyn ScreenplayLlmSession>) -> Self {
        Self { inner }
    }
}

#[derive(Clone)]
pub struct ScreenplayLlmService {
    provider: Arc<dyn ScreenplayLlmProvider>,
}

impl ScreenplayLlmService {
    pub fn new(provider: Arc<dyn ScreenplayLlmProvider>) -> Self {
        Self { provider }
    }

    pub fn provider(&self) -> Arc<dyn ScreenplayLlmProvider> {
        Arc::clone(&self.provider)
    }

    pub fn set_provider(&mut self, provider: Arc<dyn ScreenplayLlmProvider>) {
        self.provider = provider;
    }

    pub fn start_session(
        &self,
        init: SessionInit,
    ) -> Result<ScreenplaySessionHandle, ProviderError> {
        let session = self.provider.create_session(init)?;
        Ok(ScreenplaySessionHandle::from(session))
    }
}

pub trait LlmProviderFactory: Send + Sync {
    fn build(
        &self,
        config: ProviderConfig,
    ) -> Result<Arc<dyn ScreenplayLlmProvider>, ProviderError>;
}
