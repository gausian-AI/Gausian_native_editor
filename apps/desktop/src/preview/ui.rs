use eframe::egui;

use crate::decode::{DecodeCmd, FramePayload, PlayState};
use crate::gpu::readback::{ReadbackResult, ReadbackTag};
use crate::gpu::sync::PlaybackPhase;
use crate::preview::state::upload_plane;
use crate::preview::{visual_source_at, PreviewShaderMode, PreviewState, StreamMetadata};
use crate::proxy_queue::ProxyReason;
use crate::App;
use anyhow::Context;
use image::GenericImageView;
use renderer::{
    convert_yuv_to_rgba, ColorSpace as RenderColorSpace, PixelFormat as RenderPixelFormat,
};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, trace};

fn fit_rect_to_content(rect: egui::Rect, content_w: f32, content_h: f32) -> egui::Rect {
    if content_w <= 0.0 || content_h <= 0.0 {
        return rect;
    }
    let rect_size = rect.size();
    if rect_size.x <= 0.0 || rect_size.y <= 0.0 {
        return rect;
    }
    let content_aspect = content_w / content_h;
    let rect_aspect = rect_size.x / rect_size.y;
    let (width, height) = if rect_aspect > content_aspect {
        let height = rect_size.y;
        (height * content_aspect, height)
    } else {
        let width = rect_size.x;
        (width, width / content_aspect)
    };
    let size = egui::vec2(width, height);
    egui::Rect::from_center_size(rect.center(), size)
}

