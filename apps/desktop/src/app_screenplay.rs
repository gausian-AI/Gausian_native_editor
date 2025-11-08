use super::{App, ScreenplayTab, WorkspaceView};
use crate::screenplay::legacy::{
    ChatRole, PromptDefaults as LegacyPromptDefaults, ScreenplayAct, ScreenplayCharacter,
    ScreenplayDraft, ScreenplayMessage, ScreenplaySession, ScreenplayShot,
};
use crate::screenplay::{
    self, DraftOptions, LlmScreenplayDraft, ProviderKind, RevisionRequest, RevisionScope,
    ScreenplayEvent, ScreenplayLlmProvider, ScreenplayLlmService, ScreenplayQuestion,
    ScreenplaySessionHandle, SessionInit,
};
use crate::screenplay::{
    GeminiConfig, GeminiProvider, MockConfig, MockLlmProvider, OpenAiConfig, OpenAiProvider,
};
use anyhow::{anyhow, Result as AnyResult};
use chrono::Utc;
use eframe::egui;
use serde::Deserialize;
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

fn format_workflow_input_value(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) => s.clone(),
        JsonValue::Number(n) => n.to_string(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::Array(items) => {
            let preview: Vec<String> = items
                .iter()
                .take(3)
                .map(|val| format_workflow_input_value(val))
                .collect();
            let suffix = if items.len() > 3 { ", …" } else { "" };
            format!("[{}{}]", preview.join(", "), suffix)
        }
        JsonValue::Object(_) => "<object>".to_string(),
        JsonValue::Null => "null".to_string(),
    }
}

impl App {
    pub(super) fn screenplay_toggle_panel(&mut self) {
        self.show_screenplay_panel = !self.show_screenplay_panel;
        self.screenplay_error = None;
        if self.show_screenplay_panel {
            self.screenplay_active_tab = ScreenplayTab::Conversation;
        } else {
            self.screenplay_session_handle = None;
        }
    }

    pub(super) fn screenplay_start_session(&mut self) {
        self.screenplay_error = None;
        self.screenplay_session = None;
        self.screenplay_questions.clear();
        self.screenplay_input.clear();
        self.screenplay_active_tab = ScreenplayTab::Conversation;
        self.screenplay_cancel_requested = false;
        self.screenplay_session_handle = None;
        self.screenplay_busy = true;
        self.screenplay_generate_busy = false;
        let base = self.screenplay_provider_base();
        let workflow_context = self.screenplay_workflow_context();
        let token = if self.screenplay_api_token.trim().is_empty() {
            None
        } else {
            Some(self.screenplay_api_token.clone())
        };
        let api_key = self.screenplay_api_token.clone();
        let model = self.screenplay_model.clone();
        let provider_kind = self.screenplay_provider.clone();
        let project = self.project_id.clone();
        let tx = self.screenplay_event_tx.clone();
        self.screenplay_push_log(format!(
            "Connecting to {} ({})...",
            provider_label(&self.screenplay_provider),
            self.screenplay_model
        ));
        std::thread::spawn(move || {
            let session_init = SessionInit {
                additional_context: workflow_context.clone(),
                ..Default::default()
            };
            let result = match provider_kind {
                ProviderKind::Custom(url) => screenplay::start_session(
                    &base,
                    token.as_deref(),
                    Some(&project),
                    Some(url.as_str()),
                    Some(model.as_str()),
                ),
                other => start_llm_session(&other, &api_key, &model, session_init).and_then(
                    |(mut session, _questions, mut handle)| {
                        tracing::info!(
                            target: "screenplay",
                            "Greeting assistant via {:?} ({})...",
                            other,
                            model
                        );
                        let greet_start = Instant::now();
                        let turn = handle.greet().map_err(|err| {
                            tracing::error!(
                                target: "screenplay",
                                "Greeting failed after {:.2?}: {}",
                                greet_start.elapsed(),
                                err
                            );
                            anyhow!(err.to_string())
                        })?;
                        tracing::info!(
                            target: "screenplay",
                            "Greeting completed in {:.2?}",
                            greet_start.elapsed()
                        );
                        push_message(
                            &mut session,
                            ChatRole::Assistant,
                            turn.assistant_text.clone(),
                        );
                        let questions = dialog_turn_to_questions(&turn);
                        session.question_index = 0;
                        session.completed = true;
                        Ok((session, questions, Some(handle)))
                    },
                ),
            };
            if let Err(err) = tx.send(ScreenplayEvent::SessionStarted(result)) {
                tracing::error!(
                    target: "screenplay",
                    "Failed to send ScreenplayEvent::SessionStarted: {}",
                    err
                );
            }
        });
    }

