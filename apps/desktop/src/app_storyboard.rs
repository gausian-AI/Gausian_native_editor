use super::{
    App, ComfyAlertKind, StoryboardInputValue, StoryboardPendingInputRefresh,
    StoryboardWorkflowInputKind, StoryboardWorkflowInputSpec, WorkspaceView,
};
use egui::{load::SizedTexture, CollapsingHeader, ComboBox, Widget};
use uuid::Uuid;

const GEMINI_ASPECT_RATIO_OPTIONS: &[(&str, &str)] = &[
    ("1 : 1 (1024 x 1024)", "1:1"),
    ("2 : 3 (832 x 1248)", "2:3"),
    ("3 : 2 (1248 x 832)", "3:2"),
    ("3 : 4 (864 x 1184)", "3:4"),
    ("4 : 3 (1184 x 864)", "4:3"),
    ("4 : 5 (896 x 1152)", "4:5"),
    ("5 : 4 (1152 x 896)", "5:4"),
    ("9 : 16 (768 x 1344)", "9:16"),
    ("16 : 9 (1344 x 768)", "16:9"),
    ("21 : 9 (1536 x 672)", "21:9"),
];

const RUNWAY_RATIO_OPTIONS: &[(&str, &str)] = &[
    ("1280 : 720", "1280:720"),
    ("720 : 1280", "720:1280"),
    ("1104 : 832", "1104:832"),
    ("832 : 1104", "832:1104"),
    ("960 : 960", "960:960"),
    ("1584 : 672", "1584:672"),
];

