use super::super::models::{
    DialogTurn, DraftOptions, LlmScreenplayDraft, ProviderCapabilities, ProviderKind,
    RevisionRequest, SessionInit, TurnInput,
};
use super::super::service::{ScreenplayLlmProvider, ScreenplayLlmSession};
use super::ProviderError;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
pub struct MockConfig {
    pub scripted_turns: Vec<String>,
    pub screenplay_stub: Option<String>,
}

pub struct MockLlmProvider {
    config: Arc<MockConfig>,
}

impl MockLlmProvider {
    pub fn new(config: MockConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

impl ScreenplayLlmProvider for MockLlmProvider {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::Mock
    }

    fn model_name(&self) -> &str {
        "gausian-mock-llm"
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
        Ok(Box::new(MockSession {
            session_id: Uuid::new_v4().to_string(),
            remaining_turns: VecDeque::from(self.config.scripted_turns.clone()),
            init,
            screenplay_stub: self.config.screenplay_stub.clone(),
        }))
    }
}

struct MockSession {
    session_id: String,
    remaining_turns: VecDeque<String>,
    init: SessionInit,
    screenplay_stub: Option<String>,
}

impl MockSession {
    fn next_response(&mut self, fallback: &str) -> String {
        self.remaining_turns
            .pop_front()
            .unwrap_or_else(|| fallback.to_string())
    }
}

impl ScreenplayLlmSession for MockSession {
    fn provider_kind(&self) -> ProviderKind {
        ProviderKind::Mock
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn greet(&mut self) -> Result<DialogTurn, ProviderError> {
        let text = self.next_response(
            "Hello! I'm your screenplay assistant. What kind of story are you hoping to create today?",
        );
        Ok(DialogTurn {
            assistant_text: text.clone(),
            follow_up_questions: vec!["What genre are you considering?".to_string()],
            telemetry: None,
            metadata: Value::Null,
        })
    }

    fn send_user_message(&mut self, input: TurnInput) -> Result<DialogTurn, ProviderError> {
        let text = self.next_response(&format!(
            "Thanks for the detail about '{}'. What aspect should we explore next?",
            input.text.trim()
        ));
        Ok(DialogTurn {
            assistant_text: text.clone(),
            follow_up_questions: vec!["Would you like to outline the main beats?".to_string()],
            telemetry: None,
            metadata: Value::Null,
        })
    }

    fn generate_screenplay(
        &mut self,
        _options: DraftOptions,
    ) -> Result<LlmScreenplayDraft, ProviderError> {
        let script = self.screenplay_stub.clone().unwrap_or_else(|| {
            let title = self
                .init
                .working_title
                .clone()
                .unwrap_or_else(|| "Untitled Project".to_string());
            let synopsis = self
                .init
                .synopsis
                .clone()
                .unwrap_or_else(|| format!("A mock screenplay for '{title}'."));
            let payload = json!({
                "title": title,
                "genre": "Drama",
                "duration": 5,
                "synopsis": synopsis,
                "acts": [
                    {
                        "id": 1,
                        "title": "Act I – Mock Setup",
                        "summary": "Establishes the testing scenario for the mock screenplay.",
                        "shots": [
                            {
                                "id": 1,
                                "title": "Opening Placeholder",
                                "visual_description": "A simple shot representing the beginning of the mock screenplay.",
                                "location": "Mock Stage",
                                "characters": ["Lead"],
                                "duration": 8.0,
                                "camera": {
                                    "movement": "static",
                                    "angle": "medium shot"
                                },
                                "sound": {
                                    "music": "gentle underscore",
                                    "fx": "",
                                    "dialogue": [
                                        {
                                            "character": "Lead",
                                            "line": "This is a mock screenplay entry used for tests."
                                        }
                                    ]
                                }
                            }
                        ]
                    }
                ]
            });
            serde_json::to_string_pretty(&payload).unwrap()
        });
        Ok(LlmScreenplayDraft {
            script,
            synopsis: self.init.synopsis.clone(),
            metadata: Value::Null,
        })
    }

    fn revise(&mut self, request: RevisionRequest) -> Result<LlmScreenplayDraft, ProviderError> {
        let title = self
            .init
            .working_title
            .clone()
            .unwrap_or_else(|| "Untitled Project".to_string());
        let synopsis = self
            .init
            .synopsis
            .clone()
            .unwrap_or_else(|| format!("A mock screenplay for '{title}'."));
        let instructions_text = request.instructions.trim().to_string();
        let payload = json!({
            "title": title,
            "genre": "Drama",
            "duration": 5,
            "synopsis": format!("Revised mock screenplay. Applied instructions: {instructions_text}"),
            "acts": [
                {
                    "id": 1,
                    "title": "Act I – Mock Revision",
                    "summary": "Demonstrates how revisions are echoed inside the mock screenplay.",
                    "shots": [
                        {
                            "id": 1,
                            "title": "Revision Placeholder",
                            "visual_description": "A shot showing the mock screenplay after revisions.",
                            "location": "Mock Stage",
                            "characters": ["Lead"],
                            "duration": 8.0,
                            "camera": {
                                "movement": "static",
                                "angle": "medium shot"
                            },
                            "sound": {
                                "music": "gentle underscore",
                                "fx": "",
                                "dialogue": [
                                    {
                                        "character": "Lead",
                                        "line": format!("Revision instructions acknowledged: {instructions_text}")
                                    }
                                ]
                            }
                        }
                    ]
                }
            ]
        });
        Ok(LlmScreenplayDraft {
            script: serde_json::to_string_pretty(&payload).unwrap(),
            synopsis: self.init.synopsis.clone(),
            metadata: Value::Null,
        })
    }
}