    pub(super) fn screenplay_send_message(&mut self) {
        if self.screenplay_busy {
            return;
        }
        if self.screenplay_session.is_none() {
            self.screenplay_error = Some("Start a session first".to_string());
            self.screenplay_push_log("Cannot send message: no active session.".to_string());
            return;
        }
        let message = self.screenplay_input.trim().to_string();
        if message.is_empty() {
            self.screenplay_error = Some("Message cannot be empty".to_string());
            self.screenplay_push_log("Message not sent: input was empty.".to_string());
            return;
        }
        let session_snapshot = {
            let session = self.screenplay_session.as_mut().expect("checked above");
            push_message(session, ChatRole::User, message.clone());
            session.completed = false;
            session.clone()
        };
        self.screenplay_error = None;
        self.screenplay_cancel_requested = false;
        self.screenplay_busy = true;
        let base = self.screenplay_provider_base();
        let token = if self.screenplay_api_token.trim().is_empty() {
            None
        } else {
            Some(self.screenplay_api_token.clone())
        };
        let provider_kind = self.screenplay_provider.clone();
        let model = self.screenplay_model.clone();
        let handle = self.screenplay_session_handle.clone();
        let tx = self.screenplay_event_tx.clone();
        std::thread::spawn(move || {
            if let Some(handle_arc) = handle {
                let mut guard = match handle_arc.lock() {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = tx.send(ScreenplayEvent::MessagePosted(Err(anyhow!(
                            err.to_string()
                        ))));
                        return;
                    }
                };
                let turn = match guard.send_user_message(crate::screenplay::TurnInput {
                    text: message.clone(),
                    tags: Vec::new(),
                }) {
                    Ok(turn) => turn,
                    Err(err) => {
                        let _ = tx.send(ScreenplayEvent::MessagePosted(Err(anyhow!(
                            err.to_string()
                        ))));
                        return;
                    }
                };
                let mut updated_session = session_snapshot;
                push_message(
                    &mut updated_session,
                    ChatRole::Assistant,
                    turn.assistant_text.clone(),
                );
                let questions = dialog_turn_to_questions(&turn);
                updated_session.question_index = 0;
                updated_session.completed = true;
                let _ = tx.send(ScreenplayEvent::MessagePosted(Ok((
                    updated_session,
                    questions,
                ))));
                return;
            }

            let session_id = session_snapshot.id.clone();
            let result = screenplay::post_message(
                &base,
                token.as_deref(),
                &session_id,
                &message,
                Some(provider_kind.as_str()),
                Some(model.as_str()),
            );
            match result {
                Ok((session, questions)) => {
                    let _ = tx.send(ScreenplayEvent::MessagePosted(Ok((session, questions))));
                }
                Err(err) => {
                    let _ = tx.send(ScreenplayEvent::MessagePosted(Err(err)));
                }
            }
        });
    }

    pub(super) fn screenplay_generate_screenplay(&mut self) {
        if self.screenplay_generate_busy {
            return;
        }
        let Some(session) = self.screenplay_session.as_ref() else {
            self.screenplay_error = Some("Start a session first".to_string());
            self.screenplay_push_log("Cannot generate draft: no active session.".to_string());
            return;
        };
        if !session
            .messages
            .iter()
            .any(|msg| matches!(msg.role, ChatRole::User))
        {
            self.screenplay_error = Some(
                "Share a few story details with the assistant before generating a draft."
                    .to_string(),
            );
            self.screenplay_push_log(
                "Draft generation requires at least one user message.".to_string(),
            );
            return;
        }
        self.screenplay_error = None;
        self.screenplay_cancel_requested = false;
        self.screenplay_generate_busy = true;
        let base = self.screenplay_provider_base();
        let token = if self.screenplay_api_token.trim().is_empty() {
            None
        } else {
            Some(self.screenplay_api_token.clone())
        };
        let session_id = session.id.clone();
        let provider_kind = self.screenplay_provider.clone();
        let model_key = self.screenplay_model.clone();
        let handle = self.screenplay_session_handle.clone();
        let tx = self.screenplay_event_tx.clone();
        std::thread::spawn(move || {
            if let Some(handle_arc) = handle {
                let draft_result = handle_arc
                    .lock()
                    .map_err(|err| anyhow!(err.to_string()))
                    .and_then(|mut guard| {
                        guard
                            .generate_screenplay(DraftOptions::default())
                            .map_err(|err| anyhow!(err.to_string()))
                    })
                    .map(llm_draft_to_screenplay);
                let _ = tx.send(ScreenplayEvent::ScreenplayGenerated(draft_result));
                return;
            }

            let result = screenplay::generate_screenplay(
                &base,
                token.as_deref(),
                &session_id,
                Some(provider_kind.as_str()),
                Some(model_key.as_str()),
            );
            let _ = tx.send(ScreenplayEvent::ScreenplayGenerated(result));
        });
    }

    pub(super) fn screenplay_request_revision(&mut self) {
        if self.screenplay_busy || self.screenplay_generate_busy {
            return;
        }
        let instructions = self.screenplay_revision_input.trim();
        if instructions.is_empty() {
            self.screenplay_error =
                Some("Enter revision instructions before sending a request.".to_string());
            self.screenplay_push_log("Revision not sent: instructions were empty.".to_string());
            return;
        }
        let Some(session) = self.screenplay_session.as_ref() else {
            self.screenplay_error = Some("Start a session to request revisions.".to_string());
            self.screenplay_push_log("Revision not sent: no active session.".to_string());
            return;
        };
        if session.screenplay.is_none() {
            self.screenplay_error =
                Some("Generate a screenplay before requesting revisions.".to_string());
            self.screenplay_push_log("Revision not sent: screenplay draft is missing.".to_string());
            return;
        }
        let Some(handle_arc) = self.screenplay_session_handle.clone() else {
            self.screenplay_error = Some(
                "This provider does not support live revisions in the current mode.".to_string(),
            );
            self.screenplay_push_log(
                "Revision not sent: provider does not expose revision capabilities.".to_string(),
            );
            return;
        };
        let scope = match &self.screenplay_revision_scope {
            RevisionScope::EntireDraft => RevisionScope::EntireDraft,
            RevisionScope::Scenes(list) => RevisionScope::Scenes(list.clone()),
            RevisionScope::Characters(list) => RevisionScope::Characters(list.clone()),
            RevisionScope::Beats(list) => RevisionScope::Beats(list.clone()),
        };
        let request = RevisionRequest {
            instructions: instructions.to_string(),
            scope,
        };
        self.screenplay_error = None;
        self.screenplay_cancel_requested = false;
        self.screenplay_busy = true;
        self.screenplay_push_log("Requesting screenplay revision…");
        let tx = self.screenplay_event_tx.clone();
        std::thread::spawn(move || {
            let result = handle_arc
                .lock()
                .map_err(|err| anyhow!(err.to_string()))
                .and_then(|mut guard| {
                    guard
                        .revise(request)
                        .map_err(|err| anyhow!(err.to_string()))
                })
                .map(llm_draft_to_screenplay);
            let _ = tx.send(ScreenplayEvent::RevisionApplied(result));
        });
    }

    pub(super) fn screenplay_apply_draft_to_storyboard(&mut self) {
        let draft_to_apply = {
            let Some(session) = self.screenplay_session.as_ref() else {
                self.screenplay_error =
                    Some("Start a session before applying storyboard shots.".to_string());
                self.screenplay_push_log("Cannot apply storyboard: no active session.".to_string());
                return;
            };
            let Some(draft) = session.screenplay.as_ref() else {
                self.screenplay_error =
                    Some("Generate the screenplay before applying storyboard shots.".to_string());
                self.screenplay_push_log(
                    "Cannot apply storyboard: screenplay draft missing.".to_string(),
                );
                return;
            };
            if draft.shots.is_empty() {
                self.screenplay_error =
                    Some("The current screenplay draft has no shots to apply.".to_string());
                self.screenplay_push_log(
                    "Cannot apply storyboard: draft contains no shots.".to_string(),
                );
                return;
            }
            draft.clone()
        };

        let applied = self.storyboard_replace_with_screenplay_draft(&draft_to_apply);
        if let Some(session) = self.screenplay_session.as_mut() {
            session.prompts = None;
        }
        self.screenplay_error = None;
        self.switch_workspace(WorkspaceView::Storyboard);
        self.screenplay_push_log(format!(
            "Applied {} screenplay shot{} to the storyboard workspace.",
            applied,
            if applied == 1 { "" } else { "s" }
        ));
    }

    pub(super) fn screenplay_handle_event(&mut self, event: ScreenplayEvent) {
        tracing::info!(target: "screenplay", "screenplay_handle_event {:?}", event);
        if self.screenplay_cancel_requested {
            self.screenplay_cancel_requested = false;
            return;
        }
        match event {
            ScreenplayEvent::SessionStarted(result) => {
                self.screenplay_busy = false;
                match result {
                    Ok((session, questions, handle)) => {
                        self.screenplay_session = Some(session);
                        self.screenplay_questions = questions;
                        self.screenplay_error = None;
                        self.screenplay_input.clear();
                        self.screenplay_session_handle = handle.map(|h| Arc::new(Mutex::new(h)));
                        self.screenplay_active_tab = ScreenplayTab::Conversation;
                        self.screenplay_push_log(format!(
                            "Connected to {} ({})",
                            provider_label(&self.screenplay_provider),
                            self.screenplay_model
                        ));
                    }
                    Err(err) => {
                        self.screenplay_error = Some(err.to_string());
                        self.screenplay_session_handle = None;
                        self.screenplay_push_log(format!("Session failed: {}", err));
                    }
                }
            }
            ScreenplayEvent::MessagePosted(result) => {
                self.screenplay_busy = false;
                match result {
                    Ok((session, questions)) => {
                        self.screenplay_session = Some(session);
                        self.screenplay_questions = questions;
                        self.screenplay_error = None;
                        self.screenplay_input.clear();
                    }
                    Err(err) => {
                        self.screenplay_error = Some(err.to_string());
                        self.screenplay_push_log(format!("Message failed: {}", err));
                    }
                }
            }
            ScreenplayEvent::ScreenplayGenerated(result) => {
                self.screenplay_generate_busy = false;
                match result {
                    Ok(draft) => {
                        if let Some(session) = self.screenplay_session.as_mut() {
                            session.screenplay = Some(draft.clone());
                            session.prompts = None;
                        } else {
                            let session = screenplay::ScreenplaySession {
                                id: String::from("local"),
                                project_id: None,
                                created_at_ms: None,
                                updated_at_ms: None,
                                completed: true,
                                question_index: 0,
                                answers: Vec::new(),
                                messages: Vec::new(),
                                screenplay: Some(draft),
                                prompts: None,
                            };
                            self.screenplay_session = Some(session);
                        }
                        self.screenplay_error = None;
                        self.screenplay_active_tab = ScreenplayTab::Draft;
                        self.screenplay_revision_input.clear();
                    }
                    Err(err) => {
                        self.screenplay_error = Some(err.to_string());
                        self.screenplay_push_log(format!("Draft generation failed: {}", err));
                    }
                }
            }
            ScreenplayEvent::RevisionApplied(result) => {
                self.screenplay_busy = false;
                match result {
                    Ok(draft) => {
                        if let Some(session) = self.screenplay_session.as_mut() {
                            session.screenplay = Some(draft.clone());
                            session.prompts = None;
                        }
                        self.screenplay_error = None;
                        self.screenplay_revision_input.clear();
                        self.screenplay_push_log("Revision applied to screenplay draft.");
                    }
                    Err(err) => {
                        self.screenplay_error = Some(err.to_string());
                        self.screenplay_push_log(format!("Revision failed: {}", err));
                    }
                }
            }
        }
    }

    fn screenplay_provider_base(&self) -> String {
        match &self.screenplay_provider {
            ProviderKind::OpenAi => "https://api.openai.com/v1".to_string(),
            ProviderKind::Gemini => "https://generativelanguage.googleapis.com".to_string(),
            ProviderKind::Mock => "http://localhost:3001/api/screenplay".to_string(),
            ProviderKind::Custom(url) => url.clone(),
        }
    }

    fn ensure_screenplay_model(&mut self) {
        let options = provider_model_options(&self.screenplay_provider);
        if let Some(first) = options.first() {
            if !options.iter().any(|model| *model == self.screenplay_model) {
                self.screenplay_model = (*first).to_string();
            }
        }
    }

    fn screenplay_push_log(&mut self, entry: impl Into<String>) {
        let message = entry.into();
        if message.trim().is_empty() {
            return;
        }
        if self.screenplay_logs.len() >= 16 {
            self.screenplay_logs.pop_front();
        }
        self.screenplay_logs.push_back(message);
    }

    pub(super) fn screenplay_pause_current(&mut self) {
        if !(self.screenplay_busy || self.screenplay_generate_busy) {
            return;
        }
        self.screenplay_cancel_requested = true;
        self.screenplay_busy = false;
        self.screenplay_generate_busy = false;
        self.screenplay_error = Some("Paused current request".to_string());
        self.screenplay_push_log("Current request paused.".to_string());
    }
}

