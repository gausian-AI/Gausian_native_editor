use eframe::egui;

use crate::decode::{DecodeCmd, FramePayload, PlayState};
use crate::preview::state::upload_plane;
use crate::preview::{visual_source_at, PreviewShaderMode, PreviewState, StreamMetadata};
use crate::App;
use renderer::{
    convert_yuv_to_rgba, ColorSpace as RenderColorSpace, PixelFormat as RenderPixelFormat,
};
use tracing::trace;

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
        if frame.wgpu_render_state().is_none() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "No WGPU state",
                egui::FontId::proportional(16.0),
                egui::Color32::GRAY,
            );
            return;
        }

        let (active_path, media_t) = self
            .active_video_media_time_graph(t_playhead)
            .unwrap_or_else(|| (src.path.clone(), t_playhead));
        self.engine.target_pts = media_t;
        self.decode_mgr.ensure_worker(&active_path, ctx);

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
        let frame_dur = if clip_fps > 0.0 { 1.0 / clip_fps } else { 1.0 / 30.0 };
        let epsilon = (0.25 * frame_dur).max(0.010);

        // Dispatch commands based on state with epsilon gating.
        match self.engine.state {
            PlayState::Playing => {
                // Always send initial Play on state/path change
                let k = (self.engine.state, active_path.clone(), None);
                if self.last_sent != Some(k.clone()) {
                    let _ = self.decode_mgr.send_cmd(
                        &active_path,
                        DecodeCmd::Play { start_pts: media_t, rate: self.engine.rate },
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
                    let _ = self
                        .decode_mgr
                        .send_cmd(&active_path, DecodeCmd::Seek { target_pts: media_t });
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
                        if waiting_for_accurate { ms > adaptive_ms } else { false }
                    })
                    .unwrap_or(false);
                if need || stale_seek {
                    let _ = self
                        .decode_mgr
                        .send_cmd(&active_path, DecodeCmd::Seek { target_pts: media_t });
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
                .unwrap_or_else(|| (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64));
            let frame_dur = if fps_clip > 0.0 { 1.0 / fps_clip } else { 1.0 / 30.0 };
            // Accept up to one full frame (or a small floor) to avoid stuck placeholder
            (1.0 * frame_dur).max(0.020)
        };
        let picked = if matches!(self.engine.state, PlayState::Playing) {
            newest
        } else if self.strict_pause {
            // Strict paused/scrubbing: display the latest frame; fast KEY_UNIT frames are allowed
            // and will be replaced by an accurate frame when it arrives.
            newest
        } else {
            // Responsive paused/scrubbing: prefer close frame; otherwise show newest as fallback.
            match newest {
                Some(f) if (f.pts - media_t).abs() <= tol => Some(f),
                other => other,
            }
        };

        if let Some(frame_out) = picked {
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
                let cooldown_ok = self
                    .last_play_reanchor_time
                    .map(|t| t.elapsed().as_millis() >= 150)
                    .unwrap_or(true);
                if dt > (0.5 * frame_dur).max(0.015) && cooldown_ok {
                    let _ = self.decode_mgr.send_cmd(
                        &active_path,
                        DecodeCmd::Play { start_pts: media_t, rate: self.engine.rate },
                    );
                    self.last_play_reanchor_time = Some(std::time::Instant::now());
                }
            }
            if let FramePayload::Cpu { y, uv } = &frame_out.payload {
                if let Some(rs) = frame.wgpu_render_state() {
                    let mut renderer = rs.renderer.write();
                    let slot = self.preview.ensure_stream_slot(
                        &rs.device,
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
                            upload_plane(
                                &rs.queue,
                                &**out_tex,
                                &rgba,
                                frame_out.props.w,
                                frame_out.props.h,
                                (frame_out.props.w as usize) * 4,
                                4,
                            );
                            if let Some(id) = slot.egui_tex_id {
                                renderer.update_egui_texture_from_wgpu_texture(
                                    &rs.device,
                                    out_view,
                                    eframe::wgpu::FilterMode::Linear,
                                    id,
                                );
                                let uv_rect = egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                );
                                painter.image(id, rect, uv_rect, egui::Color32::WHITE);
                                trace!("preview presented frame");
                            }
                        }
                    }
                }
            }
        } else {
            // Center placeholder (include elapsed if available)
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

        // Lightweight debug overlay: resolved source path and media time, plus lock indicator
        // Determine displayed pts if any
        let latest = self.decode_mgr.take_latest(&active_path);
        let displayed_pts = latest.as_ref().map(|f| f.pts);
        let displayed_approx = latest.as_ref().map(|f| !f.accurate).unwrap_or(false);
        let diff = displayed_pts.map(|p| (p - media_t).abs());
        let locked = diff.map(|d| d <= (0.5 * frame_dur).max(0.015)).unwrap_or(false);
        let overlay = format!(
            "src: {}\nmedia_t: {:.3}s  state: {:?}  {}\nlock: {}{}",
            std::path::Path::new(&active_path)
                .file_name()
                .and_then(|s| Some(s.to_string_lossy().to_string()))
                .unwrap_or(active_path.clone()),
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
    }
}
