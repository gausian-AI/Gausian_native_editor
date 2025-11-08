use super::{App, AutoProxySetting, ComfyAlertKind, ViewerScale, WorkspaceView};
use crate::playback_selector::ProxyMode;
use crate::proxy_queue::{ProxyReason, ProxyStatus};
use chrono::{Local, TimeZone};
use egui::{ComboBox, RichText, ScrollArea};
use project::AssetRow;
use std::path::Path;

const EMBED_WEBVIEW_SUPPORTED: bool = cfg!(all(target_os = "macos", feature = "embed-webview"));

pub(super) fn top_toolbar(app: &mut App, ctx: &egui::Context, _frame: &mut eframe::Frame) {
    if !matches!(app.mode, super::AppMode::Editor) {
        return;
    }
    if !matches!(
        app.engine.state,
        super::decode::PlayState::Scrubbing | super::decode::PlayState::Seeking
    ) {
        app.engine.state = if app.playback_clock.playing {
            super::decode::PlayState::Playing
        } else {
            super::decode::PlayState::Paused
        };
    }
    egui::TopBottomPanel::top("top").show(ctx, |ui| {
        app.prune_comfy_alerts();
        let mut dismiss_alert = false;
        if let Some((message, kind)) = app
            .comfy_alerts
            .front()
            .map(|alert| (alert.message.clone(), alert.kind))
        {
            let (bg, fg) = match kind {
                ComfyAlertKind::Info => (
                    egui::Color32::from_rgb(32, 70, 130),
                    egui::Color32::from_rgb(220, 235, 255),
                ),
                ComfyAlertKind::Success => (
                    egui::Color32::from_rgb(24, 90, 48),
                    egui::Color32::from_rgb(220, 246, 224),
                ),
                ComfyAlertKind::Warning => (
                    egui::Color32::from_rgb(120, 32, 32),
                    egui::Color32::from_rgb(255, 228, 228),
                ),
            };
            egui::Frame::none()
                .fill(bg)
                .stroke(egui::Stroke::new(1.0, fg))
                .rounding(egui::Rounding::same(6.0))
                .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(message).color(fg).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Dismiss").clicked() {
                                dismiss_alert = true;
                            }
                        });
                    });
                });
        }
        if dismiss_alert {
            app.comfy_alerts.pop_front();
        }
        let job_summary = app.job_status_summary();
        ui.horizontal(|ui| {
            ui.label("Workspace:");
            let mut workspace = app.workspace_view;
            ui.selectable_value(&mut workspace, WorkspaceView::Timeline, "Timeline");
            ui.selectable_value(&mut workspace, WorkspaceView::Chat, "Screenplay");
            ui.selectable_value(&mut workspace, WorkspaceView::Storyboard, "Storyboard");
            if workspace != app.workspace_view {
                app.switch_workspace(workspace);
            }
            ui.separator();
            match app.workspace_view {
                WorkspaceView::Timeline => timeline_toolbar(app, ui),
                WorkspaceView::Chat => super::app_screenplay::chat_toolbar(app, ui),
                WorkspaceView::Storyboard => super::app_storyboard::storyboard_toolbar(app, ui),
            }
            let job_summary = job_summary.clone();
            let right_width = ui.available_width().max(0.0);
            if right_width > 0.0 {
                ui.allocate_ui_with_layout(
                    egui::vec2(right_width, 0.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let queued_text = format!("Jobs: {}", job_summary.queued);
                        let label = egui::Label::new(RichText::new(queued_text).strong())
                            .sense(egui::Sense::click());
                        let response = ui.add(label).on_hover_text(format!(
                            "Pending: {}\nRunning: {}",
                            job_summary.pending, job_summary.running
                        ));
                        if response.clicked() {
                            app.show_jobs = true;
                        }

                        if let Some(progress) = job_summary.progress {
                            let progress_value = progress.value.clamp(0.0, 1.0);
                            let progress_text = progress.label.clone();
                            ui.add(
                                egui::ProgressBar::new(progress_value)
                                    .desired_width(140.0)
                                    .show_percentage()
                                    .text(progress_text),
                            );
                        } else if job_summary.running > 0 {
                            ui.label(format!("Running: {}", job_summary.running));
                        } else if job_summary.pending > 0 {
                            ui.label(format!("Pending: {}", job_summary.pending));
                        }
                    },
                );
            }
        });
    });
}

fn timeline_toolbar(app: &mut App, ui: &mut egui::Ui) {
    ui.label("Import path:");
    ui.text_edit_singleline(&mut app.import_path);
    if ui.button("Add").clicked() {
        app.import_from_path();
    }
    if ui.button("Export...").clicked() {
        app.export_sequence();
    }
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
    if ui.button("Settings").clicked() {
        app.show_settings = !app.show_settings;
    }
    ui.separator();
    if ui
        .button(if app.engine.state == super::decode::PlayState::Playing {
            "Pause (Space)"
        } else {
            "Play (Space)"
        })
        .clicked()
    {
        let seq_fps = (app.seq.fps.num.max(1) as f64) / (app.seq.fps.den.max(1) as f64);
        let current_sec = (app.playhead as f64) / seq_fps;
        if app.engine.state == super::decode::PlayState::Playing {
            app.playback_clock.pause(current_sec);
            app.engine.state = super::decode::PlayState::Paused;
            if let Some(engine) = &app.audio_out {
                engine.pause(current_sec);
            }
        } else {
            app.playback_clock.play(current_sec);
            app.engine.state = super::decode::PlayState::Playing;
            if let Ok(clips) = app.build_audio_clips() {
                if let Some(engine) = &app.audio_out {
                    engine.start(current_sec, clips);
                }
            }
        }
    }
}

pub(super) fn preview_settings_window(app: &mut App, ctx: &egui::Context) {
    let mut persist_settings = false;
    egui::Window::new("Preview Settings")
        .open(&mut app.show_settings)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label("Frame-based tolerances:");
            ui.add(
                egui::Slider::new(
                    &mut app.settings.strict_tolerance_frames,
                    0.5..=6.0,
                )
                .text("Strict pause tolerance (frames)"),
            );
            ui.add(
                egui::Slider::new(
                    &mut app.settings.paused_tolerance_frames,
                    0.5..=6.0,
                )
                .text("Paused tolerance (frames)"),
            );
            ui.add(
                egui::Slider::new(
                    &mut app.settings.clear_threshold_frames,
                    0.5..=6.0,
                )
                .text("Clear threshold on seek (frames)"),
            );
            ui.small("Higher tolerance = more off-target frames accepted. Higher clear threshold = fewer blanks on small nudges.");

            ui.separator();
            ui.label("Media playback mode:");
            let mut user_mode = app.proxy_mode_user;
            ComboBox::from_id_source("proxy_mode_combo")
                .selected_text(user_mode.display_name())
                .show_ui(ui, |ui| {
                    for mode in [
                        ProxyMode::OriginalOptimized,
                        ProxyMode::ProxyPreferred,
                        ProxyMode::ProxyOnly,
                    ] {
                        ui.selectable_value(&mut user_mode, mode, mode.display_name());
                    }
                });
            if user_mode != app.proxy_mode_user {
                app.proxy_mode_user = user_mode;
                app.proxy_mode_override = None;
                persist_settings = true;
            }
            if let Some(override_mode) = app.proxy_mode_override {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!(
                        "Auto override active ({}).",
                        override_mode.display_name()
                    ))
                    .italics());
                    if ui.small_button("Clear override").clicked() {
                        app.proxy_mode_override = None;
                    }
                });
            }

            ui.separator();
            ui.label("Auto proxies:");
            let mut auto_pref = app.auto_proxy_setting;
            ComboBox::from_id_source("auto_proxy_combo")
                .selected_text(auto_pref.display_name())
                .show_ui(ui, |ui| {
                    for pref in [
                        AutoProxySetting::Off,
                        AutoProxySetting::LargeOnly,
                        AutoProxySetting::All,
                    ] {
                        ui.selectable_value(&mut auto_pref, pref, pref.display_name());
                    }
                });
            if auto_pref != app.auto_proxy_setting {
                app.auto_proxy_setting = auto_pref;
                persist_settings = true;
            }

            ui.separator();
            ui.label(format!("Viewer scale: {}", app.viewer_scale.label()));
            if app.viewer_scale != ViewerScale::Full {
                if ui.small_button("Reset viewer scale").clicked() {
                    app.viewer_scale = ViewerScale::Full;
                    app.playback_lag_frames = 0;
                    app.playback_stable_frames = 0;
                }
            }
        });
    if persist_settings {
        if let Err(err) = app.persist_proxy_settings() {
            eprintln!("Failed to persist proxy settings: {err}");
        }
    }
}