pub(super) fn screenplay_window(app: &mut App, ctx: &egui::Context) {
    let mut open = app.show_screenplay_panel;
    egui::Window::new("Screenplay Assistant")
        .open(&mut open)
        .default_size((560.0, 680.0))
        .min_width(420.0)
        .resizable(true)
        .show(ctx, |ui| {
            render_screenplay_header(ui, app);
            render_screenplay_tab_bar(ui, app);
            ui.separator();
            let remaining_height = ui.available_height();
            match app.screenplay_active_tab {
                ScreenplayTab::Conversation => {
                    render_conversation_tab(ui, app, remaining_height.max(320.0));
                }
                ScreenplayTab::Draft => {
                    render_draft_tab(ui, app, remaining_height.max(320.0));
                }
            }
        });
    app.show_screenplay_panel = open;
}

fn render_screenplay_header(ui: &mut egui::Ui, app: &mut App) {
    app.ensure_screenplay_model();
    ui.horizontal(|ui| {
        ui.heading("Screenplay Assistant");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if app.screenplay_busy || app.screenplay_generate_busy {
                if ui.button("Pause").clicked() {
                    app.screenplay_pause_current();
                }
                ui.add_space(6.0);
            }
            let (status_label, status_color) = if let Some(err) = app.screenplay_error.as_ref() {
                (err.as_str(), egui::Color32::from_rgb(200, 70, 70))
            } else if app.screenplay_busy || app.screenplay_generate_busy {
                ("Processing", egui::Color32::from_rgb(215, 150, 70))
            } else if app.screenplay_session.is_some() {
                ("Connected", egui::Color32::from_rgb(70, 150, 90))
            } else {
                ("Idle", egui::Color32::from_rgb(105, 120, 145))
            };
            let badge = egui::Frame::none()
                .fill(status_color)
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::symmetric(10.0, 4.0));
            badge.show(ui, |ui| {
                ui.label(egui::RichText::new(status_label).color(egui::Color32::WHITE));
            });
            ui.add_space(8.0);
            let mut selected_provider = app.screenplay_provider.clone();
            egui::ComboBox::from_id_salt("screenplay_provider_selector")
                .selected_text(provider_label(&app.screenplay_provider))
                .show_ui(ui, |ui| {
                    for (label, kind) in provider_options().iter() {
                        let is_selected = &selected_provider == kind;
                        if ui.selectable_label(is_selected, *label).clicked() && !is_selected {
                            selected_provider = kind.clone();
                        }
                    }
                });
            if selected_provider != app.screenplay_provider {
                app.screenplay_provider = selected_provider;
                app.ensure_screenplay_model();
            }
            let models = provider_model_options(&app.screenplay_provider);
            if !models.is_empty() {
                ui.add_space(6.0);
                let mut selected_model = app.screenplay_model.clone();
                egui::ComboBox::from_id_salt("screenplay_model_selector")
                    .selected_text(&app.screenplay_model)
                    .show_ui(ui, |ui| {
                        for model in models {
                            if ui
                                .selectable_label(selected_model == *model, *model)
                                .clicked()
                            {
                                selected_model = (*model).to_string();
                            }
                        }
                    });
                if selected_model != app.screenplay_model {
                    app.screenplay_model = selected_model;
                }
            }
        });
    });

    ui.add_space(4.0);
    egui::CollapsingHeader::new("Connection Settings")
        .default_open(app.screenplay_session.is_none())
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("API Token");
                ui.add(
                    egui::TextEdit::singleline(&mut app.screenplay_api_token)
                        .password(true)
                        .desired_width(240.0),
                );
            });
        });

    if !app.screenplay_logs.is_empty() {
        ui.add_space(6.0);
        egui::Frame::none()
            .fill(egui::Color32::from_rgba_unmultiplied(34, 46, 66, 140))
            .rounding(egui::Rounding::same(6.0))
            .inner_margin(egui::Margin::symmetric(10.0, 6.0))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    for entry in app.screenplay_logs.iter().rev() {
                        ui.label(
                            egui::RichText::new(entry)
                                .size(13.0)
                                .color(egui::Color32::from_rgb(210, 220, 240)),
                        );
                    }
                });
            });
    }

    if app.screenplay_busy {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Waiting for the assistant…");
        });
        ui.ctx().request_repaint(); // Poll event channel until work completes
    }
    if app.screenplay_generate_busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Generating screenplay draft…");
        });
        ui.ctx().request_repaint(); // Poll event channel until work completes
    }
    ui.add_space(6.0);
}