fn ensure_text_input_value<'a>(
    entry: &'a mut StoryboardInputValue,
    spec: &StoryboardWorkflowInputSpec,
) -> &'a mut String {
    if let StoryboardInputValue::Text(value) = entry {
        return value;
    }
    let replacement = spec
        .default_value
        .as_ref()
        .and_then(|value| match value {
            StoryboardInputValue::Text(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    *entry = StoryboardInputValue::Text(replacement);
    match entry {
        StoryboardInputValue::Text(value) => value,
        _ => unreachable!("StoryboardInputValue::Text expected after normalization"),
    }
}

pub(super) fn storyboard_toolbar(app: &mut App, ui: &mut egui::Ui) {
    if ui.button("Back to Projects").clicked() {
        let _ = app.save_project_timeline();
        app.mode = super::AppMode::ProjectPicker;
        if let Some(mut host) = app.comfy_webview.take() {
            host.close();
        }
    }
    if ui.button("Jobs").clicked() {
        app.show_jobs = !app.show_jobs;
    }
    ui.separator();
    if ui.button("Go to Timeline").clicked() {
        app.switch_workspace(WorkspaceView::Timeline);
    }
    ui.separator();
    let has_outputs = app
        .storyboard_cards
        .iter()
        .any(|card| !card.reference_path.trim().is_empty());
    if ui
        .add_enabled(has_outputs, egui::Button::new("Transfer to Timeline"))
        .clicked()
    {
        match app.transfer_storyboard_to_timeline() {
            Ok((added, skipped)) => {
                app.switch_workspace(WorkspaceView::Timeline);
                let label = if added == 1 { "shot" } else { "shots" };
                app.push_comfy_alert(
                    format!("Transferred {added} storyboard {label} to the timeline."),
                    ComfyAlertKind::Success,
                    std::time::Duration::from_secs(5),
                );
                if !skipped.is_empty() {
                    let preview: Vec<String> = skipped
                        .iter()
                        .take(3)
                        .map(|(title, _)| title.clone())
                        .collect();
                    let summary = if skipped.len() <= 3 {
                        preview.join(", ")
                    } else if preview.is_empty() {
                        String::new()
                    } else {
                        format!("{}…", preview.join(", "))
                    };
                    let mut message = format!("Skipped {} card(s)", skipped.len());
                    if !summary.is_empty() {
                        message.push_str(": ");
                        message.push_str(&summary);
                    }
                    app.push_comfy_alert(
                        message,
                        ComfyAlertKind::Info,
                        std::time::Duration::from_secs(6),
                    );
                }
            }
            Err(err) => {
                app.push_comfy_alert(
                    err,
                    ComfyAlertKind::Warning,
                    std::time::Duration::from_secs(6),
                );
            }
        }
    }
    ui.separator();
    let batch_label = if app.storyboard_batch_busy {
        "Queueing All…"
    } else {
        "Queue All in ComfyUI"
    };
    let batch_enabled = !app.storyboard_cards.is_empty() && !app.storyboard_batch_busy;
    if ui
        .add_enabled(batch_enabled, egui::Button::new(batch_label))
        .clicked()
    {
        app.storyboard_queue_all_cards();
    }
    ui.separator();
    ui.label(format!("Cards: {}", app.storyboard_cards.len()));
}

pub(super) fn render_storyboard_workflow_input(
    app: &mut App,
    ui: &mut egui::Ui,
    card_idx: usize,
    spec: &StoryboardWorkflowInputSpec,
    input_preview_reloads: &mut Vec<String>,
) {
    ui.add_space(6.0);
    let is_gemini_node = spec.node_class.eq_ignore_ascii_case("GeminiImageNode");
    let is_runway_node = spec
        .node_class
        .eq_ignore_ascii_case("RunwayImageToVideoNodeGen4");
    let key_lower = spec.input_key.to_ascii_lowercase();
    let label_text = if is_gemini_node && key_lower == "prompt" {
        "prompt".to_string()
    } else if is_gemini_node && (key_lower == "aspect_ratio" || key_lower == "aspect-ratio") {
        "Aspect Ratio".to_string()
    } else if is_runway_node && key_lower == "prompt" {
        "Prompt".to_string()
    } else if is_runway_node && key_lower == "ratio" {
        "Ratio".to_string()
    } else {
        spec.label.clone()
    };
    ui.label(egui::RichText::new(label_text).strong());
    if is_runway_node && key_lower == "prompt" {
        let default_value = spec
            .default_value
            .clone()
            .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
        let mut changed = false;
        let mut has_focus = false;
        let mut response_state = None;
        {
            let card = &mut app.storyboard_cards[card_idx];
            let entry = card
                .workflow_inputs
                .entry(spec.map_key.clone())
                .or_insert(default_value);
            let value = ensure_text_input_value(entry, spec);
            let response = ui.add(
                egui::TextEdit::multiline(value)
                    .desired_rows(8)
                    .hint_text("Describe the video you want to generate"),
            );
            response_state = Some(response);
        }
        if let Some(response) = response_state {
            if response.changed() {
                changed = true;
            }
            if response.has_focus() {
                has_focus = true;
            }
        }
        if has_focus || changed {
            app.storyboard_selected = Some(card_idx);
        }
        if let Some(card) = app.storyboard_cards.get_mut(card_idx) {
            card.workflow_input_errors.remove(&spec.map_key);
        }
        return;
    }

    if is_runway_node && key_lower == "ratio" {
        let default_value = spec
            .default_value
            .clone()
            .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
        let mut selection_changed = false;
        {
            let card = &mut app.storyboard_cards[card_idx];
            let entry = card
                .workflow_inputs
                .entry(spec.map_key.clone())
                .or_insert(default_value);
            let value = ensure_text_input_value(entry, spec);
            let selected_label = RUNWAY_RATIO_OPTIONS
                .iter()
                .find(|(_, val)| *val == value.as_str())
                .map(|(label, _)| *label)
                .unwrap_or_else(|| value.as_str());
            egui::ComboBox::from_id_source((card_idx, format!("{}_runway_ratio", spec.map_key)))
                .selected_text(selected_label.to_string())
                .show_ui(ui, |combo| {
                    for (label, val) in RUNWAY_RATIO_OPTIONS {
                        let selected = value.as_str() == *val;
                        if combo.selectable_label(selected, *label).clicked() {
                            if value.as_str() != *val {
                                *value = (*val).to_string();
                                selection_changed = true;
                            }
                        }
                    }
                });
        }
        if selection_changed {
            app.storyboard_selected = Some(card_idx);
        }
        if let Some(card) = app.storyboard_cards.get_mut(card_idx) {
            card.workflow_input_errors.remove(&spec.map_key);
        }
        return;
    }

    match spec.kind {
        StoryboardWorkflowInputKind::Text { multiline } => {
            if is_gemini_node && key_lower == "prompt" {
                let default_value = spec
                    .default_value
                    .clone()
                    .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
                let mut changed = false;
                let mut has_focus = false;
                {
                    let card = &mut app.storyboard_cards[card_idx];
                    let entry = card
                        .workflow_inputs
                        .entry(spec.map_key.clone())
                        .or_insert(default_value);
                    if let StoryboardInputValue::Text(value) = entry {
                        let response = ui.add(
                            egui::TextEdit::multiline(value)
                                .desired_rows(8)
                                .hint_text("Describe the image you want to generate"),
                        );
                        if response.changed() {
                            changed = true;
                        }
                        if response.has_focus() {
                            has_focus = true;
                        }
                    } else {
                        *entry = StoryboardInputValue::default_for_kind(&spec.kind);
                    }
                }
                if has_focus || changed {
                    app.storyboard_selected = Some(card_idx);
                }
                if let Some(card) = app.storyboard_cards.get_mut(card_idx) {
                    card.workflow_input_errors.remove(&spec.map_key);
                }
            } else if is_gemini_node && (key_lower == "aspect_ratio" || key_lower == "aspect-ratio")
            {
                let default_value = spec
                    .default_value
                    .clone()
                    .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
                let mut selection_changed = false;
                {
                    let card = &mut app.storyboard_cards[card_idx];
                    let entry = card
                        .workflow_inputs
                        .entry(spec.map_key.clone())
                        .or_insert(default_value);
                    if let StoryboardInputValue::Text(value) = entry {
                        let selected_label = GEMINI_ASPECT_RATIO_OPTIONS
                            .iter()
                            .find(|(_, val)| *val == value.as_str())
                            .map(|(label, _)| *label)
                            .unwrap_or_else(|| value.as_str());
                        egui::ComboBox::from_id_source((
                            card_idx,
                            format!("{}_aspect_ratio", spec.map_key),
                        ))
                        .selected_text(selected_label.to_string())
                        .show_ui(ui, |combo| {
                            for (label, val) in GEMINI_ASPECT_RATIO_OPTIONS {
                                let selected = value.as_str() == *val;
                                if combo.selectable_label(selected, *label).clicked() {
                                    if value.as_str() != *val {
                                        *value = (*val).to_string();
                                        selection_changed = true;
                                    }
                                }
                            }
                        });
                    } else {
                        *entry = StoryboardInputValue::default_for_kind(&spec.kind);
                    }
                }
                if selection_changed {
                    app.storyboard_selected = Some(card_idx);
                }
                if let Some(card) = app.storyboard_cards.get_mut(card_idx) {
                    card.workflow_input_errors.remove(&spec.map_key);
                }
            } else {
                let mut changed = false;
                let mut has_focus = false;
                let default_value = spec
                    .default_value
                    .clone()
                    .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
                {
                    let card = &mut app.storyboard_cards[card_idx];
                    let entry = card
                        .workflow_inputs
                        .entry(spec.map_key.clone())
                        .or_insert(default_value);
                    if let StoryboardInputValue::Text(value) = entry {
                        let response = if multiline {
                            ui.add(
                                egui::TextEdit::multiline(value)
                                    .desired_rows(6)
                                    .hint_text("Enter text"),
                            )
                        } else {
                            ui.text_edit_singleline(value)
                        };
                        if response.changed() {
                            changed = true;
                        }
                        if response.has_focus() {
                            has_focus = true;
                        }
                    } else {
                        *entry = StoryboardInputValue::default_for_kind(&spec.kind);
                    }
                }
                if has_focus {
                    app.storyboard_selected = Some(card_idx);
                }
                if changed {
                    app.storyboard_selected = Some(card_idx);
                }
                if let Some(card) = app.storyboard_cards.get_mut(card_idx) {
                    card.workflow_input_errors.remove(&spec.map_key);
                }
            }
        }
        StoryboardWorkflowInputKind::File => {
            let mut changed = false;
            let mut has_focus = false;
            let mut browse_requested = false;
            let mut use_reference_applied = false;
            let reference_path = app
                .storyboard_cards
                .get(card_idx)
                .map(|c| c.reference_path.clone())
                .unwrap_or_default();
            let reference_available = !reference_path.trim().is_empty();
            let default_value = spec
                .default_value
                .clone()
                .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
            {
                let card = &mut app.storyboard_cards[card_idx];
                let entry = card
                    .workflow_inputs
                    .entry(spec.map_key.clone())
                    .or_insert(default_value);
                if let StoryboardInputValue::File(path) = entry {
                    let inner = ui.horizontal(|row| {
                        let response = row.text_edit_singleline(path);
                        let browse = row.button("Browse…").clicked();
                        let use_reference = row
                            .add_enabled(reference_available, egui::Button::new("Use Reference"))
                            .clicked();
                        (response, browse, use_reference)
                    });
                    let (response, browse, use_reference) = inner.inner;
                    if response.changed() {
                        changed = true;
                    }
                    if response.has_focus() {
                        has_focus = true;
                    }
                    if browse {
                        browse_requested = true;
                    }
                    if use_reference && reference_available {
                        let trimmed = reference_path.trim();
                        if !trimmed.is_empty() {
                            let new_value = trimmed.to_string();
                            if *path != new_value {
                                *path = new_value;
                                changed = true;
                            }
                            use_reference_applied = true;
                        }
                    }
                } else {
                    *entry = StoryboardInputValue::default_for_kind(&spec.kind);
                }
            }
            if browse_requested {
                if let Some(path) = app
                    .file_dialog()
                    .add_filter(
                        "Media",
                        &[
                            "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "mp4",
                            "mov", "mkv", "avi", "webm",
                        ],
                    )
                    .pick_file()
                {
                    let picked = path.to_string_lossy().to_string();
                    if let Some(card) = app.storyboard_cards.get_mut(card_idx) {
                        card.workflow_inputs.insert(
                            spec.map_key.clone(),
                            StoryboardInputValue::File(picked.clone()),
                        );
                        card.workflow_input_errors.remove(&spec.map_key);
                    }
                    app.storyboard_selected = Some(card_idx);
                    input_preview_reloads.push(spec.map_key.clone());
                    changed = true;
                }
            }
            if has_focus {
                app.storyboard_selected = Some(card_idx);
            }
            if changed {
                if use_reference_applied {
                    let card_id = app.storyboard_cards[card_idx].id;
                    app.storyboard_input_previews
                        .remove(&(card_id, spec.map_key.clone()));
                } else {
                    input_preview_reloads.push(spec.map_key.clone());
                }
                app.storyboard_selected = Some(card_idx);
            }
        }
        StoryboardWorkflowInputKind::Integer => {
            let default_value = spec
                .default_value
                .clone()
                .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
            let card = &mut app.storyboard_cards[card_idx];
            let entry = card
                .workflow_inputs
                .entry(spec.map_key.clone())
                .or_insert(default_value);
            if let StoryboardInputValue::Integer(value) = entry {
                let response = ui.add(egui::DragValue::new(value).speed(1.0));
                if response.changed() {
                    card.workflow_input_errors.remove(&spec.map_key);
                    app.storyboard_selected = Some(card_idx);
                }
            } else {
                *entry = StoryboardInputValue::default_for_kind(&spec.kind);
            }
        }
        StoryboardWorkflowInputKind::Float => {
            let default_value = spec
                .default_value
                .clone()
                .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
            let card = &mut app.storyboard_cards[card_idx];
            let entry = card
                .workflow_inputs
                .entry(spec.map_key.clone())
                .or_insert(default_value);
            if let StoryboardInputValue::Float(value) = entry {
                let response = ui.add(egui::DragValue::new(value).speed(0.1).max_decimals(4));
                if response.changed() {
                    card.workflow_input_errors.remove(&spec.map_key);
                    app.storyboard_selected = Some(card_idx);
                }
            } else {
                *entry = StoryboardInputValue::default_for_kind(&spec.kind);
            }
        }
        StoryboardWorkflowInputKind::Boolean => {
            let default_value = spec
                .default_value
                .clone()
                .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
            let card = &mut app.storyboard_cards[card_idx];
            let entry = card
                .workflow_inputs
                .entry(spec.map_key.clone())
                .or_insert(default_value);
            if let StoryboardInputValue::Boolean(value) = entry {
                let changed = ui.checkbox(value, "Enabled");
                if changed.changed() {
                    card.workflow_input_errors.remove(&spec.map_key);
                    app.storyboard_selected = Some(card_idx);
                }
            } else {
                *entry = StoryboardInputValue::default_for_kind(&spec.kind);
            }
        }
        StoryboardWorkflowInputKind::Array => {
            let default_value = spec
                .default_value
                .clone()
                .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
            let card = &mut app.storyboard_cards[card_idx];
            let entry = card
                .workflow_inputs
                .entry(spec.map_key.clone())
                .or_insert(default_value);
            if let StoryboardInputValue::Array(value) = entry {
                let mut buffer =
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| "[]".to_string());
                let response = ui.add(
                    egui::TextEdit::multiline(&mut buffer)
                        .desired_rows(4)
                        .code_editor(),
                );
                if response.changed() {
                    match serde_json::from_str::<serde_json::Value>(&buffer) {
                        Ok(serde_json::Value::Array(parsed)) => {
                            *value = parsed;
                            card.workflow_input_errors.remove(&spec.map_key);
                            app.storyboard_selected = Some(card_idx);
                        }
                        Ok(_) => {
                            card.workflow_input_errors
                                .insert(spec.map_key.clone(), "Expected JSON array value.".into());
                        }
                        Err(err) => {
                            card.workflow_input_errors.insert(
                                spec.map_key.clone(),
                                format!("Invalid array JSON: {}", err),
                            );
                        }
                    }
                }
            } else {
                *entry = StoryboardInputValue::default_for_kind(&spec.kind);
            }
        }
        StoryboardWorkflowInputKind::Object => {
            let default_value = spec
                .default_value
                .clone()
                .unwrap_or_else(|| StoryboardInputValue::default_for_kind(&spec.kind));
            let card = &mut app.storyboard_cards[card_idx];
            let entry = card
                .workflow_inputs
                .entry(spec.map_key.clone())
                .or_insert(default_value);
            if let StoryboardInputValue::Object(value) = entry {
                let mut buffer =
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string());
                let response = ui.add(
                    egui::TextEdit::multiline(&mut buffer)
                        .desired_rows(4)
                        .code_editor(),
                );
                if response.changed() {
                    match serde_json::from_str::<serde_json::Value>(&buffer) {
                        Ok(serde_json::Value::Object(parsed)) => {
                            *value = parsed;
                            card.workflow_input_errors.remove(&spec.map_key);
                            app.storyboard_selected = Some(card_idx);
                        }
                        Ok(_) => {
                            card.workflow_input_errors
                                .insert(spec.map_key.clone(), "Expected JSON object value.".into());
                        }
                        Err(err) => {
                            card.workflow_input_errors.insert(
                                spec.map_key.clone(),
                                format!("Invalid object JSON: {}", err),
                            );
                        }
                    }
                }
            } else {
                *entry = StoryboardInputValue::default_for_kind(&spec.kind);
            }
        }
        StoryboardWorkflowInputKind::Null => {
            let card = &mut app.storyboard_cards[card_idx];
            card.workflow_inputs
                .entry(spec.map_key.clone())
                .or_insert(StoryboardInputValue::Null);
            card.workflow_input_errors.remove(&spec.map_key);
            ui.weak("Value: null");
        }
    }
    if let Some(card) = app.storyboard_cards.get(card_idx) {
        if let Some(err) = card.workflow_input_errors.get(&spec.map_key) {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
        }
    }
}