pub(super) fn show_project_picker_if_needed(app: &mut App, ctx: &egui::Context) -> bool {
    if matches!(app.mode, super::AppMode::ProjectPicker) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Select a Project");
            ui.separator();
            let projects = app.db.list_projects().unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                let card_w = 220.0;
                let _card_h = 170.0;
                for p in &projects {
                    ui.group(|ui| {
                        ui.set_width(card_w);
                        let thumb_h = (card_w / 16.0) * 9.0;
                        let (r, _resp) = ui.allocate_exact_size(
                            egui::vec2(card_w - 16.0, thumb_h),
                            egui::Sense::hover(),
                        );
                        let tex_key = format!("project:{}", p.id);
                        let mut drew_thumb = false;
                        if !app.asset_thumb_textures.contains_key(&tex_key) {
                            if let Ok(mut assets) = app.db.list_assets(&p.id) {
                                assets.reverse();
                                for a in &assets {
                                    let thumb_path = project::app_data_dir()
                                        .join("cache")
                                        .join("thumbnails")
                                        .join(format!("{}-thumb.jpg", a.id));
                                    if thumb_path.exists() {
                                        if let Ok(img) = image::open(&thumb_path) {
                                            let rgba = img.to_rgba8();
                                            let (w, h) = rgba.dimensions();
                                            let color = egui::ColorImage::from_rgba_unmultiplied(
                                                [w as usize, h as usize],
                                                &rgba.into_raw(),
                                            );
                                            let tex = ui.ctx().load_texture(
                                                format!("project_thumb_{}", p.id),
                                                color,
                                                egui::TextureOptions::LINEAR,
                                            );
                                            app.asset_thumb_textures.insert(tex_key.clone(), tex);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(tex) = app.asset_thumb_textures.get(&tex_key) {
                            let tw = tex.size()[0] as f32;
                            let th = tex.size()[1] as f32;
                            let rw = r.width();
                            let rh = r.height();
                            let scale = (rw / tw).min(rh / th);
                            let dw = (tw * scale).max(1.0);
                            let dh = (th * scale).max(1.0);
                            let img_rect =
                                egui::Rect::from_center_size(r.center(), egui::vec2(dw, dh));
                            let uv = egui::Rect::from_min_max(
                                egui::pos2(0.0, 0.0),
                                egui::pos2(1.0, 1.0),
                            );
                            ui.painter().image(
                                app.asset_thumb_textures.get(&tex_key).unwrap().id(),
                                img_rect,
                                uv,
                                egui::Color32::WHITE,
                            );
                            ui.painter().rect_stroke(
                                r,
                                6.0,
                                egui::Stroke::new(1.0, egui::Color32::from_gray(70)),
                            );
                            drew_thumb = true;
                        }
                        if !drew_thumb {
                            let c = egui::Color32::from_rgb(70, 80, 95);
                            ui.painter().rect_filled(r.shrink(2.0), 6.0, c);
                            let initial = p.name.chars().next().unwrap_or('?');
                            ui.painter().text(
                                r.center(),
                                egui::Align2::CENTER_CENTER,
                                initial,
                                egui::FontId::proportional(28.0),
                                egui::Color32::WHITE,
                            );
                        }
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(&p.name).strong());
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Open").clicked() {
                                app.project_id = p.id.clone();
                                app.selected = None;
                                app.drag = None;
                                app.load_project_timeline();
                                app.mode = super::AppMode::Editor;
                            }

                            let delete_btn = egui::Button::new(
                                egui::RichText::new("Delete")
                                    .color(egui::Color32::from_rgb(210, 64, 64)),
                            );
                            if ui
                                .add(delete_btn)
                                .on_hover_text("Delete project and remove associated proxies")
                                .clicked()
                            {
                                if let Err(err) = app.delete_project_and_cleanup(&p.id) {
                                    tracing::error!(
                                        project_id = %p.id,
                                        error = %err,
                                        "failed to delete project"
                                    );
                                } else {
                                    ui.ctx().request_repaint();
                                }
                            }
                        });
                    });
                    ui.add_space(8.0);
                }
            });
            ui.separator();
            ui.heading("Create Project");
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut app.new_project_name);
            });
            ui.small("Base path will be created under app data automatically.");
            if ui
                .add_enabled(
                    !app.new_project_name.trim().is_empty(),
                    egui::Button::new("Create"),
                )
                .clicked()
            {
                let id = uuid::Uuid::new_v4().to_string();
                let safe_name = app.new_project_name.trim();
                let mut base = project::app_data_dir().join("projects").join(safe_name);
                let mut i = 1;
                while base.exists() {
                    base = project::app_data_dir()
                        .join("projects")
                        .join(format!("{}-{}", safe_name, i));
                    i += 1;
                }
                let _ = std::fs::create_dir_all(&base);
                let _ = app.db.ensure_project(&id, safe_name, Some(&base));
                app.project_id = id;
                app.new_project_name.clear();
                app.load_project_timeline();
                app.mode = super::AppMode::Editor;
            }
        });
        return true;
    }
    false
}

pub(super) fn assets_panel(app: &mut App, ctx: &egui::Context) {
    egui::SidePanel::left("assets")
        .default_width(200.0)
        .resizable(true)
        .min_width(110.0)
        .max_width(1600.0)
        .show(ctx, |ui| {
            app.poll_jobs();
            ui.heading("Assets");
            ui.horizontal(|ui| {
                if ui.button("Import...").clicked() {
                    if let Some(files) = app.file_dialog().pick_files() {
                        let _ = app.import_files(&files);
                    }
                }
                if ui.button("Refresh").clicked() {}
                if ui.button("Jobs").clicked() {
                    app.show_jobs = !app.show_jobs;
                }
                if ui.button("ComfyUI").clicked() {
                    app.show_comfy_panel = !app.show_comfy_panel;
                }
            });
            let assets = app.assets();
            proxy_jobs_summary(app, ui, &assets);
            if app.show_comfy_panel {
                comfy_settings_panel(app, ui);
            }
            // Cloud (Modal) submission UI (always visible; independent of local embed)
            cloud_modal_section(app, ui);
            // Embedded ComfyUI panel at top of assets (outside scrolling region)
            comfy_embed_in_assets(app, ui);
            // Remaining scrollable section (native decoder tests, etc.)
            assets_scroll_section(app, ui, &assets);
        });
}

pub(super) fn proxy_jobs_summary(app: &mut App, ui: &mut egui::Ui, assets: &[AssetRow]) {
    let mut active: Vec<(String, ProxyStatus)> = app
        .proxy_status
        .iter()
        .filter_map(|(asset_id, status)| match status {
            ProxyStatus::Pending | ProxyStatus::Running { .. } => {
                Some((asset_id.clone(), status.clone()))
            }
            _ => None,
        })
        .collect();
    if active.is_empty() {
        return;
    }
    active.sort_by(|a, b| a.0.cmp(&b.0));

    ui.add_space(6.0);
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Proxy Jobs").strong());
            ui.label(format!("Active: {}", active.len()));
        });
        ui.add_space(4.0);
        for (asset_id, status) in active {
            let name = assets
                .iter()
                .find(|asset| asset.id == asset_id)
                .map(asset_display_name)
                .unwrap_or_else(|| asset_id.clone());
            ui.label(RichText::new(name).small());
            match status {
                ProxyStatus::Pending => {
                    ui.add(
                        egui::ProgressBar::new(0.0)
                            .desired_width(ui.available_width())
                            .text("Waiting to start…"),
                    );
                }
                ProxyStatus::Running { progress } => {
                    let pct = progress.clamp(0.0, 1.0);
                    ui.add(
                        egui::ProgressBar::new(pct)
                            .desired_width(ui.available_width())
                            .show_percentage()
                            .text(format!("Encoding {:.0}%", pct * 100.0)),
                    );
                }
                _ => {}
            }
            ui.add_space(4.0);
        }
    });
    ui.add_space(6.0);
}