fn render_screenplay_tab_bar(ui: &mut egui::Ui, app: &mut App) {
    ui.horizontal(|ui| {
        let tabs = [
            (ScreenplayTab::Conversation, "Session"),
            (ScreenplayTab::Draft, "Screenplay Draft"),
        ];
        for (tab, label) in tabs {
            let selected = app.screenplay_active_tab == tab;
            if ui
                .add(egui::SelectableLabel::new(selected, label))
                .clicked()
            {
                app.screenplay_active_tab = tab;
            }
        }
    });
}

fn render_conversation_tab(ui: &mut egui::Ui, app: &mut App, available_height: f32) {
    let session = app.screenplay_session.clone();

    ui.horizontal(|ui| {
        let start_label = if session.is_some() {
            "Restart Session"
        } else {
            "Start Session"
        };
        let start_enabled = !app.screenplay_busy && !app.screenplay_generate_busy;
        if ui
            .add_enabled(start_enabled, egui::Button::new(start_label))
            .clicked()
        {
            app.screenplay_start_session();
            ui.ctx().request_repaint(); // Keep update() running to drain events
        }

        if let Some(session) = session.as_ref() {
            let can_generate = session.completed && !app.screenplay_generate_busy;
            let has_user_message = session
                .messages
                .iter()
                .any(|msg| matches!(msg.role, ChatRole::User));
            let generate_btn = ui.add_enabled(
                can_generate && has_user_message,
                egui::Button::new("Generate Screenplay"),
            );
            if generate_btn.clicked() {
                app.screenplay_generate_screenplay();
                ui.ctx().request_repaint(); // Keep update() running to drain events
            }
            if !has_user_message {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Share a few story details before generating the draft.")
                        .color(egui::Color32::from_rgb(210, 180, 80))
                        .small(),
                );
            }
        }

        if app.screenplay_busy || app.screenplay_generate_busy {
            ui.add_space(8.0);
            if ui.button("Pause").clicked() {
                app.screenplay_pause_current();
            }
        }
    });

    ui.add_space(6.0);

    if let Some(session) = session.as_ref() {
        if let Some(next) = app.screenplay_questions.get(session.question_index) {
            ui.label(
                egui::RichText::new(format!("Next question: {}", next.question))
                    .italics()
                    .color(egui::Color32::from_rgb(130, 170, 250)),
            );
        } else if !session.completed {
            ui.label(egui::RichText::new("Answer the next prompt to continue.").italics());
        }

        if !session.answers.is_empty() {
            ui.add_space(6.0);
            egui::CollapsingHeader::new("Collected Answers")
                .default_open(false)
                .show(ui, |ui| {
                    for answer in &session.answers {
                        ui.label(egui::RichText::new(&answer.label).strong());
                        ui.label(&answer.value);
                        ui.add_space(4.0);
                    }
                });
        }

        ui.add_space(6.0);
        let log_height = (available_height - 160.0).max(160.0);
        egui::ScrollArea::vertical()
            .id_salt("screenplay_chat_log")
            .stick_to_bottom(true)
            .max_height(log_height)
            .show(ui, |ui| {
                for message in &session.messages {
                    let is_user = matches!(message.role, ChatRole::User);
                    let bubble_color = if is_user {
                        egui::Color32::from_rgb(58, 62, 74)
                    } else {
                        egui::Color32::from_rgb(34, 66, 110)
                    };
                    ui.with_layout(
                        if is_user {
                            egui::Layout::right_to_left(egui::Align::TOP)
                        } else {
                            egui::Layout::left_to_right(egui::Align::TOP)
                        },
                        |ui| {
                            let frame = egui::Frame::none()
                                .fill(bubble_color)
                                .rounding(egui::Rounding::same(12.0))
                                .inner_margin(egui::Margin::symmetric(12.0, 8.0));
                            frame.show(ui, |ui| {
                                ui.set_max_width(ui.available_width() * 0.86);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&message.text)
                                            .color(egui::Color32::from_rgb(230, 235, 245)),
                                    )
                                    .wrap(),
                                );
                            });
                        },
                    );
                    ui.add_space(6.0);
                }
            });
    } else {
        ui.label("Start a session to begin the conversation.");
    }

    ui.add_space(8.0);
    render_chat_composer(ui, app);
}