pub(super) fn storyboard_workspace(app: &mut App, ctx: &egui::Context) {
    egui::CentralPanel::default().show(ctx, |ui| {
        if !app.storyboard_cards.is_empty() {
            let last_idx = app.storyboard_cards.len() - 1;
            match app.storyboard_selected {
                Some(sel) if sel > last_idx => app.storyboard_selected = Some(last_idx),
                None => app.storyboard_selected = Some(0),
                _ => {}
            }
        }

        ui.heading("Storyboard");
        ui.separator();

        let has_selection = app.storyboard_selected.is_some();

        ui.horizontal(|ui| {
            ui.label(format!("Cards: {}", app.storyboard_cards.len()));
            if ui.button("Add Card").clicked() {
                app.storyboard_add_card();
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Duplicate"))
                .clicked()
            {
                if let Some(idx) = app.storyboard_duplicate_selected() {
                    app.storyboard_load_preview(ctx, idx);
                }
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Delete"))
                .clicked()
            {
                let _ = app.storyboard_remove_selected();
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Move Up"))
                .clicked()
            {
                app.storyboard_move_selected(-1);
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Move Down"))
                .clicked()
            {
                app.storyboard_move_selected(1);
            }
        });

        ui.separator();

        egui::ScrollArea::vertical()
            .id_salt("storyboard_cards_scroll")
            .auto_shrink([false; 2])
            .show(ui, |scroll| {
                if app.storyboard_cards.is_empty() {
                    scroll.weak("Add cards to build your storyboard.");
                } else {
                    let len = app.storyboard_cards.len();
                    for idx in 0..len {
                        let label = {
                            let card = &app.storyboard_cards[idx];
                            format!("{:02}. {}", idx + 1, card.title)
                        };
                        scroll.group(|card_group| {
                            card_group.vertical(|card_ui| {
                                let response = card_ui
                                    .selectable_label(app.storyboard_selected == Some(idx), label);
                                if response.clicked() {
                                    app.storyboard_selected = Some(idx);
                                }
                                card_ui.add_space(6.0);
                                storyboard_card_editor(app, ctx, card_ui, idx);
                            });
                        });
                        scroll.add_space(10.0);
                    }
                }
            });
    });
}

fn storyboard_card_editor(app: &mut App, ctx: &egui::Context, ui: &mut egui::Ui, idx: usize) {
    let load_if_missing = {
        let card = &app.storyboard_cards[idx];
        !card.reference_path.trim().is_empty()
            && app.storyboard_previews.get(&card.id).is_none()
            && card.preview_error.is_none()
    };
    let mut needs_reload = load_if_missing;
    let mut workflow_to_import: Option<std::path::PathBuf> = None;
    let mut queue_card = false;
    let mut workflow_to_delete: Option<Uuid> = None;
    let mut refresh_input_previews = false;
    let mut workflow_specs: Vec<StoryboardWorkflowInputSpec> = Vec::new();
    let mut input_preview_reloads: Vec<String> = Vec::new();
    let mut managed_refresh_keys: Vec<String> = Vec::new();
    let card_id = app.storyboard_cards[idx].id;
    if let Some(pending) = app.take_storyboard_input_refresh(card_id) {
        match pending {
            StoryboardPendingInputRefresh::All => {
                app.storyboard_refresh_all_input_previews(ctx, idx);
            }
            StoryboardPendingInputRefresh::Keys(keys) => {
                app.storyboard_refresh_input_previews_for_keys(ctx, idx, keys);
            }
        }
    }

    fn sync_runway_managed_inputs(
        app: &mut App,
        idx: usize,
        specs: &[StoryboardWorkflowInputSpec],
    ) -> Vec<String> {
        let Some(card) = app.storyboard_cards.get_mut(idx) else {
            return Vec::new();
        };
        let reference = card.reference_path.trim().to_string();
        let mut changed_keys: Vec<String> = Vec::new();
        for spec in specs {
            let key_lower = spec.input_key.to_ascii_lowercase();
            if spec
                .node_class
                .eq_ignore_ascii_case("RunwayImageToVideoNodeGen4")
            {
                if key_lower == "duration" {
                    let desired = StoryboardInputValue::Float(card.duration_seconds as f64);
                    if card.workflow_inputs.get(&spec.map_key) != Some(&desired) {
                        card.workflow_inputs.insert(spec.map_key.clone(), desired);
                        changed_keys.push(spec.map_key.clone());
                    }
                    card.workflow_input_errors.remove(&spec.map_key);
                }
            } else if spec.node_class.eq_ignore_ascii_case("LoadImage")
                && spec.input_key.eq_ignore_ascii_case("image")
            {
                let desired = StoryboardInputValue::File(reference.clone());
                if card.workflow_inputs.get(&spec.map_key) != Some(&desired) {
                    card.workflow_inputs.insert(spec.map_key.clone(), desired);
                    changed_keys.push(spec.map_key.clone());
                }
                card.workflow_input_errors.remove(&spec.map_key);
            }
        }
        changed_keys
    }

    fn render_runway_inline_inputs(
        app: &mut App,
        ui: &mut egui::Ui,
        card_idx: usize,
        specs: &[StoryboardWorkflowInputSpec],
        input_preview_reloads: &mut Vec<String>,
    ) -> std::collections::HashSet<String> {
        let mut rendered = std::collections::HashSet::new();
        for spec in specs {
            if spec
                .node_class
                .eq_ignore_ascii_case("RunwayImageToVideoNodeGen4")
            {
                let key_lower = spec.input_key.to_ascii_lowercase();
                if key_lower == "prompt" || key_lower == "ratio" {
                    render_storyboard_workflow_input(
                        app,
                        ui,
                        card_idx,
                        spec,
                        input_preview_reloads,
                    );
                    rendered.insert(spec.map_key.clone());
                }
            }
        }
        rendered
    }

    ui.columns(2, |columns| {
        columns[0].set_min_width(220.0);
        columns[0].set_max_width(280.0);

        columns[1].vertical(|form_ui| {
            let forced_prefix_for_card = app.storyboard_forced_filename_prefix(card_id);
            if app.take_storyboard_preview_reset(card_id) {
                app.storyboard_previews.remove(&card_id);
                app.storyboard_input_previews
                    .retain(|(cid, _), _| *cid != card_id);
                needs_reload = true;
                refresh_input_previews = true;
            }
            {
                let card = &mut app.storyboard_cards[idx];
                form_ui.label(egui::RichText::new("Title").strong());
                let title_resp = form_ui.text_edit_singleline(&mut card.title);
                if title_resp.has_focus() {
                    app.storyboard_selected = Some(idx);
                }
            }

            form_ui.add_space(8.0);
            {
                let card = &mut app.storyboard_cards[idx];
                form_ui.label(egui::RichText::new("Duration (seconds)").strong());
                let duration_resp = form_ui.add(
                    egui::DragValue::new(&mut card.duration_seconds)
                        .range(0.5..=120.0)
                        .speed(0.1),
                );
                if duration_resp.dragged() || duration_resp.has_focus() {
                    app.storyboard_selected = Some(idx);
                }
            }

            form_ui.add_space(8.0);
            let mut browse_reference = false;
            {
                let card = &mut app.storyboard_cards[idx];
                form_ui.label(egui::RichText::new("Reference Path").strong());
                let path_changed = form_ui
                    .horizontal(|row| {
                        let mut changed = false;
                        let response = row.text_edit_singleline(&mut card.reference_path);
                        if response.changed() {
                            changed = true;
                        }
                        if response.has_focus() {
                            app.storyboard_selected = Some(idx);
                        }
                        if row.button("Browse…").clicked() {
                            browse_reference = true;
                        }
                        if changed {
                            card.preview_error = None;
                        }
                        changed
                    })
                    .inner;
                if path_changed {
                    needs_reload = true;
                }
            }
            if browse_reference {
                if let Some(path) = app.file_dialog().pick_file() {
                    let card = &mut app.storyboard_cards[idx];
                    card.reference_path = path.to_string_lossy().to_string();
                    card.preview_error = None;
                    app.storyboard_selected = Some(idx);
                    needs_reload = true;
                }
            }

            form_ui.add_space(8.0);
            {
                let card = &mut app.storyboard_cards[idx];
                form_ui.label(egui::RichText::new("Description").strong());
                let desc_resp = form_ui.add(
                    egui::TextEdit::multiline(&mut card.description)
                        .desired_rows(8)
                        .hint_text("Describe this shot: framing, motion, mood, dialogue, etc."),
                );
                if desc_resp.has_focus() {
                    app.storyboard_selected = Some(idx);
                }
            }

            form_ui.add_space(8.0);
            let mut open_workflow_dialog = false;
            let previous_workflow = app.storyboard_cards.get(idx).and_then(|card| card.workflow_id);
            let (workflow_changed, new_selection) = {
                let card = &mut app.storyboard_cards[idx];
                form_ui.label(egui::RichText::new("Workflow").strong());
                let mut selected = card.workflow_id;
                let selected_label = selected
                    .and_then(|wid| {
                        app.storyboard_workflows
                            .iter()
                            .find(|p| p.id == wid)
                            .map(|p| p.name.clone())
                    })
                    .unwrap_or_else(|| "Select workflow".to_string());
                ComboBox::from_id_salt(("storyboard_workflow", card.id))
                    .selected_text(selected_label)
                    .show_ui(form_ui, |ui| {
                        for preset in &app.storyboard_workflows {
                            if ui
                                .selectable_label(selected == Some(preset.id), &preset.name)
                                .clicked()
                            {
                                selected = Some(preset.id);
                            }
                        }
                    });
                form_ui.add_space(4.0);
                form_ui.label(egui::RichText::new(card.output_kind.label()).weak());
                let changed = card.workflow_id != selected;
                if changed {
                    card.workflow_id = selected;
                    card.workflow_error = None;
                    card.workflow_input_errors.clear();
                }
                form_ui.horizontal(|row| {
                    let deletable = selected.and_then(|wid| {
                        app.storyboard_workflows
                            .iter()
                            .find(|p| p.id == wid)
                            .map(|preset| (!preset.builtin, wid))
                    });
                    let can_delete = deletable.map(|(flag, _)| flag).unwrap_or(false);
                    let delete_id =
                        deletable.and_then(|(flag, wid)| if flag { Some(wid) } else { None });
                    if row.button("Add Workflow…").clicked() {
                        open_workflow_dialog = true;
                    }
                    if row.button("Send to ComfyUI").clicked() {
                        queue_card = true;
                    }
                    if row
                        .add_enabled(can_delete, egui::Button::new("Delete Workflow"))
                        .clicked()
                    {
                        if let Some(id) = delete_id {
                            workflow_to_delete = Some(id);
                        }
                    }
                });
                if let Some(err) = card.workflow_error.as_ref() {
                    form_ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                } else if let Some(status) = card.workflow_status.as_ref() {
                    form_ui.colored_label(egui::Color32::from_rgb(80, 160, 240), status);
                }
                if let Some(settings) = card.video_settings.as_mut() {
                    form_ui.add_space(12.0);
                    form_ui.separator();
                    form_ui.label(egui::RichText::new("ComfyUI Video Output").strong());
                    form_ui.small("Customize SaveVideo inputs before sending to ComfyUI.");
                    form_ui.add_space(6.0);
                    settings.filename_prefix.clone_from(&forced_prefix_for_card);
                    form_ui.label("Filename prefix");
                    form_ui.monospace(forced_prefix_for_card.as_str());
                    form_ui
                        .small("Prefix is enforced per project/card to keep auto-imports scoped.");
                    form_ui.add_space(6.0);
                    form_ui.label("Video format (e.g. auto, mp4)");
                    let format_resp = form_ui.text_edit_singleline(&mut settings.format);
                    if format_resp.has_focus() {
                        app.storyboard_selected = Some(idx);
                    }
                    form_ui.add_space(6.0);
                    form_ui.label("Video codec (e.g. auto, libx264)");
                    let codec_resp = form_ui.text_edit_singleline(&mut settings.codec);
                    if codec_resp.has_focus() {
                        app.storyboard_selected = Some(idx);
                    }
                }
                (changed, selected)
            };
            let previous_preset = previous_workflow.and_then(|wid| {
                app.storyboard_workflows
                    .iter()
                    .find(|p| p.id == wid)
                    .cloned()
            });
            if workflow_changed {
                let defaults = new_selection.and_then(|wid| app.storyboard_workflow_defaults(wid));
                let preset_clone = new_selection.and_then(|wid| {
                    app.storyboard_workflows
                        .iter()
                        .find(|p| p.id == wid)
                        .cloned()
                });
                refresh_input_previews |= preset_clone.is_some();
                if let Some(card) = app.storyboard_cards.get_mut(idx) {
                    card.video_settings = defaults;
                    card.output_kind = preset_clone
                        .as_ref()
                        .map(|preset| preset.output_kind)
                        .unwrap_or_default();
                    if let Some(new_preset) = preset_clone.as_ref() {
                        App::sync_storyboard_inputs_with_transfer(
                            card,
                            new_preset,
                            previous_preset.as_ref(),
                        );
                    } else {
                        App::sync_storyboard_inputs(card, None);
                    }
                }
            }
            workflow_specs = new_selection
                .and_then(|wid| {
                    app.storyboard_workflow_input_specs(wid)
                        .map(|specs| specs.to_vec())
                })
                .unwrap_or_default();
            let preset_for_card = new_selection.and_then(|wid| {
                app.storyboard_workflows
                    .iter()
                    .find(|p| p.id == wid)
                    .cloned()
            });
            let runway_changes = if !workflow_specs.is_empty() {
                sync_runway_managed_inputs(app, idx, &workflow_specs)
            } else {
                Vec::new()
            };
            if !workflow_changed {
                if let Some(card) = app.storyboard_cards.get_mut(idx) {
                    if let Some(preset_ref) = preset_for_card.as_ref() {
                        if App::sync_storyboard_inputs_with_transfer(
                            card,
                            preset_ref,
                            Some(preset_ref),
                        ) {
                            refresh_input_previews = true;
                        }
                    } else if App::sync_storyboard_inputs(card, None) {
                        refresh_input_previews = true;
                    }
                }
            }
            if !runway_changes.is_empty() {
                managed_refresh_keys.extend(runway_changes);
            }
            if !workflow_specs.is_empty() {
                form_ui.add_space(12.0);
                form_ui.separator();
                form_ui.label(egui::RichText::new("Workflow Inputs").strong());
                let card_id = app.storyboard_cards[idx].id;
                let runway_inline_keys = render_runway_inline_inputs(
                    app,
                    form_ui,
                    idx,
                    &workflow_specs,
                    &mut input_preview_reloads,
                );
                let is_inline_spec = |spec: &StoryboardWorkflowInputSpec| {
                    if spec.node_class.eq_ignore_ascii_case("GeminiImageNode")
                        && matches!(
                            spec.input_key.as_str(),
                            "prompt" | "aspect_ratio" | "aspect-ratio"
                        )
                    {
                        return true;
                    }
                    false
                };
                let should_skip_spec = |spec: &StoryboardWorkflowInputSpec| {
                    if runway_inline_keys.contains(&spec.map_key) {
                        return true;
                    }
                    if spec
                        .node_class
                        .eq_ignore_ascii_case("RunwayImageToVideoNodeGen4")
                    {
                        let key = spec.input_key.to_ascii_lowercase();
                        if matches!(key.as_str(), "duration" | "seed") {
                            return true;
                        }
                    }
                    spec.node_class.eq_ignore_ascii_case("LoadImage")
                        && spec.input_key.eq_ignore_ascii_case("image")
                };
                let mut inline_specs: Vec<&StoryboardWorkflowInputSpec> = Vec::new();
                let mut groups: Vec<(String, String, Vec<&StoryboardWorkflowInputSpec>)> =
                    Vec::new();
                for spec in &workflow_specs {
                    if should_skip_spec(spec) {
                        continue;
                    }
                    if is_inline_spec(spec) {
                        inline_specs.push(spec);
                        continue;
                    }
                    if let Some((_, _, specs)) = groups
                        .iter_mut()
                        .find(|(node_id, _, _)| node_id == &spec.node_id)
                    {
                        specs.push(spec);
                    } else {
                        groups.push((spec.node_id.clone(), spec.group_label.clone(), vec![spec]));
                    }
                }
                let has_inline = !inline_specs.is_empty();
                for spec in &inline_specs {
                    render_storyboard_workflow_input(
                        app,
                        form_ui,
                        idx,
                        *spec,
                        &mut input_preview_reloads,
                    );
                }
                if has_inline && !groups.is_empty() {
                    form_ui.add_space(6.0);
                }
                for (node_id, group_label, specs) in groups {
                    let header_id =
                        form_ui.make_persistent_id(("storyboard_inputs", card_id, node_id.clone()));
                    egui::CollapsingHeader::new(group_label.clone())
                        .id_source(header_id)
                        .show(form_ui, |section_ui| {
                            for spec in specs {
                                render_storyboard_workflow_input(
                                    app,
                                    section_ui,
                                    idx,
                                    spec,
                                    &mut input_preview_reloads,
                                );
                            }
                        });
                }
            }
            if open_workflow_dialog {
                if let Some(path) = app
                    .file_dialog()
                    .add_filter("Workflow", &["json"])
                    .pick_file()
                {
                    workflow_to_import = Some(path);
                }
            }
        });

        if needs_reload {
            app.storyboard_load_preview(ctx, idx);
        }

        let selected_workflow = app
            .storyboard_cards
            .get(idx)
            .and_then(|card| card.workflow_id);
        if let Some(sel) = selected_workflow {
            let defaults = app.storyboard_workflow_defaults(sel);
            if let Some(card) = app.storyboard_cards.get_mut(idx) {
                if card.video_settings.is_none() {
                    card.video_settings = defaults;
                }
            }
        } else if let Some(card) = app.storyboard_cards.get_mut(idx) {
            card.video_settings = None;
        }

        let (card_id, preview_error, has_path) = {
            let card = &app.storyboard_cards[idx];
            (
                card.id,
                card.preview_error.clone(),
                !card.reference_path.trim().is_empty(),
            )
        };

        columns[0].vertical(|preview_ui| {
            preview_ui.label(egui::RichText::new("Preview").strong());
            preview_ui.add_space(4.0);
            let max_width = preview_ui.available_width().min(540.0).max(1.0);
            if let Some(tex) = app.storyboard_previews.get(&card_id) {
                let size = tex.size();
                if size[0] > 0 && size[1] > 0 {
                    let scale = (max_width / size[0] as f32).min(1.0);
                    let desired = egui::vec2(size[0] as f32 * scale, size[1] as f32 * scale);
                    let sized = SizedTexture::from_handle(tex);
                    preview_ui.add(egui::Image::from_texture(sized).fit_to_exact_size(desired));
                } else {
                    preview_ui.weak("Preview unavailable.");
                }
            } else if let Some(err) = preview_error {
                preview_ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
            } else if has_path {
                preview_ui.weak("Generating preview...");
            } else {
                preview_ui.weak("Choose an image or video to see a preview.");
            }
            if let Some(card) = app.storyboard_cards.get(idx) {
                let file_specs: Vec<_> = workflow_specs
                    .iter()
                    .filter(|spec| {
                        if spec
                            .node_class
                            .eq_ignore_ascii_case("RunwayImageToVideoNodeGen4")
                        {
                            let key = spec.input_key.to_ascii_lowercase();
                            if key == "prompt" || key == "ratio" {
                                return false;
                            }
                        }
                        matches!(spec.kind, StoryboardWorkflowInputKind::File)
                            && !(spec.node_class.eq_ignore_ascii_case("LoadImage")
                                && spec.input_key.eq_ignore_ascii_case("image"))
                    })
                    .collect();
                if !file_specs.is_empty() {
                    preview_ui.add_space(12.0);
                    preview_ui.separator();
                    preview_ui.label(egui::RichText::new("Workflow References").strong());
                    for spec in file_specs {
                        let heading =
                            if spec.group_label.is_empty() || spec.group_label == spec.label {
                                spec.label.clone()
                            } else {
                                format!("{} · {}", spec.group_label, spec.label)
                            };
                        preview_ui.add_space(6.0);
                        preview_ui.label(egui::RichText::new(heading).strong());
                        let current_path = card
                            .workflow_inputs
                            .get(&spec.map_key)
                            .and_then(|value| match value {
                                StoryboardInputValue::File(path) => Some(path.trim().to_string()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        if current_path.is_empty() {
                            preview_ui.weak("No file selected.");
                        } else {
                            preview_ui.small(current_path.clone());
                        }
                        if let Some(tex) = app
                            .storyboard_input_previews
                            .get(&(card.id, spec.map_key.clone()))
                        {
                            let size = tex.size();
                            if size[0] > 0 && size[1] > 0 {
                                let max_width = preview_ui.available_width().min(540.0).max(1.0);
                                let scale = (max_width / size[0] as f32).min(1.0);
                                let desired =
                                    egui::vec2(size[0] as f32 * scale, size[1] as f32 * scale);
                                let sized = SizedTexture::from_handle(tex);
                                preview_ui.add(
                                    egui::Image::from_texture(sized).fit_to_exact_size(desired),
                                );
                            } else {
                                preview_ui.weak("Preview unavailable.");
                            }
                        } else if let Some(err) = card.workflow_input_errors.get(&spec.map_key) {
                            preview_ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                        } else if !current_path.is_empty() {
                            preview_ui.weak("Generating preview...");
                        }
                    }
                }
            }
        });
    });

    if let Some(delete_id) = workflow_to_delete {
        let result = app.storyboard_remove_workflow(delete_id);
        if let Some(card) = app.storyboard_cards.get_mut(idx) {
            match result {
                Ok(()) => {
                    card.workflow_error = None;
                    refresh_input_previews = true;
                }
                Err(err) => {
                    card.workflow_error = Some(err);
                }
            }
        }
    }

    if refresh_input_previews {
        app.schedule_storyboard_input_refresh(card_id, None);
    } else if !managed_refresh_keys.is_empty() {
        managed_refresh_keys.sort();
        managed_refresh_keys.dedup();
        app.schedule_storyboard_input_refresh(card_id, Some(managed_refresh_keys));
    }
    if !input_preview_reloads.is_empty() {
        input_preview_reloads.sort();
        input_preview_reloads.dedup();
        for key in input_preview_reloads {
            app.storyboard_load_input_preview(ctx, idx, &key);
        }
    }

    if let Some(path) = workflow_to_import {
        match app.storyboard_import_workflow(&path) {
            Ok(id) => {
                let defaults = app.storyboard_workflow_defaults(id);
                let previous_workflow =
                    app.storyboard_cards.get(idx).and_then(|card| card.workflow_id);
                let previous_preset = previous_workflow.and_then(|wid| {
                    app.storyboard_workflows
                        .iter()
                        .find(|p| p.id == wid)
                        .cloned()
                });
                let new_preset = app
                    .storyboard_workflows
                    .iter()
                    .find(|p| p.id == id)
                    .cloned();
                if let Some(card) = app.storyboard_cards.get_mut(idx) {
                    card.workflow_error = None;
                    card.workflow_input_errors.clear();
                    card.workflow_id = Some(id);
                    card.video_settings = defaults;
                    card.output_kind = new_preset
                        .as_ref()
                        .map(|preset| preset.output_kind)
                        .unwrap_or_default();
                    if let Some(preset) = new_preset.as_ref() {
                        App::sync_storyboard_inputs_with_transfer(
                            card,
                            preset,
                            previous_preset.as_ref(),
                        );
                    } else {
                        App::sync_storyboard_inputs(card, None);
                    }
                }
                refresh_input_previews = true;
                app.schedule_storyboard_input_refresh(card_id, None);
            }
            Err(err) => {
                if let Some(card) = app.storyboard_cards.get_mut(idx) {
                    card.workflow_error = Some(err);
                }
            }
        }
    }

    if queue_card {
        let result = app.storyboard_send_to_comfy(idx);
        if let Some(card) = app.storyboard_cards.get_mut(idx) {
            match result {
                Ok(_) => card.workflow_error = None,
                Err(err) => card.workflow_error = Some(err),
            }
        }
    }
}