pub(super) fn comfy_settings_panel(app: &mut App, ui: &mut egui::Ui) {
    ui.separator();
    ui.heading("ComfyUI");
    ui.small("Set the ComfyUI repository path (folder containing main.py). Start server locally and open embedded window.");
    if !EMBED_WEBVIEW_SUPPORTED {
        if app.comfy_embed_inside {
            if let Some(mut host) = app.comfy_webview.take() {
                host.close();
            }
            app.comfy_embed_logs.push_back(
                "Embedded view requires macOS build with --features embed-webview".into(),
            );
        } else if let Some(mut host) = app.comfy_webview.take() {
            host.close();
        }
        app.comfy_embed_inside = false;
        app.comfy_embed_in_assets = false;
    }

    ui.horizontal(|ui| {
        let mut embed = app.comfy_embed_inside;
        let embed_resp = ui.add_enabled(
            EMBED_WEBVIEW_SUPPORTED,
            egui::Checkbox::new(&mut embed, "Open inside editor"),
        );
        if embed_resp.changed() {
            app.comfy_embed_inside = embed;
            if !embed {
                if let Some(mut host) = app.comfy_webview.take() {
                    host.close();
                }
                app.comfy_embed_logs
                    .push_back("Embedded view closed".into());
            }
        }
        if !EMBED_WEBVIEW_SUPPORTED {
            ui.small("(embed requires macOS build with feature: embed-webview)");
        }
        if app.comfy_embed_inside {
            if ui.small_button("Reload").clicked() {
                if let Some(h) = app.comfy_webview.as_mut() {
                    h.reload();
                    app.comfy_embed_logs.push_back("Reload requested".into());
                }
            }
            ui.separator();
            let mut ai = app.comfy_auto_import;
            if ui
                .checkbox(&mut ai, "Auto-import outputs")
                .on_hover_text(
                    "Watch ComfyUI output folder and import finished videos into this project",
                )
                .changed()
            {
                app.comfy_auto_import = ai;
            }
        }
    });

    ui.horizontal(|ui| {
        ui.label("Repo Path");
        let resp = ui.text_edit_singleline(&mut app.comfy_repo_input);
        let enter_commit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let save_clicked = ui
            .small_button("Save")
            .on_hover_text("Commit path to settings")
            .clicked();
        if ui.small_button("Browse…").clicked() {
            if let Some(folder) = app.file_dialog().pick_folder() {
                app.comfy_repo_input = folder.to_string_lossy().to_string();
            }
        }
        if enter_commit || save_clicked {
            let s = app.comfy_repo_input.trim();
            if !s.is_empty() {
                let dir = Path::new(s);
                if dir.is_dir() && dir.join("main.py").exists() {
                    app.comfy.config_mut().repo_path = Some(dir.to_path_buf());
                }
            }
        }
    });

    let path_str = app.comfy_repo_input.trim();
    if path_str.is_empty() {
        ui.colored_label(egui::Color32::GRAY, "Path not set");
    } else {
        let dir = Path::new(path_str);
        if !dir.is_dir() {
            ui.colored_label(egui::Color32::RED, "Folder does not exist");
        } else if !dir.join("main.py").exists() {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Selected folder does not contain main.py",
            );
        } else {
            ui.colored_label(egui::Color32::GREEN, "OK");
        }
    }

    ui.horizontal(|ui| {
        ui.label("Python");
        let mut py = app.comfy.config().python_cmd.clone();
        if ui.text_edit_singleline(&mut py).changed() {
            app.comfy.config_mut().python_cmd = py;
        }
    });
    ui.horizontal(|ui| {
        ui.label("Host");
        let mut host = app.comfy.config().host.clone();
        if ui.text_edit_singleline(&mut host).changed() {
            app.comfy.set_host_input(host);
        }
        ui.label("Port");
        let mut p = app.comfy.config().port as i32;
        if ui
            .add(egui::DragValue::new(&mut p).clamp_range(1024..=65535))
            .changed()
        {
            app.comfy.config_mut().port = p.clamp(1024, 65535) as u16;
        }
        let mut https = app.comfy.config().https;
        if ui
            .checkbox(&mut https, "HTTPS")
            .on_hover_text("Use HTTPS/WSS when connecting to remote ComfyUI")
            .changed()
        {
            app.comfy.config_mut().https = https;
        }
        if app.comfy.is_port_open() {
            ui.colored_label(egui::Color32::YELLOW, "Port in use");
        }
    });

    ui.collapsing("Installation", |ui| {
        ui.horizontal(|ui| {
            ui.label("Install Dir");
            let _ = ui.text_edit_singleline(&mut app.comfy_install_dir_input);
            if ui.small_button("Browse…").clicked() {
                if let Some(folder) = app.file_dialog().pick_folder() {
                    app.comfy_install_dir_input = folder.to_string_lossy().to_string();
                }
            }
        });
        let dir = Path::new(app.comfy_install_dir_input.trim());
        if !dir.exists() {
            ui.colored_label(egui::Color32::GRAY, "Dir will be created");
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut app.comfy_install_ffmpeg, "Install FFmpeg (system)")
                .on_hover_text("Best-effort install via your OS package manager (brew/winget/choco/apt/etc.)");
        });
        ui.horizontal(|ui| {
            ui.label("Python for venv");
            ui.text_edit_singleline(&mut app.comfy_venv_python_input)
                .on_hover_text("Optional: interpreter to create .venv (prefer Python 3.11/3.12)");
        });
        ui.collapsing("pip settings", |ui| {
            ui.horizontal(|ui| {
                ui.label("Index URL");
                ui.text_edit_singleline(&mut app.pip_index_url_input)
                    .on_hover_text("e.g., https://pypi.org/simple or a local mirror");
            });
            ui.horizontal(|ui| {
                ui.label("Extra Index URL");
                ui.text_edit_singleline(&mut app.pip_extra_index_url_input)
                    .on_hover_text("additional package index to search");
            });
            ui.horizontal(|ui| {
                ui.label("Trusted hosts");
                ui.text_edit_singleline(&mut app.pip_trusted_hosts_input)
                    .on_hover_text("comma-separated hostnames (e.g., pypi.org,files.pythonhosted.org)");
            });
            ui.horizontal(|ui| {
                ui.label("Proxy URL");
                ui.text_edit_singleline(&mut app.pip_proxy_input)
                    .on_hover_text("e.g., http://user:pass@proxy:port");
            });
            ui.checkbox(&mut app.pip_no_cache, "No cache")
                .on_hover_text("Pass --no-cache-dir to pip");
        });
        ui.horizontal(|ui| {
            ui.label("Torch Backend");
            egui::ComboBox::from_id_source("torch_backend")
                .selected_text(match app.comfy_torch_backend {
                    crate::comfyui::TorchBackend::Auto => "Auto",
                    crate::comfyui::TorchBackend::Cuda => "CUDA",
                    crate::comfyui::TorchBackend::Mps => "MPS",
                    crate::comfyui::TorchBackend::Rocm => "ROCm",
                    crate::comfyui::TorchBackend::Cpu => "CPU",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut app.comfy_torch_backend,
                        crate::comfyui::TorchBackend::Auto,
                        "Auto",
                    );
                    ui.selectable_value(
                        &mut app.comfy_torch_backend,
                        crate::comfyui::TorchBackend::Cuda,
                        "CUDA",
                    );
                    ui.selectable_value(
                        &mut app.comfy_torch_backend,
                        crate::comfyui::TorchBackend::Mps,
                        "MPS",
                    );
                    ui.selectable_value(
                        &mut app.comfy_torch_backend,
                        crate::comfyui::TorchBackend::Rocm,
                        "ROCm",
                    );
                    ui.selectable_value(
                        &mut app.comfy_torch_backend,
                        crate::comfyui::TorchBackend::Cpu,
                        "CPU",
                    );
                });
        });
        ui.checkbox(&mut app.comfy_recreate_venv, "Recreate venv (.venv) using Python for venv")
            .on_hover_text("Deletes and recreates .venv to switch Python versions (useful when upgrading/downgrading)");
        ui.horizontal(|ui| {
            if ui.button("Install / Repair").clicked() {
                let mut plan = crate::comfyui::InstallerPlan::default();
                let s = app.comfy_install_dir_input.trim();
                if !s.is_empty() {
                    plan.install_dir = Some(std::path::PathBuf::from(s));
                }
                plan.torch_backend = app.comfy_torch_backend;
                let p = app.comfy_venv_python_input.trim();
                if !p.is_empty() {
                    plan.python_for_venv = Some(p.to_string());
                }
                plan.recreate_venv = app.comfy_recreate_venv;
                plan.install_ffmpeg = app.comfy_install_ffmpeg;
                let mut trusted: Vec<String> = Vec::new();
                for t in app.pip_trusted_hosts_input.split(',') {
                    let tt = t.trim();
                    if !tt.is_empty() {
                        trusted.push(tt.to_string());
                    }
                }
                plan.pip.index_url = if app.pip_index_url_input.trim().is_empty() {
                    None
                } else {
                    Some(app.pip_index_url_input.trim().to_string())
                };
                plan.pip.extra_index_url = if app.pip_extra_index_url_input.trim().is_empty() {
                    None
                } else {
                    Some(app.pip_extra_index_url_input.trim().to_string())
                };
                plan.pip.trusted_hosts = trusted;
                plan.pip.proxy = if app.pip_proxy_input.trim().is_empty() {
                    None
                } else {
                    Some(app.pip_proxy_input.trim().to_string())
                };
                plan.pip.no_cache = app.pip_no_cache;
                app.comfy.install(plan);
            }
            if ui.button("Validate").clicked() {
                app.comfy.validate_install();
            }
            if ui.button("Use Installed").clicked() {
                app.comfy.use_installed();
                if let Some(p) = app.comfy.config().repo_path.as_ref() {
                    app.comfy_repo_input = p.to_string_lossy().to_string();
                }
            }
            if ui.button("Uninstall").clicked() {
                app.comfy.uninstall();
            }
            if ui.button("Repair Missing Packages").clicked() {
                app.comfy.repair_common_packages();
            }
        });
        ui.small("Installer creates a reusable .venv in the selected directory. It will NOT re-create it on Start.");
    });

    let running = app.comfy.is_running();
    let repo_dir_valid = {
        let s = app.comfy_repo_input.trim();
        !s.is_empty() && Path::new(s).is_dir() && Path::new(s).join("main.py").exists()
    };
    let py_ok = !app.comfy.config().python_cmd.trim().is_empty();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !running && repo_dir_valid && py_ok,
                egui::Button::new("Start ComfyUI"),
            )
            .clicked()
        {
            let s = app.comfy_repo_input.trim();
            if !s.is_empty() {
                app.comfy.config_mut().repo_path = Some(std::path::PathBuf::from(s));
            }
            app.comfy.start();
            if app.comfy_embed_inside && app.comfy_webview.is_none() {
                if let Some(mut host) = crate::embed_webview::create_platform_host() {
                    host.navigate(&app.comfy.url());
                    host.set_visible(true);
                    host.focus();
                    app.comfy_webview = Some(host);
                    app.comfy_embed_logs
                        .push_back("Embedded view created".into());
                } else {
                    app.comfy_embed_inside = false;
                    let reason = if !EMBED_WEBVIEW_SUPPORTED {
                        "feature flag not active; rebuild with --features embed-webview"
                    } else {
                        "no NSWindow contentView found; focus the app window and try again"
                    };
                    app.comfy_embed_logs
                        .push_back(format!("Failed to create embedded view ({})", reason));
                }
            }
        }
        if ui
            .add_enabled(running, egui::Button::new("Open Window"))
            .clicked()
        {
            app.comfy.open_webview_window();
        }
        if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
            app.comfy.stop();
            if let Some(mut host) = app.comfy_webview.take() {
                host.close();
            }
            app.comfy_embed_logs
                .push_back("Embedded view closed".into());
        }
    });

    ui.label(format!("Status: {:?}", app.comfy.last_status));
    if let Some(err) = &app.comfy.last_error {
        ui.colored_label(egui::Color32::RED, err);
    }

    ui.collapsing("Logs", |ui| {
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(220.0)
            .show(ui, |ui| {
                for line in app.comfy.logs(500) {
                    ui.monospace(line);
                }
            });
    });
    ui.collapsing("Embedded View Logs", |ui| {
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(120.0)
            .show(ui, |ui| {
                while app.comfy_embed_logs.len() > 200 {
                    app.comfy_embed_logs.pop_front();
                }
                for line in app.comfy_embed_logs.iter() {
                    ui.monospace(line);
                }
            });
    });
    ui.collapsing("Auto-import Logs", |ui| {
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(120.0)
            .show(ui, |ui| {
                while app.comfy_import_logs.len() > 400 {
                    app.comfy_import_logs.pop_front();
                }
                for line in app.comfy_import_logs.iter() {
                    ui.monospace(line);
                }
            });
    });
    if ui.small_button("Open in Browser").clicked() {
        let _ = webbrowser::open(&app.comfy.url());
    }
}