fn render_chat_composer(ui: &mut egui::Ui, app: &mut App) {
    let composer_frame = egui::Frame::group(ui.style())
        .fill(ui.visuals().panel_fill.gamma_multiply(1.05))
        .inner_margin(egui::Margin::symmetric(12.0, 10.0));

    composer_frame.show(ui, |ui| {
        let available = ui.available_width();
        let button_width = 90.0;
        let text_width = (available - button_width - ui.spacing().item_spacing.x).max(160.0);
        let text_height = ui.spacing().interact_size.y * 3.2;

        let mut send_now = false;
        let response = ui.add_sized(
            egui::vec2(text_width, text_height),
            egui::TextEdit::multiline(&mut app.screenplay_input)
                .desired_rows(3)
                .hint_text("Describe the next beat or ask the assistant for new ideas."),
        );

        if response.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift)
        {
            app.screenplay_input = app.screenplay_input.trim_end_matches('\n').to_string();
            send_now = true;
        }

        ui.add_space(8.0);
        let send_button = ui.add_sized(egui::vec2(button_width, 40.0), egui::Button::new("Send"));
        if send_button.clicked() {
            send_now = true;
        }

        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Shift + Enter for a new line • Enter to send")
                .color(egui::Color32::from_rgb(150, 160, 175))
                .small(),
        );

        if send_now && !app.screenplay_busy {
            app.screenplay_send_message();
            ui.ctx().request_repaint(); // Keep update() running to drain events
        }
    });
}

fn render_draft_tab(ui: &mut egui::Ui, app: &mut App, available_height: f32) {
    let draft = match app.screenplay_session.as_ref() {
        Some(session) => match session.screenplay.as_ref() {
            Some(draft) => draft.clone(),
            None => {
                ui.label("Your screenplay draft will appear here once generated.");
                return;
            }
        },
        None => {
            ui.label("Generate a screenplay to view the draft.");
            return;
        }
    };

    ui.horizontal(|ui| {
        let width = ui.available_width();
        let text_width = (width - 160.0).max(180.0);
        let text_response = ui.add_sized(
            egui::vec2(text_width, ui.spacing().interact_size.y * 1.4),
            egui::TextEdit::singleline(&mut app.screenplay_revision_input)
                .hint_text("Request revisions – e.g. brighten Act II pacing"),
        );
        let instructions_filled = !app.screenplay_revision_input.trim().is_empty();
        let provider_ready = app.screenplay_session_handle.is_some();
        let has_draft = app
            .screenplay_session
            .as_ref()
            .and_then(|s| s.screenplay.as_ref())
            .is_some();
        let revision_enabled = instructions_filled
            && provider_ready
            && has_draft
            && !app.screenplay_busy
            && !app.screenplay_generate_busy;
        let button = ui.add_enabled(revision_enabled, egui::Button::new("Request Revision"));
        let enter_commit = text_response.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        if (button.clicked() || enter_commit) && revision_enabled {
            app.screenplay_request_revision();
            ui.ctx().request_repaint();
        }
    });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let apply_enabled =
            !draft.shots.is_empty() && !app.screenplay_busy && !app.screenplay_generate_busy;
        let button = ui.add_enabled(
            apply_enabled,
            egui::Button::new("Apply Shots to Storyboard"),
        );
        if button.clicked() {
            app.screenplay_apply_draft_to_storyboard();
            ui.ctx().request_repaint();
        }
        if draft.shots.is_empty() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Generate shots first to apply them to the storyboard.")
                    .small()
                    .italics(),
            );
        }
    });

    ui.add_space(4.0);
    let outline_height = (available_height - 200.0).max(220.0);
    ui.columns(2, |columns| {
        let left = &mut columns[0];
        left.set_min_width(240.0);
        left.heading("Scenes & Shots");
        egui::ScrollArea::vertical()
            .id_source("screenplay_draft_shots")
            .max_height(outline_height)
            .show(left, |ui| {
                let render_shot_card = |ui: &mut egui::Ui, number: usize, shot: &ScreenplayShot| {
                    ui.group(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{:02}. {}", number, shot.title)).strong(),
                        );
                        if !shot.location.is_empty() {
                            ui.label(format!("Location: {}", shot.location));
                        }
                        if !shot.time_of_day.is_empty() {
                            ui.label(format!("Time: {}", shot.time_of_day));
                        }
                        if !shot.camera.is_empty() {
                            ui.label(format!("Camera: {}", shot.camera));
                        }
                        if !shot.visual_description.is_empty() {
                            ui.add(egui::Label::new(shot.visual_description.clone()).wrap());
                        }
                        if !shot.notes.is_empty() {
                            ui.label(egui::RichText::new("Notes").strong());
                            ui.add(egui::Label::new(shot.notes.clone()).wrap());
                        }
                        if !shot.prompt.is_empty() {
                            ui.label(egui::RichText::new("Prompt").strong());
                            ui.add(egui::Label::new(shot.prompt.clone()).wrap());
                        }
                        if !shot.negative_prompt.is_empty() {
                            ui.label(egui::RichText::new("Negative prompt").strong());
                            ui.add(egui::Label::new(shot.negative_prompt.clone()).wrap());
                        }
                        if shot.workflow_key.is_some()
                            || shot.workflow_id.is_some()
                            || !shot.workflow_inputs.is_empty()
                        {
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new("Workflow").strong());
                            if let Some(key) = shot.workflow_key.as_ref() {
                                if !key.trim().is_empty() {
                                    ui.label(format!("Key: {}", key));
                                }
                            }
                            if let Some(id) = shot.workflow_id {
                                ui.label(format!("ID: {}", id));
                            }
                            if !shot.workflow_inputs.is_empty() {
                                ui.label("Inputs:");
                                for (input_key, value) in &shot.workflow_inputs {
                                    ui.monospace(format!(
                                        "{} = {}",
                                        input_key,
                                        format_workflow_input_value(value)
                                    ));
                                }
                            }
                        }
                        ui.label(
                            egui::RichText::new(format!("Duration: {:.1}s", shot.duration))
                                .color(egui::Color32::from_rgb(160, 180, 210)),
                        );
                    });
                };

                let mut shot_index = 1usize;
                if !draft.acts.is_empty() {
                    for act in &draft.acts {
                        ui.group(|ui| {
                            ui.label(
                                egui::RichText::new(format!("Act {} – {}", act.id, act.title))
                                    .strong(),
                            );
                            if !act.summary.is_empty() {
                                ui.add(egui::Label::new(act.summary.clone()).wrap());
                            }
                            ui.add_space(4.0);
                            for shot in &act.shots {
                                render_shot_card(ui, shot_index, shot);
                                shot_index += 1;
                                ui.add_space(6.0);
                            }
                        });
                        ui.add_space(8.0);
                    }
                } else {
                    for (idx, shot) in draft.shots.iter().enumerate() {
                        render_shot_card(ui, idx + 1, shot);
                        ui.add_space(6.0);
                    }
                }
            });

        let right = &mut columns[1];
        right.heading(&draft.title);
        right.label(format!("Genre: {}", draft.genre));
        if let Some(minutes) = draft.duration_minutes {
            right.label(format!("Duration: {:.1} min", minutes));
        }
        if !draft.logline.is_empty() {
            right.add(
                egui::Label::new(
                    egui::RichText::new(draft.logline.clone())
                        .color(egui::Color32::from_rgb(210, 210, 210)),
                )
                .wrap(),
            );
        }
        if !draft.style_tags.is_empty() {
            right.label(format!("Style tags: {}", draft.style_tags.join(", ")));
        }
        if !draft.visual_style.is_empty() {
            right.add(
                egui::Label::new(
                    egui::RichText::new(draft.visual_style.clone())
                        .color(egui::Color32::from_rgb(200, 210, 230)),
                )
                .wrap(),
            );
        }

        right.add_space(6.0);
        right.heading("Output defaults");
        let defaults = &draft.defaults;
        right.label(format!("Frame rate: {:.1} fps", defaults.frame_rate));
        right.label(format!(
            "Resolution: {} × {}",
            defaults.width, defaults.height
        ));
        if !defaults.negative_prompt.is_empty() {
            right.label("Negative prompt");
            right.add(
                egui::Label::new(
                    egui::RichText::new(defaults.negative_prompt.clone())
                        .color(egui::Color32::from_rgb(200, 200, 220)),
                )
                .wrap(),
            );
        }

        right.add_space(8.0);
        right.heading("Characters");
        egui::ScrollArea::vertical()
            .id_source("screenplay_draft_characters")
            .max_height((outline_height * 0.6).max(150.0))
            .show(right, |ui| {
                for character in &draft.characters {
                    ui.group(|ui| {
                        ui.label(egui::RichText::new(&character.name).strong());
                        ui.label(format!("Role: {}", character.role));
                        if !character.description.is_empty() {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(character.description.clone())
                                        .color(egui::Color32::from_rgb(215, 215, 215)),
                                )
                                .wrap(),
                            );
                        }
                        if !character.visual_design.is_empty() {
                            ui.label(egui::RichText::new("Visual design").strong());
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(character.visual_design.clone())
                                        .color(egui::Color32::from_rgb(200, 210, 230)),
                                )
                                .wrap(),
                            );
                        }
                    });
                    ui.add_space(6.0);
                }
            });
    });
}