impl App {
    pub(crate) fn preview_ui(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        ui: &mut egui::Ui,
    ) {
        // Determine current visual source at playhead (lock to exact frame)
        let fps = self.seq.fps.num.max(1) as f64 / self.seq.fps.den.max(1) as f64;
        let t_playhead = self.playback_clock.now();
        let playhead_frame = if self.engine.state == PlayState::Playing {
            (t_playhead * fps).floor() as i64
        } else {
            (t_playhead * fps).round() as i64
        };
        self.playhead = playhead_frame;
        let _target_ts = (playhead_frame as f64) / fps;
        let source = visual_source_at(&self.seq.graph, self.playhead);

        // Debug: shader mode toggle for YUV preview
        ui.horizontal(|ui| {
            ui.label("Shader:");
            let mode = &mut self.preview.shader_mode;
            let solid = matches!(*mode, PreviewShaderMode::Solid);
            if ui.selectable_label(solid, "Solid").clicked() {
                *mode = PreviewShaderMode::Solid;
                ctx.request_repaint();
            }
            let showy = matches!(*mode, PreviewShaderMode::ShowY);
            if ui.selectable_label(showy, "Y").clicked() {
                *mode = PreviewShaderMode::ShowY;
                ctx.request_repaint();
            }
            let uvd = matches!(*mode, PreviewShaderMode::UvDebug);
            if ui.selectable_label(uvd, "UV").clicked() {
                *mode = PreviewShaderMode::UvDebug;
                ctx.request_repaint();
            }
            let nv12 = matches!(*mode, PreviewShaderMode::Nv12);
            if ui.selectable_label(nv12, "NV12").clicked() {
                *mode = PreviewShaderMode::Nv12;
                ctx.request_repaint();
            }
        });
        // Hotkeys 1/2/3
        if ui.input(|i| i.key_pressed(egui::Key::Num1)) {
            self.preview.shader_mode = PreviewShaderMode::Solid;
            ctx.request_repaint();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Num2)) {
            self.preview.shader_mode = PreviewShaderMode::ShowY;
            ctx.request_repaint();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Num3)) {
            self.preview.shader_mode = PreviewShaderMode::UvDebug;
            ctx.request_repaint();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Num4)) {
            self.preview.shader_mode = PreviewShaderMode::Nv12;
            ctx.request_repaint();
        }

        // Layout: reserve a 16:9 box or fit available space
        let avail = ui.available_size();
        let mut w = avail.x.max(320.0);
        let mut h = (w * 9.0 / 16.0).round();
        if h > avail.y {
            h = avail.y;
            w = (h * 16.0 / 9.0).round();
        }
        let scale_factor = self.viewer_scale.factor();

        // Playback progression handled by PlaybackClock (no speed-up)

        // Draw
        // Header controls
        ui.horizontal(|ui| {
            ui.label("Preview Mode:");
            let mut strict = self.strict_pause;
            if ui.checkbox(&mut strict, "Strict Pause").on_hover_text("Show exact frame while paused (placeholder while seeking) vs. show last frame until target arrives").changed() {
                self.strict_pause = strict;
            }
        });

        let (rect, _resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(12, 12, 12));
        let video_rect = if scale_factor < 1.0 {
            let draw_w = (rect.width() * scale_factor).max(160.0);
            let draw_h = (rect.height() * scale_factor).max(90.0);
            egui::Rect::from_center_size(rect.center(), egui::vec2(draw_w, draw_h))
        } else {
            rect
        };

        if matches!(self.engine.state, PlayState::Playing) {
            self.last_seek_request_at = None;
        }

        // Use persistent decoder with prefetch
        // Solid/text generators fallback
        if let Some(src) = source.as_ref() {
            if src.path.starts_with("solid:") {
                let hex = src.path.trim_start_matches("solid:");
                let color = crate::timeline::ui::parse_hex_color(hex)
                    .unwrap_or(egui::Color32::from_rgb(80, 80, 80));
                painter.rect_filled(rect, 4.0, color);
                return;
            }
            if src.path.starts_with("text://") {
                painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(20, 20, 20));
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Text Generator",
                    egui::FontId::proportional(24.0),
                    egui::Color32::WHITE,
                );
                return;
            }
        }

        let Some(src) = source else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "No Preview",
                egui::FontId::proportional(16.0),
                egui::Color32::GRAY,
            );
            return;
        };
        let Some(rs) = frame.wgpu_render_state() else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "No WGPU state",
                egui::FontId::proportional(16.0),
                egui::Color32::GRAY,
            );
            return;
        };

        let phase = match self.engine.state {
            PlayState::Playing => PlaybackPhase::PlayingRealtime,
            PlayState::Scrubbing | PlayState::Seeking => PlaybackPhase::ScrubbingOrSeeking,
            PlayState::Paused => PlaybackPhase::Paused,
        };
        self.preview.update_gpu_phase(rs, phase);
        let Some(gpu_ctx) = self.preview.gpu_context(rs) else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "GPU sync unavailable",
                egui::FontId::proportional(16.0),
                egui::Color32::GRAY,
            );
            return;
        };

        // Special-case images: render directly without video decode/seek
        if src.is_image {
            // Load or refresh texture when source or size changes
            let need_reload = match self.preview.current_source.as_ref() {
                Some(prev) => {
                    prev.path != src.path || self.preview.last_size != (w as u32, h as u32)
                }
                None => true,
            };
            if need_reload {
                match image::open(&src.path) {
                    Ok(mut img) => {
                        let (orig_w, orig_h) = img.dimensions();
                        let fitted = fit_rect_to_content(video_rect, orig_w as f32, orig_h as f32);
                        let target_w = fitted.width().max(1.0).round() as u32;
                        let target_h = fitted.height().max(1.0).round() as u32;
                        if (target_w, target_h) != (orig_w, orig_h) {
                            img = img.resize_exact(
                                target_w,
                                target_h,
                                image::imageops::FilterType::Lanczos3,
                            );
                        }
                        let rgba = img.to_rgba8();
                        let (iw, ih) = img.dimensions();
                        let bytes = rgba.as_raw();
                        let color = egui::ColorImage::from_rgba_unmultiplied(
                            [iw as usize, ih as usize],
                            bytes,
                        );
                        let tex =
                            ctx.load_texture("preview_image", color, egui::TextureOptions::LINEAR);
                        self.preview.texture = Some(tex);
                        self.preview.current_source = Some(src.clone());
                        self.preview.last_size = (w as u32, h as u32);
                    }
                    Err(_) => {
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "Failed to load image",
                            egui::FontId::proportional(16.0),
                            egui::Color32::RED,
                        );
                        return;
                    }
                }
            }
            if let Some(tex) = &self.preview.texture {
                let size = tex.size();
                let dest = fit_rect_to_content(video_rect, size[0] as f32, size[1] as f32);
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                painter.image(tex.id(), dest, uv, egui::Color32::WHITE);
            }
            return;
        }

        let (timeline_path, media_t) = self
            .active_video_media_time_graph(t_playhead)
            .unwrap_or_else(|| (src.path.clone(), t_playhead));
        let playback = self.determine_playback_path(&timeline_path);
        if let (Some(asset), Some(reason)) = (playback.asset.as_ref(), playback.queue_reason) {
            self.queue_proxy_for_asset(asset, reason, matches!(reason, ProxyReason::Mode));
        }
        let active_path = playback.decode_path.clone();
        let using_proxy = playback.using_proxy;
        let using_optimized = playback.using_optimized;
        let active_asset = playback.asset.clone();
        let clip_id = active_asset
            .as_ref()
            .map(|a| a.id.as_str())
            .unwrap_or("<unknown>");
        let tier = if using_optimized {
            "optimized"
        } else if using_proxy {
            "proxy"
        } else {
            "original"
        };
        let log_key = format!("{}::{}", active_path, tier);
        if self.preview.last_logged_playback.as_ref() != Some(&log_key) {
            info!(
                "[preview] playback_start clip={} tier={} path={} t={:.3}",
                clip_id, tier, active_path, media_t
            );
            self.preview.last_logged_playback = Some(log_key.clone());
            self.preview.last_interactive_request = None;
        }
        self.engine.target_pts = media_t;
        self.decode_mgr.ensure_worker(
            &active_path,
            active_asset.as_ref().map(|a| a.id.as_str()),
            ctx,
        );

        // Debounce decode commands
        let fps_seq = (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
        let seek_bucket = (media_t * fps_seq).round() as i64;
        // Compute clip fps (from latest) to derive epsilon tolerances.
        let clip_fps = self
            .decode_mgr
            .take_latest(&active_path)
            .map(|f| f.props.fps as f64)
            .filter(|v| *v > 0.0 && v.is_finite())
            .unwrap_or_else(|| (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64));
        let frame_dur = if clip_fps > 0.0 {
            1.0 / clip_fps
        } else {
            1.0 / 30.0
        };
        let epsilon = (0.25 * frame_dur).max(0.010);

        // Dispatch commands based on state with epsilon gating.
        let mut lagging_frame = false;
        match self.engine.state {
            PlayState::Playing => {
                // Always send initial Play on state/path change
                let k = (self.engine.state, active_path.clone(), None);
                if self.last_sent != Some(k.clone()) {
                    let _ = self.decode_mgr.send_cmd(
                        &active_path,
                        DecodeCmd::Play {
                            start_pts: media_t,
                            rate: self.engine.rate,
                        },
                    );
                    self.last_sent = Some(k);
                    self.last_seek_sent_pts = None;
                    self.last_play_reanchor_time = Some(std::time::Instant::now());
                }
            }
            PlayState::Scrubbing | PlayState::Seeking => {
                let need = match self.last_seek_sent_pts {
                    Some(last) => (media_t - last).abs() > epsilon,
                    None => true,
                };
                if need {
                    // Hybrid: only clear if target moved significantly (> ~2 frames)
                    let mut should_clear = true;
                    if let Some((ref p, last_pts)) = self.last_present_pts.as_ref() {
                        if p == &active_path {
                            let fps_clip = self
                                .decode_mgr
                                .take_latest(&active_path)
                                .map(|f| f.props.fps as f64)
                                .filter(|v| *v > 0.0 && v.is_finite())
                                .unwrap_or_else(|| {
                                    (self.seq.fps.num.max(1) as f64)
                                        / (self.seq.fps.den.max(1) as f64)
                                });
                            let frame_dur = if fps_clip > 0.0 {
                                1.0 / fps_clip
                            } else {
                                1.0 / 30.0
                            };
                            let dt_frames = ((media_t - *last_pts).abs() / frame_dur).abs();
                            should_clear =
                                dt_frames > (self.settings.clear_threshold_frames as f64);
                        }
                    }
                    if should_clear {
                        self.decode_mgr.clear_latest(&active_path);
                    }
                    let _ = self.decode_mgr.send_cmd(
                        &active_path,
                        DecodeCmd::Seek {
                            target_pts: media_t,
                        },
                    );
                    // Worker will transition to Paused after delivering a frame; keep UI paused/scrubbing
                    self.last_seek_sent_pts = Some(media_t);
                    self.last_seek_request_at = Some(std::time::Instant::now());
                    ctx.request_repaint();
                }
                // Force next Play to re-send anchor
                self.last_sent = None;
            }
            PlayState::Paused => {
                let need = match self.last_seek_sent_pts {
                    Some(last) => (media_t - last).abs() > epsilon,
                    None => true,
                };
                // Adaptive re-seek while waiting for accurate preroll in strict paused mode.
                // If we have only an approximate (KEY_UNIT) frame, give the backend more time
                // based on clip frame duration before re-sending the seek.
                let newest_for_timeout = self.decode_mgr.take_latest(&active_path);
                let waiting_for_accurate = newest_for_timeout
                    .as_ref()
                    .map(|f| !f.accurate)
                    .unwrap_or(true);
                let adaptive_ms: u128 = {
                    // Use clip fps when available to derive a patient timeout (e.g., ~8 frames)
                    let v = (frame_dur * 1000.0 * 8.0).max(450.0).min(1500.0);
                    v as u128
                };
                // If accurate hasn't arrived yet, use adaptive timeout; otherwise avoid re-seeking.
                let stale_seek = self
                    .last_seek_request_at
                    .map(|t| {
                        let ms = t.elapsed().as_millis();
                        if waiting_for_accurate {
                            ms > adaptive_ms
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if need || stale_seek {
                    // Hybrid: only clear if target moved significantly (> ~2 frames)
                    let mut should_clear = true;
                    if let Some((ref p, last_pts)) = self.last_present_pts.as_ref() {
                        if p == &active_path {
                            let fps_clip = self
                                .decode_mgr
                                .take_latest(&active_path)
                                .map(|f| f.props.fps as f64)
                                .filter(|v| *v > 0.0 && v.is_finite())
                                .unwrap_or_else(|| {
                                    (self.seq.fps.num.max(1) as f64)
                                        / (self.seq.fps.den.max(1) as f64)
                                });
                            let frame_dur = if fps_clip > 0.0 {
                                1.0 / fps_clip
                            } else {
                                1.0 / 30.0
                            };
                            let dt_frames = ((media_t - *last_pts).abs() / frame_dur).abs();
                            should_clear =
                                dt_frames > (self.settings.clear_threshold_frames as f64);
                        }
                    }
                    if should_clear {
                        self.decode_mgr.clear_latest(&active_path);
                    }
                    let _ = self.decode_mgr.send_cmd(
                        &active_path,
                        DecodeCmd::Seek {
                            target_pts: media_t,
                        },
                    );
                    self.last_seek_sent_pts = Some(media_t);
                    self.last_seek_request_at = Some(std::time::Instant::now());
                    ctx.request_repaint();
                }
                // Force next Play to re-send anchor
                self.last_sent = None;
            }
        }

        // Drain worker and pick latest frame
        let newest = self.decode_mgr.take_latest(&active_path);
        // Use active clip fps (fallback to sequence) for display tolerance
        let tol = {
            let fps_clip = self
                .decode_mgr
                .take_latest(&active_path)
                .map(|f| f.props.fps as f64)
                .filter(|v| *v > 0.0 && v.is_finite())
                .unwrap_or_else(|| {
                    (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64)
                });
            let frame_dur = if fps_clip > 0.0 {
                1.0 / fps_clip
            } else {
                1.0 / 30.0
            };
            // Use user-configured hybrid tolerance
            let frames = if self.strict_pause {
                self.settings.strict_tolerance_frames as f64
            } else {
                self.settings.paused_tolerance_frames as f64
            };
            (frames * frame_dur).max(0.020)
        };
        // Gate what we display to avoid flicker from the old time base:
        // - While playing: show newest (streaming)
        // - While seeking/scrubbing with strict_pause: show KEY_UNIT (accurate=false) immediately,
        //   or only frames near the target; drop far old frames.
        // - Non-strict: accept frames near the target.
        let picked = if matches!(self.engine.state, PlayState::Playing) {
            newest.clone()
        } else if self.strict_pause {
            match newest.as_ref() {
                Some(f) if !f.accurate => newest.clone(), // allow fast keyframe stage even if far
                Some(f) if (f.pts - media_t).abs() <= tol => newest.clone(),
                _ => None,
            }
        } else {
            match newest.as_ref() {
                Some(f) if (f.pts - media_t).abs() <= tol => newest.clone(),
                _ => None,
            }
        };

        // If any frame exists while paused, clear the seeking timer to avoid indefinite "seeking…"
        let has_newest = newest.is_some();
        if !matches!(self.engine.state, PlayState::Playing) && has_newest {
            self.last_seek_request_at = None;
        }

        if let Some(frame_out) = picked.as_ref() {
            // Clear seeking timer when we have a frame in paused/scrubbing
            if !matches!(self.engine.state, PlayState::Playing) {
                self.last_seek_request_at = None;
            }
            trace!(
                width = frame_out.props.w,
                height = frame_out.props.h,
                fmt = ?frame_out.props.fmt,
                pts = frame_out.pts,
                "preview dequeued frame"
            );
            // Re-anchor while playing if preview drifts from playhead beyond tolerance.
            if matches!(self.engine.state, PlayState::Playing) {
                let dt = (frame_out.pts - media_t).abs();
                let frame_budget = frame_dur.max(1.0 / 60.0);
                if dt > frame_budget * 1.25 {
                    lagging_frame = true;
                }
                let cooldown_ok = self
                    .last_play_reanchor_time
                    .map(|t| t.elapsed().as_millis() >= 150)
                    .unwrap_or(true);
                if dt > (0.5 * frame_dur).max(0.015) && cooldown_ok {
                    let _ = self.decode_mgr.send_cmd(
                        &active_path,
                        DecodeCmd::Play {
                            start_pts: media_t,
                            rate: self.engine.rate,
                        },
                    );
                    self.last_play_reanchor_time = Some(std::time::Instant::now());
                }
            }
            if let FramePayload::Cpu { y, uv } = &frame_out.payload {
                {
                    let mut renderer = rs.renderer.write();
                    let slot = self.preview.ensure_stream_slot(
                        &gpu_ctx,
                        &mut renderer,
                        StreamMetadata {
                            stream_id: active_path.clone(),
                            width: frame_out.props.w,
                            height: frame_out.props.h,
                            fmt: frame_out.props.fmt,
                            clear_color: egui::Color32::BLACK,
                        },
                    );
                    if let (Some(out_tex), Some(out_view)) =
                        (slot.out_tex.as_ref(), slot.out_view.as_ref())
                    {
                        let pixel_format = match frame_out.props.fmt {
                            media_io::YuvPixFmt::Nv12 => RenderPixelFormat::Nv12,
                            media_io::YuvPixFmt::P010 => RenderPixelFormat::P010,
                        };
                        if let Ok(rgba) = convert_yuv_to_rgba(
                            pixel_format,
                            RenderColorSpace::Rec709,
                            frame_out.props.w,
                            frame_out.props.h,
                            y.as_ref(),
                            uv.as_ref(),
                        ) {
                            gpu_ctx.with_queue(|queue| {
                                upload_plane(
                                    queue,
                                    &**out_tex,
                                    &rgba,
                                    frame_out.props.w,
                                    frame_out.props.h,
                                    (frame_out.props.w as usize) * 4,
                                    4,
                                );
                            });
                            if let Some(id) = slot.egui_tex_id {
                                gpu_ctx.with_device(|device| {
                                    renderer.update_egui_texture_from_wgpu_texture(
                                        device,
                                        out_view,
                                        eframe::wgpu::FilterMode::Linear,
                                        id,
                                    );
                                });
                                crate::gpu_pump!(&gpu_ctx, "ui_present_upload");
                                let uv_rect = egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                );
                                let dest = fit_rect_to_content(
                                    video_rect,
                                    frame_out.props.w as f32,
                                    frame_out.props.h as f32,
                                );
                                painter.image(id, dest, uv_rect, egui::Color32::WHITE);
                                trace!("preview presented frame");
                                // Update last presented pts for hybrid clearing heuristic
                                self.last_present_pts = Some((active_path.clone(), frame_out.pts));
                            }
                            if matches!(
                                self.engine.state,
                                PlayState::Scrubbing | PlayState::Seeking
                            ) {
                                self.preview.request_scrub_readback();
                            }
                        }
                    }
                }
            }
        } else if !matches!(self.engine.state, PlayState::Playing) {
            let mut drew_previous = false;
            if let Some(slot) = self.preview.stream.as_ref() {
                if let (Some(id), w, h) = (slot.egui_tex_id, slot.width, slot.height) {
                    let dest = fit_rect_to_content(video_rect, w as f32, h as f32);
                    let uv_rect =
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    painter.image(id, dest, uv_rect, egui::Color32::WHITE);
                    drew_previous = true;
                }
            }

            if !drew_previous {
                let seeking_label = if let Some(t0) = self.last_seek_request_at {
                    let ms = t0.elapsed().as_millis();
                    format!("Seeking… ({} ms)", ms)
                } else {
                    "Seeking…".to_string()
                };
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    seeking_label,
                    egui::FontId::proportional(16.0),
                    egui::Color32::GRAY,
                );
            }
        }

        if matches!(self.engine.state, PlayState::Playing) && picked.is_none() {
            lagging_frame = true;
        }

        self.update_playback_adaptive(
            lagging_frame,
            frame_dur,
            active_asset.as_ref(),
            using_proxy || using_optimized,
        );

        if lagging_frame {
            self.preview.interactive_policy.note_lag(
                true,
                self.engine.state,
                std::time::Instant::now(),
            );
        }

        // Lightweight debug overlay: resolved source path and media time, plus lock indicator
        // Determine displayed pts if any
        let latest = self.decode_mgr.take_latest(&active_path);
        let displayed_pts = latest.as_ref().map(|f| f.pts);
        let displayed_approx = latest.as_ref().map(|f| !f.accurate).unwrap_or(false);
        let diff = displayed_pts.map(|p| (p - media_t).abs());
        let locked = diff
            .map(|d| d <= (0.5 * frame_dur).max(0.015))
            .unwrap_or(false);
        let source_suffix = if using_optimized {
            " [optimized]"
        } else if using_proxy {
            " [proxy]"
        } else {
            ""
        };
        let overlay = format!(
            "src: {}{} (scale {}, mode {})\nmedia_t: {:.3}s  state: {:?}  {}\nlock: {}{}",
            std::path::Path::new(&timeline_path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(timeline_path.clone()),
            source_suffix,
            self.viewer_scale.label(),
            self.effective_proxy_mode().display_name(),
            media_t,
            self.engine.state,
            if displayed_approx { "approx" } else { "" },
            if locked { "✓" } else { "✕" },
            diff.map(|d| format!("  Δ={:.3}s", d)).unwrap_or_default()
        );
        let margin = egui::vec2(8.0, 6.0);
        painter.text(
            rect.left_top() + margin,
            egui::Align2::LEFT_TOP,
            overlay,
            egui::FontId::monospace(11.0),
            egui::Color32::from_gray(180),
        );

        if self.preview.last_play_state_for_readback != Some(self.engine.state) {
            if matches!(self.engine.state, PlayState::Paused) {
                self.preview.request_capture_readback();
            }
            self.preview.last_play_state_for_readback = Some(self.engine.state);
        }

        self.preview.process_readback(&gpu_ctx, self.engine.state);

        if let Some(stats) = self.decode_mgr.worker_stats_snapshot(&active_path) {
            self.preview
                .interactive_policy
                .note_first_frame_ms(stats.first_frame_ms);
        }

        let desired_interactive = self.preview.interactive_policy.evaluate(
            clip_id,
            tier,
            self.engine.state,
            std::time::Instant::now(),
        );

        if self.preview.last_interactive_request != Some(desired_interactive) {
            let reason = if desired_interactive {
                match self.engine.state {
                    PlayState::Scrubbing | PlayState::Seeking => "scrub",
                    PlayState::Playing => "playback_start",
                    PlayState::Paused => "paused",
                }
            } else if matches!(self.engine.state, PlayState::Paused) {
                "paused"
            } else if tier != "original" {
                "proxy"
            } else {
                "realtime"
            };
            info!(
                "[interactive] request clip={} interactive={} reason={}",
                clip_id, desired_interactive, reason
            );
            self.preview.last_interactive_request = Some(desired_interactive);
        }

        self.decode_mgr.set_interactive(
            &active_path,
            active_asset.as_ref().map(|a| a.id.as_str()),
            desired_interactive,
        );

        let readback_results = self.preview.take_readback_results();
        if !readback_results.is_empty() {
            self.handle_preview_readback_results(readback_results);
        }
    }

    fn handle_preview_readback_results(&mut self, results: Vec<ReadbackResult>) {
        for result in results {
            match result.tag {
                ReadbackTag::Capture => {
                    if let Err(err) = self.persist_preview_capture(&result) {
                        tracing::error!(
                            target = "preview_readback",
                            error = %err,
                            "failed to persist preview capture"
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn persist_preview_capture(&mut self, result: &ReadbackResult) -> anyhow::Result<()> {
        let capture_root = std::env::temp_dir().join("gausian_preview");
        std::fs::create_dir_all(&capture_root).with_context(|| {
            format!(
                "create preview capture directory {}",
                capture_root.display()
            )
        })?;

        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let filename = format!(
            "preview_capture_{}_{}x{}.png",
            timestamp_ns, result.extent.width, result.extent.height
        );
        let path = capture_root.join(filename);

        let width = result.extent.width as usize;
        let height = result.extent.height as usize;
        let row_pitch = result.bytes_per_row as usize;
        let mut rgba = vec![0u8; width * height * 4];
        for row in 0..height {
            let src_off = row * row_pitch;
            let dst_off = row * width * 4;
            rgba[dst_off..dst_off + width * 4]
                .copy_from_slice(&result.pixels[src_off..src_off + width * 4]);
        }

        image::save_buffer(
            &path,
            &rgba,
            result.extent.width,
            result.extent.height,
            image::ColorType::Rgba8,
        )
        .with_context(|| format!("failed to write preview capture {}", path.display()))?;

        self.preview_last_capture = Some(crate::PreviewCapture {
            path: path.clone(),
            timestamp: std::time::Instant::now(),
            width: result.extent.width,
            height: result.extent.height,
        });

        tracing::info!(
            target = "preview_readback",
            path = %path.display(),
            width = result.extent.width,
            height = result.extent.height,
            "preview capture saved"
        );

        Ok(())
    }
}