pub(super) fn cloud_modal_section(app: &mut App, ui: &mut egui::Ui) {
    ui.collapsing("Cloud (Modal)", |ui| {
        ui.checkbox(&mut app.modal_enabled, "Enable");
        ui.horizontal(|ui| {
            ui.label("Target");
            egui::ComboBox::from_id_source("cloud_target")
                .selected_text(match app.cloud_target {
                    super::CloudTarget::Prompt => "ComfyUI /prompt",
                    super::CloudTarget::Workflow => "Workflow (auto-convert)",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut app.cloud_target,
                        super::CloudTarget::Prompt,
                        "ComfyUI /prompt",
                    );
                    ui.selectable_value(
                        &mut app.cloud_target,
                        super::CloudTarget::Workflow,
                        "Workflow (auto-convert)",
                    );
                });
        });
        ui.horizontal(|ui| {
            ui.label("Base URL");
            ui.text_edit_singleline(&mut app.modal_base_url)
                .on_hover_text("e.g., https://api.yourdomain.com");
        });
        ui.horizontal(|ui| {
            ui.label("Relay WS URL");
            ui.text_edit_singleline(&mut app.modal_relay_ws_url)
                .on_hover_text("Optional: wss://relay.yourdomain.com/stream (overrides /events)");
        });
        ui.horizontal(|ui| {
            ui.label("API Key");
            let mut masked = app.modal_api_key.clone();
            if ui.text_edit_singleline(&mut masked).changed() {
                app.modal_api_key = masked;
            }
        });
        ui.label("Payload (JSON)");
        egui::ScrollArea::vertical()
            .id_source("cloud_payload")
            .max_height(240.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(&mut app.modal_payload).desired_rows(12));
            });
        ui.horizontal(|ui| {
            if ui.button("Test Connection").clicked() {
                app.modal_test_connection();
            }
            if ui
                .add_enabled(app.modal_enabled, egui::Button::new("Queue Job"))
                .clicked()
            {
                app.modal_queue_job();
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!(
                "Queue — pending: {}, running: {}",
                app.modal_queue_pending, app.modal_queue_running
            ));
            if ui.small_button("Clear Progress").clicked() {
                app.modal_job_progress.clear();
            }
        });
        egui::ScrollArea::vertical()
            .id_source("cloud_progress")
            .max_height(220.0)
            .show(ui, |ui| {
                let now = std::time::Instant::now();
                let mut prune: Vec<String> = Vec::new();
                for (jid, (_p, _c, _t, ts)) in app.modal_job_progress.iter() {
                    if now.duration_since(*ts).as_secs() > 600 {
                        prune.push(jid.clone());
                    }
                }
                for k in prune {
                    app.modal_job_progress.remove(&k);
                    app.modal_phase_agg.remove(&k);
                }
                for (jid, agg) in app.modal_phase_agg.iter() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong(jid);
                            if let Some(src) = app.modal_job_source.get(jid) {
                                let tag = match src {
                                    crate::CloudUpdateSrc::Ws => "WS",
                                    crate::CloudUpdateSrc::Jobs => "/jobs",
                                    crate::CloudUpdateSrc::Status => "/status",
                                };
                                ui.small(format!("[{}]", tag));
                            }
                        });
                        // Adaptive overall progress
                        let s_frac = if agg.s_tot > 0 {
                            Some((agg.s_cur as f32) / (agg.s_tot as f32).max(1.0))
                        } else {
                            None
                        };
                        let e_frac = if agg.e_tot > 0 {
                            Some((agg.e_cur as f32) / (agg.e_tot as f32).max(1.0))
                        } else {
                            None
                        };
                        let overall = match (s_frac, e_frac) {
                            (Some(fs), Some(fe)) => {
                                if agg.s_cur == 0 && agg.e_cur > 0 {
                                    fe
                                } else if agg.e_cur == 0 && agg.s_cur > 0 {
                                    fs
                                } else {
                                    0.5 * (fs + fe)
                                }
                            }
                            (Some(fs), None) => fs,
                            (None, Some(fe)) => fe,
                            (None, None) => 0.0,
                        };
                        if agg.importing {
                            ui.add(egui::ProgressBar::new(1.0).text("importing"));
                        } else if overall > 0.0 {
                            let p = overall.clamp(0.0, 1.0);
                            ui.add(
                                egui::ProgressBar::new(p)
                                    .text(format!("Overall {:.1}%", p * 100.0)),
                            );
                        } else if let Some((percent, _c, _t, _ts)) = app.modal_job_progress.get(jid)
                        {
                            let p = (*percent / 100.0).clamp(0.0, 1.0);
                            ui.add(
                                egui::ProgressBar::new(p).text(format!("Overall {:.1}%", *percent)),
                            );
                        } else {
                            ui.add(
                                egui::ProgressBar::new(0.01).text("Queued / waiting for updates…"),
                            );
                        }
                    });
                    ui.add_space(6.0);
                }
                if app.modal_phase_agg.is_empty() {
                    ui.small("No active jobs.");
                }
            });
        ui.label("Recent Cloud Artifacts");
        ui.horizontal(|ui| {
            if ui.small_button("Refresh").clicked() {
                app.modal_refresh_recent();
            }
            ui.small("Click Import to fetch into this project.");
        });
        // Cloud logs (restored)
        ui.separator();
        ui.label("Cloud Logs");
        ui.horizontal(|ui| {
            if ui.small_button("Clear").clicked() {
                app.modal_logs.clear();
            }
            ui.small(format!("{} lines", app.modal_logs.len()));
        });
        egui::ScrollArea::vertical()
            .id_source("cloud_logs")
            .max_height(160.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if app.modal_logs.is_empty() {
                    ui.small("No logs yet.");
                } else {
                    let total = app.modal_logs.len();
                    let start = total.saturating_sub(200);
                    for line in app.modal_logs.iter().skip(start) {
                        ui.monospace(line);
                    }
                }
            });
        egui::ScrollArea::vertical()
            .id_source("cloud_recent")
            .max_height(180.0)
            .show(ui, |ui| {
                if app.modal_recent.is_empty() {
                    ui.small("No recent jobs yet.");
                }
                for (jid, arts) in &app.modal_recent {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong(jid);
                            if arts.is_empty() {
                                ui.small("(no artifacts)");
                            }
                        });
                        for (fname, url) in arts.iter().take(3) {
                            ui.horizontal(|ui| {
                                ui.label(fname);
                                if ui.small_button("Import").clicked() {
                                    app.modal_import_url(url.clone(), Some(fname.clone()));
                                }
                                if ui.small_button("Open").clicked() {
                                    let _ = webbrowser::open(url);
                                }
                            });
                        }
                    });
                }
            });
    });
}