fn start_llm_session(
    provider_kind: &ProviderKind,
    api_key: &str,
    model: &str,
    init: SessionInit,
) -> AnyResult<(
    ScreenplaySession,
    Vec<ScreenplayQuestion>,
    ScreenplaySessionHandle,
)> {
    let provider = build_llm_provider(provider_kind, api_key, model)?;
    let handle = ScreenplayLlmService::new(provider)
        .start_session(init)
        .map_err(|err| anyhow!(err.to_string()))?;
    let now = Utc::now().timestamp_millis();
    let session = ScreenplaySession {
        id: handle.session_id().to_string(),
        project_id: None,
        created_at_ms: Some(now),
        updated_at_ms: Some(now),
        completed: true,
        question_index: 0,
        answers: Vec::new(),
        messages: Vec::new(),
        screenplay: None,
        prompts: None,
    };
    Ok((session, Vec::new(), handle))
}

fn build_llm_provider(
    provider_kind: &ProviderKind,
    api_key: &str,
    model: &str,
) -> AnyResult<Arc<dyn ScreenplayLlmProvider>> {
    let provider: Arc<dyn ScreenplayLlmProvider> = match provider_kind {
        ProviderKind::OpenAi => {
            let mut config = OpenAiConfig::default();
            config.api_key = api_key.to_string();
            config.model = model.to_string();
            Arc::new(OpenAiProvider::new(config).map_err(|err| anyhow!(err.to_string()))?)
        }
        ProviderKind::Gemini => {
            let mut config = GeminiConfig::default();
            config.api_key = api_key.to_string();
            config.model = model.to_string();
            Arc::new(GeminiProvider::new(config).map_err(|err| anyhow!(err.to_string()))?)
        }
        ProviderKind::Mock => Arc::new(MockLlmProvider::new(MockConfig::default())),
        ProviderKind::Custom(name) => {
            return Err(anyhow!(
                "Custom provider '{}' is not supported via the built-in LLM adapters.",
                name
            ))
        }
    };
    Ok(provider)
}