pub(super) fn comfy_embed_in_assets(app: &mut App, ui: &mut egui::Ui) {
    if !EMBED_WEBVIEW_SUPPORTED {
        return;
    }

    if app.comfy_embed_inside && app.comfy_embed_in_assets {
        let running = app.comfy.is_running();
        ui.horizontal(|ui| {
            ui.strong("ComfyUI");
            ui.add(egui::Slider::new(&mut app.comfy_assets_height, 200.0..=900.0).text("Height"));
            ui.separator();
            if ui.small_button("Reload").clicked() {
                if let Some(h) = app.comfy_webview.as_mut() {
                    h.reload();
                    app.comfy_embed_logs.push_back("Reload requested".into());
                }
            }
            if ui.small_button("Browser").clicked() {
                let _ = webbrowser::open(&app.comfy.url());
            }
            ui.separator();
            ui.checkbox(&mut app.comfy_auto_import, "Auto-import");
            ui.separator();
            ui.checkbox(&mut app.comfy_ws_monitor, "Live job monitor")
                .on_hover_text("Listen to ComfyUI WebSocket and import outputs on job completion");
        });
        ui.separator();
        if running {
            if app.comfy_webview.is_none() {
                if let Some(mut host) = crate::embed_webview::create_platform_host() {
                    host.navigate(&app.comfy.url());
                    host.set_visible(true);
                    host.focus();
                    app.comfy_webview = Some(host);
                    app.comfy_embed_logs
                        .push_back("Embedded view created (assets)".into());
                } else {
                    app.comfy_embed_logs
                        .push_back("Failed to create embedded view (assets)".into());
                }
            }
            let w = (ui.available_width() - 8.0).max(0.0);
            let size = egui::vec2(w, app.comfy_assets_height);
            let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
            if let Some(host) = app.comfy_webview.as_mut() {
                let to_floor = |v: f32| -> i32 { v.floor() as i32 };
                let to_ceil = |v: f32| -> i32 { v.ceil() as i32 };
                let r = crate::embed_webview::RectPx {
                    x: to_floor(rect.left()),
                    y: to_floor(rect.top()),
                    w: to_ceil(rect.width()),
                    h: to_ceil(rect.height()),
                };
                host.set_rect(r);
                host.set_visible(true);
                if resp.clicked() || resp.drag_started() {
                    host.focus();
                }
            }
            handle_embedded_comfy_paste(ui, rect, app);
            // height handle
            let hrect = egui::Rect::from_min_size(
                rect.left_bottom() - egui::vec2(0.0, 6.0),
                egui::vec2(rect.width(), 8.0),
            );
            let hresp = ui.interact(
                hrect,
                egui::Id::new("comfy_height_handle"),
                egui::Sense::click_and_drag(),
            );
            let hovered = hresp.hovered() || hresp.dragged();
            let stroke = if hovered {
                egui::Stroke::new(2.0, egui::Color32::from_gray(220))
            } else {
                egui::Stroke::new(1.0, egui::Color32::from_gray(150))
            };
            ui.painter()
                .line_segment([hrect.left_center(), hrect.right_center()], stroke);
            if hresp.dragged() {
                app.comfy_assets_height =
                    (app.comfy_assets_height + hresp.drag_delta().y).clamp(200.0, 900.0);
            }
            ui.separator();
        } else {
            if let Some(mut host) = app.comfy_webview.take() {
                host.close();
            }
        }
    }
}

fn handle_embedded_comfy_paste(ui: &egui::Ui, rect: egui::Rect, app: &mut App) {
    let mut paste_text: Option<String> = None;
    let mut paste_requested = false;
    ui.ctx().input(|input| {
        for event in &input.events {
            if let egui::Event::Paste(text) = event {
                if let Some(pos) = input.pointer.latest_pos() {
                    if rect.contains(pos) {
                        paste_text = Some(text.clone());
                        break;
                    }
                } else {
                    paste_text = Some(text.clone());
                    break;
                }
            }
            if let egui::Event::Key {
                key: egui::Key::V,
                pressed: true,
                modifiers,
                ..
            } = event
            {
                let command_only =
                    modifiers.command && !modifiers.ctrl && !modifiers.alt && !modifiers.shift;
                let ctrl_only =
                    modifiers.ctrl && !modifiers.command && !modifiers.alt && !modifiers.shift;
                if command_only || ctrl_only {
                    if let Some(pos) = input.pointer.latest_pos() {
                        if rect.contains(pos) {
                            paste_requested = true;
                            break;
                        }
                    } else {
                        paste_requested = true;
                        break;
                    }
                }
            }
        }
    });
    if let Some(text) = paste_text {
        if let Some(host) = app.comfy_webview.as_mut() {
            host.focus();
            host.insert_text(&text);
        }
    } else if paste_requested {
        if let Some(host) = app.comfy_webview.as_mut() {
            host.focus();
            host.paste_from_clipboard();
        }
    }
}

pub(super) fn assets_scroll_section(app: &mut App, ui: &mut egui::Ui, assets: &[AssetRow]) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if assets.is_empty() {
                ui.small("No assets in this project yet. Use Import… to add media.");
            } else {
                let cell = 80.0f32; // fixed square thumbnails
                let card_w = cell + 4.0; // small horizontal gap
                let cols = (ui.available_width() / card_w).floor().max(1.0) as usize;
                egui::Grid::new("assets_grid")
                    .num_columns(cols)
                    .spacing([2.0, 8.0])
                    .show(ui, |ui| {
                        for (i, a) in assets.iter().enumerate() {
                            ui.vertical(|ui| {
                                // Square slot
                                let (r, resp) = ui.allocate_exact_size(
                                    egui::vec2(cell, cell),
                                    egui::Sense::click_and_drag(),
                                );
                                // Paint image (contained inside square) or placeholder
                                if let Some(tex) =
                                    app.load_thumb_texture(ui.ctx(), a, cell as u32, cell as u32)
                                {
                                    // Contain inside square based on asset or texture aspect
                                    let (dw, dh) = match (a.width, a.height) {
                                        (Some(w), Some(h)) if w > 0 && h > 0 => {
                                            let aspect = w as f32 / h as f32;
                                            if aspect >= 1.0 {
                                                (cell, (cell / aspect).max(1.0))
                                            } else {
                                                (cell * aspect, cell)
                                            }
                                        }
                                        _ => {
                                            // Fallback assume 16:9
                                            let aspect = 16.0 / 9.0;
                                            (cell, (cell / aspect).max(1.0))
                                        }
                                    };
                                    let img_rect = egui::Rect::from_center_size(
                                        r.center(),
                                        egui::vec2(dw, dh),
                                    );
                                    let uv = egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    );
                                    ui.painter().image(
                                        tex.id(),
                                        img_rect,
                                        uv,
                                        egui::Color32::WHITE,
                                    );
                                    ui.painter().rect_stroke(
                                        r,
                                        4.0,
                                        egui::Stroke::new(1.0, egui::Color32::from_gray(60)),
                                    );
                                } else {
                                    // Placeholder card
                                    ui.painter().rect_filled(
                                        r,
                                        6.0,
                                        egui::Color32::from_rgb(60, 66, 82),
                                    );
                                    // Initial letter overlay
                                    let name = std::path::Path::new(&a.src_abs)
                                        .file_stem()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_else(|| a.src_abs.clone());
                                    let ch = name.chars().next().unwrap_or('?');
                                    ui.painter().text(
                                        r.center(),
                                        egui::Align2::CENTER_CENTER,
                                        ch,
                                        egui::FontId::proportional(28.0),
                                        egui::Color32::WHITE,
                                    );
                                }
                                // Interactions
                                if resp.drag_started() {
                                    app.dragging_asset = Some(a.clone());
                                }
                                if resp.clicked() {
                                    app.add_asset_to_timeline(a);
                                }
                                ui.add_space(2.0);
                                proxy_status_badge(ui, app, a, cell);
                                let name = asset_display_name(a);
                                let lbl = egui::Label::new(egui::RichText::new(name).small());
                                ui.add_sized([cell, 14.0], lbl);
                                ui.small(&a.kind);
                            });
                            if (i + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                        // Ensure the last row ends
                        if assets.len() % cols != 0 {
                            ui.end_row();
                        }
                    });
            }
            ui.separator();
            ui.collapsing("Native Video Decoder", |ui| {
                let available = native_decoder::is_native_decoding_available();
                ui.label(format!(
                    "Native decoding available: {}",
                    if available { "✅ Yes" } else { "❌ No" }
                ));

                if available {
                    ui.label("• VideoToolbox hardware acceleration");
                    ui.label("• Phase 1: CPU plane copies (NV12/P010)");
                    ui.label("• Phase 2: Zero-copy IOSurface (planned)");

                    if ui.button("Test Native Decoder (Phase 1)").clicked() {
                        if let Some(asset) = assets.first() {
                            let config = native_decoder::DecoderConfig {
                                hardware_acceleration: true,
                                preferred_format: Some(native_decoder::YuvPixFmt::Nv12),
                                zero_copy: false,
                            };
                            match native_decoder::create_decoder(&asset.src_abs, config) {
                                Ok(mut decoder) => {
                                    let properties = decoder.get_properties();
                                    ui.label("✅ Phase 1 Decoder created successfully!");
                                    ui.label(format!(
                                        "Video: {}x{} @ {:.1}fps",
                                        properties.width, properties.height, properties.frame_rate
                                    ));
                                    ui.label(format!("Duration: {:.1}s", properties.duration));
                                    ui.label(format!("Format: {:?}", properties.format));

                                    if let Ok(Some(frame)) = decoder.decode_frame(1.0) {
                                        ui.label(format!(
                                            "✅ Frame decoded: {}x{} YUV",
                                            frame.width, frame.height
                                        ));
                                        ui.label(format!("Y plane: {} bytes", frame.y_plane.len()));
                                        ui.label(format!(
                                            "UV plane: {} bytes",
                                            frame.uv_plane.len()
                                        ));
                                    } else {
                                        ui.label("❌ Frame decoding failed");
                                    }
                                }
                                Err(e) => {
                                    ui.label(format!("❌ Decoder creation failed: {}", e));
                                }
                            }
                        } else {
                            ui.label("❌ No assets available for testing");
                        }
                    }

                    if ui.button("Test Zero-Copy Decoder (Phase 2)").clicked() {
                        if let Some(asset) = assets.first() {
                            let config = native_decoder::DecoderConfig {
                                hardware_acceleration: true,
                                preferred_format: Some(native_decoder::YuvPixFmt::Nv12),
                                zero_copy: true,
                            };
                            match native_decoder::create_decoder(&asset.src_abs, config) {
                                Ok(mut decoder) => {
                                    let properties = decoder.get_properties();
                                    ui.label("✅ Phase 2 Zero-Copy Decoder created!");
                                    ui.label(format!(
                                        "Video: {}x{} @ {:.1}fps",
                                        properties.width, properties.height, properties.frame_rate
                                    ));
                                    ui.label(format!(
                                        "Zero-copy supported: {}",
                                        decoder.supports_zero_copy()
                                    ));

                                    #[cfg(target_os = "macos")]
                                    {
                                        if let Ok(Some(iosurface_frame)) =
                                            decoder.decode_frame_zero_copy(1.0)
                                        {
                                            ui.label(format!(
                                                "✅ IOSurface frame decoded: {}x{}",
                                                iosurface_frame.width, iosurface_frame.height
                                            ));
                                            ui.label(format!(
                                                "Surface format: {:?}",
                                                iosurface_frame.format
                                            ));
                                            ui.label(format!(
                                                "Timestamp: {:.3}s",
                                                iosurface_frame.timestamp
                                            ));
                                            ui.label("🎬 Testing WGPU integration...");
                                            ui.label("✅ Zero-copy pipeline ready for rendering!");
                                        } else {
                                            ui.label("❌ Zero-copy frame decoding failed");
                                        }
                                    }
                                    #[cfg(not(target_os = "macos"))]
                                    {
                                        ui.label(
                                            "ℹ️ Zero-copy mode not available on this platform",
                                        );
                                    }
                                }
                                Err(e) => {
                                    ui.label(format!(
                                        "❌ Zero-copy decoder creation failed: {}",
                                        e
                                    ));
                                }
                            }
                        } else {
                            ui.label("❌ No assets available for testing");
                        }
                    }
                } else {
                    ui.label("Native decoding not available on this platform");
                    ui.label("Falling back to FFmpeg-based decoding");
                }
            });
        });
}

fn proxy_status_badge(ui: &mut egui::Ui, app: &App, asset: &AssetRow, width: f32) {
    if !asset.kind.eq_ignore_ascii_case("video") {
        return;
    }
    if let Some(status) = app.proxy_status.get(&asset.id) {
        match status {
            ProxyStatus::Pending => {
                ui.add(
                    egui::ProgressBar::new(0.0)
                        .desired_width(width)
                        .text("Proxy pending…"),
                );
            }
            ProxyStatus::Running { progress } => {
                let pct = progress.clamp(0.0, 1.0);
                ui.add(
                    egui::ProgressBar::new(pct)
                        .desired_width(width)
                        .show_percentage()
                        .text(format!("Proxy {:.0}%", pct * 100.0)),
                );
            }
            ProxyStatus::Failed { message } => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("Proxy failed: {}", message),
                );
            }
            ProxyStatus::Completed { .. } => {
                ui.colored_label(
                    egui::Color32::LIGHT_GREEN,
                    egui::RichText::new("Proxy ready").small(),
                );
            }
        }
        return;
    }
    if asset.is_proxy_ready {
        ui.colored_label(
            egui::Color32::LIGHT_GREEN,
            egui::RichText::new("Proxy ready").small(),
        );
    }
}

fn asset_display_name(asset: &AssetRow) -> String {
    Path::new(&asset.src_abs)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| asset.src_abs.clone())
}

fn describe_proxy_preset(preset: &str) -> &'static str {
    match preset {
        "mac_prores" => "ProRes Proxy",
        "dnxhr_lb" => "DNxHR LB",
        _ => "Unknown",
    }
}

fn describe_proxy_codec(preset: &str) -> &'static str {
    match preset {
        "mac_prores" => "Apple ProRes (Proxy)",
        "dnxhr_lb" => "Avid DNxHR LB",
        _ => "Unknown",
    }
}

fn describe_proxy_container(_preset: &str) -> &'static str {
    "QuickTime MOV"
}

fn describe_proxy_reason(reason: &str) -> String {
    match reason {
        "import" => "Import".to_string(),
        "timeline" => "Timeline".to_string(),
        "playback_lag" => "Playback Lag".to_string(),
        "manual" => "Manual".to_string(),
        "mode" => "Proxy Mode".to_string(),
        other => other.to_string(),
    }
}

fn format_proxy_job_timestamp(completed_at: Option<i64>, updated_at: i64) -> Option<String> {
    let (label, ts) = if let Some(done) = completed_at {
        ("Completed", done)
    } else {
        ("Updated", updated_at)
    };
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt| format!("{}: {}", label, dt.format("%Y-%m-%d %H:%M:%S")))
}