fn dialog_turn_to_questions(turn: &crate::screenplay::DialogTurn) -> Vec<ScreenplayQuestion> {
    turn.follow_up_questions
        .iter()
        .enumerate()
        .map(|(idx, prompt)| ScreenplayQuestion {
            id: Uuid::new_v4().to_string(),
            label: format!("Follow-up {}", idx + 1),
            question: prompt.clone(),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct StructuredScreenplay {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    genre: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    synopsis: Option<String>,
    #[serde(default)]
    acts: Vec<StructuredAct>,
}

#[derive(Debug, Deserialize)]
struct StructuredAct {
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    shots: Vec<StructuredShot>,
}

#[derive(Debug, Deserialize)]
struct StructuredShot {
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    visual_description: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    characters: Vec<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    camera: Option<StructuredCamera>,
    #[serde(default)]
    sound: Option<StructuredSound>,
    #[serde(default)]
    workflow: Option<StructuredWorkflow>,
}

#[derive(Debug, Deserialize, Default)]
struct StructuredCamera {
    #[serde(default)]
    movement: Option<String>,
    #[serde(default)]
    angle: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct StructuredSound {
    #[serde(default)]
    music: Option<String>,
    #[serde(default)]
    fx: Option<String>,
    #[serde(default)]
    dialogue: Vec<StructuredDialogueLine>,
}

#[derive(Debug, Deserialize, Default)]
struct StructuredWorkflow {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    inputs: Option<JsonMap<String, JsonValue>>,
}

#[derive(Debug, Deserialize, Default)]
struct StructuredDialogueLine {
    #[serde(default)]
    character: Option<String>,
    #[serde(default)]
    line: Option<String>,
}

fn llm_draft_to_screenplay(draft: LlmScreenplayDraft) -> ScreenplayDraft {
    if let Some(structured) = parse_structured_screenplay(&draft.script) {
        if structured.acts.iter().any(|act| !act.shots.is_empty()) {
            return structured_screenplay_to_draft(structured, &draft);
        }
    }
    legacy_screenplay_from_text(draft)
}

fn parse_structured_screenplay(raw: &str) -> Option<StructuredScreenplay> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(parsed) = serde_json::from_str::<StructuredScreenplay>(trimmed) {
        return Some(parsed);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    let candidate = &trimmed[start..=end];
    serde_json::from_str::<StructuredScreenplay>(candidate).ok()
}

fn structured_screenplay_to_draft(
    structured: StructuredScreenplay,
    draft: &LlmScreenplayDraft,
) -> ScreenplayDraft {
    let defaults = LegacyPromptDefaults {
        negative_prompt: "blurry, low quality".to_string(),
        frame_rate: 24.0,
        width: 832,
        height: 480,
    };
    let (fallback_title, fallback_genre, mut style_tags, fallback_visual_style) =
        extract_draft_metadata(draft);
    if !style_tags
        .iter()
        .any(|tag| tag.eq_ignore_ascii_case("structured-json"))
    {
        style_tags.push("structured-json".to_string());
    }
    style_tags.sort();
    style_tags.dedup();

    let title = clean_optional_string(structured.title).unwrap_or(fallback_title);
    let genre = clean_optional_string(structured.genre).unwrap_or(fallback_genre);

    let synopsis = clean_optional_string(structured.synopsis);
    let logline = synopsis.clone().unwrap_or_else(|| {
        draft
            .synopsis
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| truncate_preview(&draft.script, 180))
    });

    let mut visual_style = if fallback_visual_style.trim().is_empty() {
        "Structured screenplay draft".to_string()
    } else {
        fallback_visual_style
    };

    let mut shot_id_seed = 1u32;
    let mut act_id_seed = 1u32;
    let mut all_shots = Vec::new();
    let mut acts_out = Vec::new();
    let mut character_counts: BTreeMap<String, usize> = BTreeMap::new();

    for act in structured.acts.into_iter() {
        let act_id = act.id.filter(|id| *id > 0).unwrap_or_else(|| {
            let id = act_id_seed;
            act_id_seed += 1;
            id
        });
        act_id_seed = act_id_seed.max(act_id.saturating_add(1));

        let act_title =
            clean_optional_string(act.title).unwrap_or_else(|| format!("Act {}", act_id));
        let act_summary = clean_optional_string(act.summary).unwrap_or_default();

        let mut act_shots = Vec::new();
        for shot in act.shots {
            let shot_id = shot.id.filter(|id| *id > 0).unwrap_or_else(|| {
                let id = shot_id_seed;
                shot_id_seed += 1;
                id
            });
            shot_id_seed = shot_id_seed.max(shot_id.saturating_add(1));

            let title =
                clean_optional_string(shot.title).unwrap_or_else(|| format!("Shot {}", shot_id));
            let visual_description =
                clean_optional_string(shot.visual_description).unwrap_or_else(|| title.clone());
            let location = clean_optional_string(shot.location).unwrap_or_default();

            let duration = shot
                .duration
                .map(|d| if d <= 0.0 { 5.0 } else { d as f32 })
                .unwrap_or(5.0);

            let mut camera_parts = Vec::new();
            if let Some(camera) = shot.camera {
                if let Some(movement) = clean_optional_string(camera.movement) {
                    camera_parts.push(format!("Movement: {}", movement));
                }
                if let Some(angle) = clean_optional_string(camera.angle) {
                    camera_parts.push(format!("Angle: {}", angle));
                }
            }
            let camera = camera_parts.join(" | ");

            let mut characters: Vec<String> = shot
                .characters
                .into_iter()
                .filter_map(|name| clean_optional_string(Some(name)))
                .collect();
            characters.sort();
            characters.dedup();

            for name in &characters {
                *character_counts.entry(name.clone()).or_insert(0) += 1;
            }

            let mut notes = Vec::new();
            if !characters.is_empty() {
                notes.push(format!("Characters: {}", characters.join(", ")));
            }
            if let Some(sound) = shot.sound {
                if let Some(music) = clean_optional_string(sound.music) {
                    notes.push(format!("Music: {}", music));
                }
                if let Some(fx) = clean_optional_string(sound.fx) {
                    notes.push(format!("FX: {}", fx));
                }
                for line in sound.dialogue {
                    if let Some(dialogue_line) = clean_optional_string(line.line) {
                        let speaker = clean_optional_string(line.character)
                            .unwrap_or_else(|| "Unknown".to_string());
                        notes.push(format!("Dialogue – {}: {}", speaker, dialogue_line));
                    }
                }
            }
            let notes_text = if notes.is_empty() {
                String::new()
            } else {
                notes.join("\n")
            };

            let (workflow_key, workflow_id, workflow_inputs) = shot
                .workflow
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
            let shot_entry = ScreenplayShot {
                id: shot_id,
                title,
                duration,
                location,
                time_of_day: String::new(),
                camera,
                action: visual_description.clone(),
                visual_description: visual_description.clone(),
                prompt: visual_description.clone(),
                negative_prompt: defaults.negative_prompt.clone(),
                notes: notes_text,
                workflow_key,
                workflow_id,
                workflow_inputs,
            };
            act_shots.push(shot_entry.clone());
            all_shots.push(shot_entry);
        }

        acts_out.push(ScreenplayAct {
            id: act_id,
            title: act_title,
            summary: act_summary,
            shots: act_shots,
        });
    }

    if !acts_out.is_empty() {
        visual_style = format!("{} ({} act structure)", visual_style, acts_out.len());
    }

    let characters = character_counts
        .into_iter()
        .map(|(name, count)| ScreenplayCharacter {
            name,
            role: format!(
                "Appears in {} shot{}",
                count,
                if count == 1 { "" } else { "s" }
            ),
            description: String::new(),
            visual_design: String::new(),
        })
        .collect();

    let duration_minutes =
        structured
            .duration
            .and_then(|d| if d > 0.0 { Some(d as f32) } else { None });

    ScreenplayDraft {
        title,
        genre,
        logline,
        visual_style,
        style_tags,
        duration_minutes,
        defaults,
        characters,
        shots: all_shots,
        acts: acts_out,
    }
}

fn legacy_screenplay_from_text(draft: LlmScreenplayDraft) -> ScreenplayDraft {
    let defaults = LegacyPromptDefaults {
        negative_prompt: "blurry, low quality".to_string(),
        frame_rate: 24.0,
        width: 832,
        height: 480,
    };
    let mut shots = Vec::new();
    for (idx, block) in draft
        .script
        .split("\n\n")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .enumerate()
    {
        let title = block
            .lines()
            .next()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .unwrap_or("Story Beat");
        let preview = truncate_preview(block, 240);
        shots.push(ScreenplayShot {
            id: (idx + 1) as u32,
            title: title.to_string(),
            duration: 5.0,
            location: String::new(),
            time_of_day: String::new(),
            camera: String::new(),
            action: block.to_string(),
            visual_description: preview.clone(),
            prompt: preview,
            negative_prompt: defaults.negative_prompt.clone(),
            notes: String::new(),
            workflow_key: None,
            workflow_id: None,
            workflow_inputs: JsonMap::new(),
        });
    }
    if shots.is_empty() {
        let preview = truncate_preview(&draft.script, 240);
        shots.push(ScreenplayShot {
            id: 1,
            title: "Opening".to_string(),
            duration: 5.0,
            location: String::new(),
            time_of_day: String::new(),
            camera: String::new(),
            action: preview.clone(),
            visual_description: preview.clone(),
            prompt: preview,
            negative_prompt: defaults.negative_prompt.clone(),
            notes: String::new(),
            workflow_key: None,
            workflow_id: None,
            workflow_inputs: JsonMap::new(),
        });
    }

    let (title, genre, style_tags, visual_style) = extract_draft_metadata(&draft);
    let logline = draft
        .synopsis
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| truncate_preview(&draft.script, 180));

    ScreenplayDraft {
        title,
        genre,
        logline,
        visual_style,
        style_tags,
        duration_minutes: None,
        defaults,
        characters: Vec::new(),
        shots,
        acts: Vec::new(),
    }
}

fn clean_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn extract_draft_metadata(draft: &LlmScreenplayDraft) -> (String, String, Vec<String>, String) {
    if let Some(obj) = draft.metadata.as_object() {
        let title = obj
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Generated Screenplay".to_string());
        let genre = obj
            .get("genre")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Drama".to_string());
        let style_tags = obj
            .get("style_tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| entry.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let source = obj.get("source").and_then(|v| v.as_str()).unwrap_or("LLM");
        let visual_style = format!("Generated via {}", source);
        (title, genre, style_tags, visual_style)
    } else {
        (
            "Generated Screenplay".to_string(),
            "Drama".to_string(),
            Vec::new(),
            "Generated via LLM".to_string(),
        )
    }
}

fn truncate_preview(text: &str, max_len: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }
    let mut preview = trimmed[..max_len].to_string();
    preview.push_str("...");
    preview
}

fn push_message(session: &mut ScreenplaySession, role: ChatRole, text: String) {
    let now = Utc::now().timestamp_millis();
    session.messages.push(ScreenplayMessage {
        id: Uuid::new_v4().to_string(),
        role,
        text,
        created_at_ms: Some(now),
    });
    session.updated_at_ms = Some(now);
}

const OPENAI_MODELS: &[&str] = &["gpt-5", "gpt-4o", "gpt-4o-mini", "o1-mini"];
const GEMINI_MODELS: &[&str] = &["gemini-2.5-flash", "gemini-2.5-pro"];
const MOCK_MODELS: &[&str] = &["mock-screenwriter"];
const EMPTY_MODELS: &[&str] = &[];
const PROVIDER_CHOICES: [(&str, ProviderKind); 3] = [
    ("OpenAI", ProviderKind::OpenAi),
    ("Gemini", ProviderKind::Gemini),
    ("Mock", ProviderKind::Mock),
];

fn provider_model_options(kind: &ProviderKind) -> &'static [&'static str] {
    match kind {
        ProviderKind::OpenAi => OPENAI_MODELS,
        ProviderKind::Gemini => GEMINI_MODELS,
        ProviderKind::Mock => MOCK_MODELS,
        ProviderKind::Custom(_) => EMPTY_MODELS,
    }
}

fn provider_options() -> &'static [(&'static str, ProviderKind)] {
    &PROVIDER_CHOICES
}

fn provider_label(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Gemini => "Gemini",
        ProviderKind::Mock => "Mock",
        ProviderKind::Custom(_) => "Custom",
    }
}

pub(super) fn chat_toolbar(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        if ui.button("New Session").clicked() {
            app.screenplay_start_session();
        }
        ui.add_space(8.0);
        if ui.button("Generate Draft").clicked() {
            app.screenplay_generate_screenplay();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Settings").clicked() {
                app.screenplay_toggle_panel();
            }
        });
    });
}

pub(super) fn chat_workspace(app: &mut App, ctx: &egui::Context) {
    egui::CentralPanel::default().show(ctx, |ui| {
        render_screenplay_header(ui, app);
        render_screenplay_tab_bar(ui, app);
        ui.separator();
        let available_height = ui.available_height();
        match app.screenplay_active_tab {
            ScreenplayTab::Conversation => {
                render_conversation_tab(ui, app, available_height.max(320.0));
            }
            ScreenplayTab::Draft => {
                render_draft_tab(ui, app, available_height.max(320.0));
            }
        }
    });
}