pub(super) fn properties_panel(app: &mut App, ctx: &egui::Context) {
    egui::SidePanel::right("properties")
        .default_width(280.0)
        .show(ctx, |ui| {
            ui.heading("Properties");
            if let Some((ti, ii)) = app.selected {
                if ti < app.seq.tracks.len() && ii < app.seq.tracks[ti].items.len() {
                    let item = &mut app.seq.tracks[ti].items[ii];
                    let clip_type_label = match &item.kind {
                        super::timeline_crate::ItemKind::Video { .. } => "Video",
                        super::timeline_crate::ItemKind::Image { .. } => "Image",
                        super::timeline_crate::ItemKind::Audio { .. } => "Audio",
                        super::timeline_crate::ItemKind::Text { .. } => "Text",
                        super::timeline_crate::ItemKind::Solid { .. } => "Solid",
                    };
                    ui.label(format!("Clip ID: {}", &item.id[..8.min(item.id.len())]));
                    ui.label(format!("Type: {}", clip_type_label));
                    ui.label(format!("From: {}  Dur: {}f", item.from, item.duration_in_frames));
                    let asset_src = match &item.kind {
                        super::timeline_crate::ItemKind::Video { src, .. } => Some(src.clone()),
                        super::timeline_crate::ItemKind::Image { src } => Some(src.clone()),
                        super::timeline_crate::ItemKind::Audio { src, .. } => Some(src.clone()),
                        _ => None,
                    };
                    match &mut item.kind {
                        super::timeline_crate::ItemKind::Video { in_offset_sec, rate, .. } => {
                            let mut pending_rate: Option<f32> = None;
                            let mut pending_offset_frames: Option<i64> = None;
                            ui.separator();
                            ui.label("Video");
                            ui.horizontal(|ui| {
                                ui.label("Rate");
                                let mut r = *rate as f64;
                                let changed = ui
                                    .add(egui::DragValue::new(&mut r).clamp_range(0.05..=8.0).speed(0.02))
                                    .changed();
                                if changed {
                                    *rate = (r as f32).max(0.01);
                                    pending_rate = Some(*rate);
                                }
                                if ui.small_button("1.0").on_hover_text("Reset").clicked() {
                                    *rate = 1.0;
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("In Offset (s)");
                                let mut o = *in_offset_sec;
                                let changed = ui
                                    .add(egui::DragValue::new(&mut o).clamp_range(0.0..=1_000_000.0).speed(0.01))
                                    .changed();
                                if changed {
                                    *in_offset_sec = o.max(0.0);
                                    let num = app.seq.fps.num as f64;
                                    let den = app.seq.fps.den.max(1) as f64;
                                    let frames = ((o.max(0.0)) * (num / den)).round() as i64;
                                    pending_offset_frames = Some(frames.max(0));
                                }
                                if ui.small_button("0").on_hover_text("Reset").clicked() {
                                    *in_offset_sec = 0.0;
                                }
                            });
                            if pending_rate.is_some() || pending_offset_frames.is_some() {
                                if let Ok(uuid) = uuid::Uuid::parse_str(&item.id) {
                                    let node_id = super::timeline_crate::NodeId(uuid);
                                    if let Some(mut node) = app.seq.graph.nodes.get(&node_id).cloned() {
                                        if let super::timeline_crate::TimelineNodeKind::Clip(mut clip) = node.kind.clone() {
                                            if let Some(frames) = pending_offset_frames {
                                                clip.media_range.start = frames;
                                                clip.media_range.duration = clip.timeline_range.duration;
                                            }
                                            if let Some(new_rate) = pending_rate {
                                                clip.playback_rate = new_rate as f64 as f32;
                                            }
                                            node.kind = super::timeline_crate::TimelineNodeKind::Clip(clip);
                                            let _ = app.apply_timeline_command(super::timeline_crate::TimelineCommand::UpdateNode { node });
                                        }
                                    }
                                }
                            }
                        }
                        super::timeline_crate::ItemKind::Image { .. } => {
                            ui.separator();
                            ui.label("Image clip has no time controls");
                        }
                        super::timeline_crate::ItemKind::Audio { .. } => {}
                        _ => {}
                    }
                    let mut asset_row: Option<AssetRow> = None;
                    let mut comfy_meta: Option<serde_json::Value> = None;
                    if let Some(src) = asset_src.as_deref() {
                        if let Ok(Some(asset)) = app.db.find_asset_by_path(&app.project_id, src) {
                            if let Some(meta_str) = asset.metadata_json.as_deref() {
                                let trimmed = meta_str.trim();
                                if !trimmed.is_empty() && trimmed != "null" {
                                    comfy_meta = serde_json::from_str(trimmed).ok();
                                }
                            }
                            asset_row = Some(asset);
                        }
                    }
                    if let Some(asset) = asset_row.as_ref() {
                        ui.separator();
                        ui.label(RichText::new("Media").strong());
                        ui.label(format!("Source Kind: {}", asset.kind));
                        if let Some(path) = asset_src.as_ref() {
                            ui.label(format!("Path: {}", path));
                        }
                        if let (Some(w), Some(h)) = (asset.width, asset.height) {
                            ui.label(format!("Dimensions: {}x{}", w, h));
                        }
                        if let (Some(num), Some(den)) = (asset.fps_num, asset.fps_den) {
                            if num > 0 && den > 0 {
                                let fps = num as f64 / den as f64;
                                ui.label(format!("Frame rate: {:.3} fps ({} / {})", fps, num, den));
                            }
                        }
                        if let Some(frames) = asset.duration_frames {
                            ui.label(format!("Duration: {} frames", frames));
                            if let Some(sec) = asset.duration_seconds() {
                                ui.label(format!("Duration (s): {:.3}", sec));
                            }
                        }
                        if let Some(rate) = asset.sample_rate {
                            ui.label(format!("Sample rate: {} Hz", rate));
                        }
                        if let Some(ch) = asset.audio_channels {
                            ui.label(format!("Audio channels: {}", ch));
                        }
                        if let Some(proxy_path) = asset.proxy_path.as_deref() {
                            ui.label(format!("Proxy Path: {}", proxy_path));
                        }

                        ui.separator();
                        ui.label(RichText::new("Proxy").strong());
                        let status_label = app
                            .proxy_status
                            .get(&asset.id)
                            .map(|status| match status {
                                ProxyStatus::Pending => "Pending".to_string(),
                                ProxyStatus::Running { progress } => format!(
                                    "Running ({:.0}%)",
                                    (progress * 100.0).clamp(0.0, 100.0)
                                ),
                                ProxyStatus::Completed { .. } => "Completed".to_string(),
                                ProxyStatus::Failed { .. } => "Failed".to_string(),
                            })
                            .or_else(|| {
                                if asset.is_proxy_ready {
                                    Some("Ready".to_string())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_else(|| "Not generated".to_string());
                        ui.label(format!("Status: {}", status_label));

                        let proxy_running = matches!(
                            app.proxy_status.get(&asset.id),
                            Some(ProxyStatus::Pending | ProxyStatus::Running { .. })
                        );
                        let button_label = if asset.is_proxy_ready {
                            "Regenerate Proxy"
                        } else {
                            "Generate Proxy"
                        };
                        let button_enabled = app.proxy_queue.is_some() && !proxy_running;
                        if ui
                            .add_enabled(button_enabled, egui::Button::new(button_label))
                            .clicked()
                        {
                            app.queue_proxy_for_asset(asset, ProxyReason::Manual, true);
                        }
                        if asset.is_proxy_ready {
                            if ui
                                .add_enabled(
                                    !proxy_running,
                                    egui::Button::new("Delete Proxy"),
                                )
                                .clicked()
                            {
                                app.delete_proxy_for_asset(asset);
                            }
                        }
                        let proxy_forced = app.is_proxy_preview_forced(&asset.id);
                        let force_label = if proxy_forced {
                            "Preview Original Clip"
                        } else {
                            "Preview Proxy Clip"
                        };
                        let mut force_button = ui.add_enabled(
                            asset.is_proxy_ready || proxy_forced,
                            egui::Button::new(force_label),
                        );
                        if !asset.is_proxy_ready && !proxy_forced {
                            force_button =
                                force_button.on_hover_text("Proxy not ready yet for preview");
                        }
                        if force_button.clicked() {
                            if proxy_forced {
                                app.restore_original_preview_for_asset(&asset.id);
                            } else {
                                app.force_proxy_preview_for_asset(asset);
                            }
                        }
                        let proxy_job_details = app
                            .db
                            .find_latest_proxy_job_for_asset(&asset.id)
                            .ok()
                            .flatten();
                        if let Some(job) = proxy_job_details {
                            ui.add_space(4.0);
                            ui.label(RichText::new("Proxy Details").strong());
                            ui.label(format!(
                                "Preset: {}",
                                describe_proxy_preset(&job.preset)
                            ));
                            ui.label(format!(
                                "Codec: {}",
                                describe_proxy_codec(&job.preset)
                            ));
                            ui.label(format!(
                                "Container: {}",
                                describe_proxy_container(&job.preset)
                            ));
                            if let (Some(w), Some(h)) = (job.width, job.height) {
                                if w > 0 && h > 0 {
                                    ui.label(format!("Target Resolution: {}x{}", w, h));
                                }
                            }
                            if let Some(kbps) = job.bitrate_kbps {
                                if kbps > 0 {
                                    ui.label(format!(
                                        "Target Bitrate: {:.1} Mbps",
                                        kbps as f64 / 1000.0
                                    ));
                                }
                            }
                            if let Some(reason) = job.reason.as_deref() {
                                ui.label(format!(
                                    "Last Job Reason: {}",
                                    describe_proxy_reason(reason)
                                ));
                            }
                            ui.label(format!("Last Job Status: {}", job.status));
                            if let Some(labelled_time) = format_proxy_job_timestamp(
                                job.completed_at,
                                job.updated_at,
                            ) {
                                ui.label(labelled_time);
                            }
                        }
                        if proxy_running {
                            ui.weak("Proxy job currently running…");
                        } else if app.proxy_queue.is_none() {
                            ui.weak("Proxy queue unavailable.");
                        } else if asset.is_proxy_ready {
                            ui.weak("Proxy ready; regenerate if media changed.");
                        }

                        let log_entries = app.proxy_logs.get(&asset.id).cloned();
                        ui.add_space(4.0);
                        ui.label("Logs");
                        ScrollArea::vertical()
                            .max_height(140.0)
                            .id_source(format!("proxy_logs_{}", asset.id))
                            .show(ui, move |ui| {
                                if let Some(entries) = log_entries {
                                    if entries.is_empty() {
                                        ui.weak("No proxy activity yet.");
                                    } else {
                                        for entry in entries.iter().rev() {
                                            ui.label(entry);
                                        }
                                    }
                                } else {
                                    ui.weak("No proxy activity yet.");
                                }
                            });
                    }
                    if let Some(meta) = comfy_meta.as_ref() {
                        if meta
                            .get("source")
                            .and_then(|v| v.as_str())
                            .map(|s| s == "comfy_storyboard")
                            .unwrap_or(false)
                        {
                            ui.separator();
                            ui.label(RichText::new("Storyboard").strong());
                            if let Some(title) = meta.get("card_title").and_then(|v| v.as_str()) {
                                ui.label(format!("Card: {}", title));
                            }
                            if let Some(workflow) =
                                meta.get("workflow_name").and_then(|v| v.as_str())
                            {
                                ui.label(format!("Workflow: {}", workflow));
                            }
                            if let Some(queued) = meta.get("queued_at").and_then(|v| v.as_str()) {
                                ui.label(format!("Queued: {}", queued));
                            }
                            if let Some(completed) =
                                meta.get("completed_at").and_then(|v| v.as_str())
                            {
                                ui.label(format!("Completed: {}", completed));
                            }
                            if let Some(desc) = meta
                                .get("card_description")
                                .and_then(|v| v.as_str())
                            {
                                if !desc.trim().is_empty() {
                                    ui.label("Description:");
                                    ui.label(desc);
                                }
                            }
                            if let Some(inputs) =
                                meta.get("workflow_inputs").and_then(|v| v.as_object())
                            {
                                if !inputs.is_empty() {
                                    ui.collapsing("Workflow Inputs", |ui| {
                                        let mut keys: Vec<_> = inputs.keys().collect();
                                        keys.sort();
                                        for key in keys {
                                            let value = &inputs[key];
                                            if let Some(text) = value.as_str() {
                                                ui.label(format!("{}: {}", key, text));
                                            } else {
                                                ui.label(format!("{}: {}", key, value));
                                            }
                                        }
                                    });
                                }
                            }
                        } else {
                            ui.separator();
                            ui.collapsing("Metadata", |ui| {
                                ui.label(meta.to_string());
                            });
                        }
                    }
                } else {
                    ui.label("Selection out of range");
                }
            } else {
                ui.label("Sequence");
                ui.add_space(4.0);
                ui.label(format!(
                    "Resolution: {}x{}",
                    app.seq.width,
                    app.seq.height
                ));
                let fps_num = app.seq.fps.num.max(1);
                let fps_den = app.seq.fps.den.max(1);
                let fps = (fps_num as f64) / (fps_den as f64);
                ui.label(format!("Frame rate: {:.3} fps ({} / {})", fps, fps_num, fps_den));
                ui.label(format!(
                    "Duration: {:.2} s ({} frames)",
                    crate::timeline::ui::frames_to_seconds(app.seq.duration_in_frames, app.seq.fps),
                    app.seq.duration_in_frames
                ));
                ui.add_space(8.0);
                ui.label("Select a clip to edit rate or in-offset.");
            }
        });
}

pub(super) fn center_editor(app: &mut App, ctx: &egui::Context, frame: &mut eframe::Frame) {
    // No floating window: when not embedding in assets, ensure any host is closed.
    if !(app.comfy_embed_inside && app.comfy_embed_in_assets) {
        if let Some(mut host) = app.comfy_webview.take() {
            host.close();
        }
    }

    egui::CentralPanel::default().show(ctx, |ui| {
        egui::Resize::default()
            .id_salt("preview_resize")
            .default_size(egui::vec2(ui.available_width(), 360.0))
            .show(ui, |ui| {
                app.preview_ui(ctx, frame, ui);
            });
        ui.add_space(4.0);
        ui.separator();

        ui.horizontal(|ui| {
            ui.heading("Timeline");
            if ui.small_button("+ Video Track").clicked() {
                let binding = timeline_crate::TrackBinding {
                    id: timeline_crate::TrackId::new(),
                    name: String::new(),
                    kind: timeline_crate::TrackKind::Video,
                    node_ids: Vec::new(),
                };
                app.seq.graph.tracks.insert(0, binding);
                app.sync_tracks_from_graph();
                let _ = app.save_project_timeline();
            }
            if ui.small_button("+ Audio Track").clicked() {
                let binding = timeline_crate::TrackBinding {
                    id: timeline_crate::TrackId::new(),
                    name: String::new(),
                    kind: timeline_crate::TrackKind::Audio,
                    node_ids: Vec::new(),
                };
                let _ = app.apply_timeline_command(timeline_crate::TimelineCommand::UpsertTrack {
                    track: binding,
                });
                let _ = app.save_project_timeline();
            }
            if ui.small_button("− Last Video").clicked() {
                if let Some((_idx, id)) =
                    app.seq
                        .graph
                        .tracks
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(_i, t)| match t.kind {
                            timeline_crate::TrackKind::Video => Some((_i, t.id)),
                            _ => None,
                        })
                {
                    let _ =
                        app.apply_timeline_command(timeline_crate::TimelineCommand::RemoveTrack {
                            track_id: id,
                        });
                    let _ = app.save_project_timeline();
                }
            }
            if ui.small_button("− Last Audio").clicked() {
                if let Some((_idx, id)) =
                    app.seq
                        .graph
                        .tracks
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(_i, t)| match t.kind {
                            timeline_crate::TrackKind::Audio => Some((_i, t.id)),
                            _ => None,
                        })
                {
                    let _ =
                        app.apply_timeline_command(timeline_crate::TimelineCommand::RemoveTrack {
                            track_id: id,
                        });
                    let _ = app.save_project_timeline();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(t) = app.last_save_at {
                    let ago = std::time::Instant::now().saturating_duration_since(t);
                    let label = if ago.as_secs_f32() < 2.0 {
                        "Saved".to_string()
                    } else {
                        format!("Autosave {}s ago", ago.as_secs())
                    };
                    ui.small(label);
                }
                let cache_stats = format!(
                    "Cache: {}/{} hits",
                    app.preview.cache_hits,
                    app.preview.cache_hits + app.preview.cache_misses
                );
                ui.small(&cache_stats);
                if ui.small_button("Save Project").clicked() {
                    let _ = app.save_project_timeline();
                }
            });
        });

        app.timeline_ui(ui);
    });
}

pub(super) fn drag_overlay(app: &mut App, ctx: &egui::Context) {
    if let Some(asset) = app.dragging_asset.clone() {
        if let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("dragging_asset_overlay"),
            ));
            let thumb_w = app.asset_thumb_w.min(200.0).max(80.0);
            let thumb_h = thumb_w / 16.0 * 9.0;
            let rect = egui::Rect::from_center_size(
                pos + egui::vec2(0.0, -thumb_h / 2.0),
                egui::vec2(thumb_w, thumb_h),
            );
            painter.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(128));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Dragging...",
                egui::FontId::proportional(12.0),
                egui::Color32::WHITE,
            );
        }
    }
}
