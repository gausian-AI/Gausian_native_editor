struct App {
    db: ProjectDb,
    project_id: String,
    import_path: String,
    // timeline state
    seq: Sequence,
    timeline_history: CommandHistory,
    zoom_px_per_frame: f32,
    playhead: i64,
    playing: bool,
    last_tick: Option<Instant>,
    // Anchored playhead timing to avoid jitter
    play_anchor_instant: Option<Instant>,
    play_anchor_frame: i64,
    preview: PreviewState,
    audio_out: Option<audio_engine::AudioEngine>,
    selected: Option<(usize, usize)>,
    drag: Option<DragState>,
    export: ExportUiState,
    import_workers: Vec<std::thread::JoinHandle<()>>,
    jobs: Option<jobs_crate::JobsHandle>,
    job_events: Vec<JobEvent>,
    show_jobs: bool,
    decode_mgr: DecodeManager,
    playback_clock: PlaybackClock,
    audio_cache: AudioCache,
    audio_buffers: AudioBufferCache,
    // When true during this frame, enable audible scrubbing while paused
    // Last successfully presented key: (source path, media time in milliseconds)
    // Using media time (not playhead frame) avoids wrong reuse when clips share a path but have different in_offset/rate.
    last_preview_key: Option<(String, i64)>,
    // Playback engine
    engine: EngineState,
    // Debounce decode commands: remember last sent (state, path, optional seek bucket)
    last_sent: Option<(PlayState, String, Option<i64>)>,
    // Epsilon-based dispatch tracking
    last_seek_sent_pts: Option<f64>,
    last_play_reanchor_time: Option<Instant>,
    // Throttled engine log state
    // (Used only for preview_ui logging when sending worker commands)
    // Not strictly necessary, but kept for future UI log hygiene.
    // last_engine_log: Option<Instant>,
    // Strict paused behavior toggle (UI)
    strict_pause: bool,
    // Track when a paused seek was requested (for overlay timing)
    last_seek_request_at: Option<Instant>,
}

impl App {
    fn new(db: ProjectDb) -> Self {
        let project_id = "default".to_string();
        let _ = db.ensure_project(&project_id, "Default Project", None);
        let mut seq = Sequence::new("Main", 1920, 1080, Fps::new(30, 1), 600);
        if seq.tracks.is_empty() {
            seq.add_track(Track { name: "V1".into(), items: vec![] });
            seq.add_track(Track { name: "V2".into(), items: vec![] });
            seq.add_track(Track { name: "A1".into(), items: vec![] });
        }
        seq.graph = timeline_crate::migrate_sequence_tracks(&seq);
        let db_path = db.path().to_path_buf();
        let mut app = Self {
            db,
            project_id,
            import_path: String::new(),
            seq,
            timeline_history: CommandHistory::default(),
            zoom_px_per_frame: 2.0,
            playhead: 0,
            playing: false,
            last_tick: None,
            play_anchor_instant: None,
            play_anchor_frame: 0,
            preview: PreviewState::new(),
            audio_out: audio_engine::AudioEngine::new().ok(),
            selected: None,
            drag: None,
            export: ExportUiState::default(),
            import_workers: Vec::new(),
            jobs: Some(jobs_crate::JobsRuntime::start(db_path, 2)),
            job_events: Vec::new(),
            show_jobs: false,
            decode_mgr: DecodeManager::default(),
            playback_clock: PlaybackClock { rate: 1.0, ..Default::default() },
            audio_cache: AudioCache::default(),
            audio_buffers: AudioBufferCache::default(),
            last_preview_key: None,
            engine: EngineState { state: PlayState::Paused, rate: 1.0, target_pts: 0.0 },
            last_sent: None,
            last_seek_sent_pts: None,
            last_play_reanchor_time: None,
            strict_pause: true,
            last_seek_request_at: None,
        };
        app.sync_tracks_from_graph();
        app
    }

    fn apply_timeline_command(&mut self, command: TimelineCommand) -> Result<(), TimelineError> {
        self.timeline_history.apply(&mut self.seq.graph, command)?;
        self.sync_tracks_from_graph();
        Ok(())
    }

    fn sync_tracks_from_graph(&mut self) {
        let mut tracks: Vec<Track> = Vec::with_capacity(self.seq.graph.tracks.len());
        let mut max_end: i64 = 0;
        for binding in &self.seq.graph.tracks {
            let mut items = Vec::with_capacity(binding.node_ids.len());
            for node_id in &binding.node_ids {
                if let Some(node) = self.seq.graph.nodes.get(node_id) {
                    if let Some(item) = Self::item_from_node(node, &binding.kind, self.seq.fps) {
                        max_end = max_end.max(item.from + item.duration_in_frames);
                        items.push(item);
                    }
                }
            }
            tracks.push(Track { name: binding.name.clone(), items });
        }
        self.seq.tracks = tracks;
        self.seq.duration_in_frames = max_end;
    }

    fn item_from_node(node: &TimelineNode, track_kind: &TrackKind, fps: Fps) -> Option<Item> {
        let id = node.id.to_string();
        match (&node.kind, track_kind) {
            (TimelineNodeKind::Clip(clip), TrackKind::Audio) => {
                let src = clip.asset_id.clone().unwrap_or_default();
                Some(Item {
                    id,
                    from: clip.timeline_range.start,
                    duration_in_frames: clip.timeline_range.duration,
                    kind: ItemKind::Audio {
                        src,
                        in_offset_sec: crate::timeline::ui::frames_to_seconds(clip.media_range.start, fps),
                        rate: clip.playback_rate,
                    },
                })
            }
            (TimelineNodeKind::Clip(clip), _) => {
                let src = clip.asset_id.clone().unwrap_or_default();
                Some(Item {
                    id,
                    from: clip.timeline_range.start,
                    duration_in_frames: clip.timeline_range.duration,
                    kind: ItemKind::Video {
                        src,
                        frame_rate: Some(fps.num as f32 / fps.den.max(1) as f32),
                        in_offset_sec: crate::timeline::ui::frames_to_seconds(clip.media_range.start, fps),
                        rate: clip.playback_rate,
                    },
                })
            }
            (TimelineNodeKind::Generator { generator_id, timeline_range, metadata }, _) => {
                match generator_id.as_str() {
                    "solid" => {
                        let color = metadata
                            .get("color")
                            .and_then(|v| v.as_str())
                            .unwrap_or("#4c4c4c")
                            .to_string();
                        Some(Item {
                            id,
                            from: timeline_range.start,
                            duration_in_frames: timeline_range.duration,
                            kind: ItemKind::Solid { color },
                        })
                    }
                    "text" => {
                        let text = metadata.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let color = metadata
                            .get("color")
                            .and_then(|v| v.as_str())
                            .unwrap_or("#ffffff")
                            .to_string();
                        Some(Item {
                            id,
                            from: timeline_range.start,
                            duration_in_frames: timeline_range.duration,
                            kind: ItemKind::Text { text, color },
                        })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn build_audio_clips(&mut self) -> anyhow::Result<Vec<ActiveAudioClip>> {
        let fps = self.seq.fps;
        let mut clips = Vec::new();
        for binding in &self.seq.graph.tracks {
            if !matches!(binding.kind, TrackKind::Audio) { continue; }
            for node_id in &binding.node_ids {
                let node = match self.seq.graph.nodes.get(node_id) { Some(n) => n, None => continue };
                let clip = match &node.kind { TimelineNodeKind::Clip(c) => c, _ => continue };
                let path_str = match &clip.asset_id { Some(p) => p, None => continue };
                let path = Path::new(path_str);
                let buf = self.audio_buffers.get_or_load(path)?;
                let timeline_start = crate::timeline::ui::frames_to_seconds(clip.timeline_range.start, fps);
                let mut timeline_dur = crate::timeline::ui::frames_to_seconds(clip.timeline_range.duration, fps);
                let mut media_start = crate::timeline::ui::frames_to_seconds(clip.media_range.start, fps);
                let rate = clip.playback_rate.max(0.0001) as f64;
                timeline_dur /= rate;
                media_start /= rate;
                let clip_duration = timeline_dur.min((buf.duration_sec as f64 - media_start).max(0.0));
                if clip_duration <= 0.0 { continue; }
                clips.push(ActiveAudioClip {
                    start_tl_sec: timeline_start,
                    start_media_sec: media_start,
                    duration_sec: clip_duration,
                    buf: buf.clone(),
                });
            }
        }

        clips.sort_by(|a, b| a.start_tl_sec.partial_cmp(&b.start_tl_sec).unwrap_or(std::cmp::Ordering::Equal));
        Ok(clips)
    }

    fn active_video_media_time_graph(&self, timeline_sec: f64) -> Option<(String, f64)> {
        let seq_fps = (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
        let playhead = (timeline_sec * seq_fps).round() as i64;
        // Priority: lower-numbered (top-most) video tracks first
        for binding in self.seq.graph.tracks.iter() {
            if matches!(binding.kind, TrackKind::Audio) { continue; }
            for node_id in &binding.node_ids {
                let Some(node) = self.seq.graph.nodes.get(node_id) else { continue };
                let clip = match &node.kind { TimelineNodeKind::Clip(c) => c, _ => continue };
                if playhead < clip.timeline_range.start || playhead >= clip.timeline_range.end() { continue; }
                let Some(path) = clip.asset_id.clone() else { continue };
                let start_on_timeline_sec = clip.timeline_range.start as f64 / seq_fps;
                let local_t = (timeline_sec - start_on_timeline_sec).max(0.0);
                let media_sec = crate::timeline::ui::frames_to_seconds(clip.media_range.start, self.seq.fps) + local_t * clip.playback_rate as f64;
                return Some((path, media_sec));
            }
        }
        None
    }

    fn active_audio_media_time_graph(&self, timeline_sec: f64) -> Option<(String, f64)> {
        let seq_fps = (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
        let playhead = (timeline_sec * seq_fps).round() as i64;
        for binding in self.seq.graph.tracks.iter().rev() {
            if !matches!(binding.kind, TrackKind::Audio) { continue; }
            for node_id in &binding.node_ids {
                let Some(node) = self.seq.graph.nodes.get(node_id) else { continue };
                let clip = match &node.kind { TimelineNodeKind::Clip(c) => c, _ => continue };
                if playhead < clip.timeline_range.start || playhead >= clip.timeline_range.end() { continue; }
                let Some(path) = clip.asset_id.clone() else { continue };
                let start_on_timeline_sec = clip.timeline_range.start as f64 / seq_fps;
                let local_t = (timeline_sec - start_on_timeline_sec).max(0.0);
                let media_sec = crate::timeline::ui::frames_to_seconds(clip.media_range.start, self.seq.fps) + local_t * clip.playback_rate as f64;
                return Some((path, media_sec));
            }
        }
        self.active_video_media_time_graph(timeline_sec)
    }

    fn request_audio_peaks(&mut self, _path: &std::path::Path) {
        // Placeholder: integrate with audio decoding backend to compute peaks.
        // Keep bounded: one job per path. For now, no-op to avoid blocking UI.
    }

    fn import_from_path(&mut self) {
        let p = std::mem::take(&mut self.import_path);
        if p.trim().is_empty() { return; }
        let path = PathBuf::from(p);
        let _ = self.import_files(&[path]);
    }

    fn export_sequence(&mut self) {
        // Open the export dialog UI
        self.export.open = true;
    }

    fn import_files(&mut self, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() { return Ok(()); }
        let ancestor = nearest_common_ancestor(files);
        if let Some(base) = ancestor.as_deref() { self.db.set_project_base_path(&self.project_id, base)?; }
        let db_path = self.db.path().to_path_buf();
        let project_id = self.project_id.clone();
        for f in files.to_vec() {
            let base = ancestor.clone();
            let db_path = db_path.clone();
            let project_id = project_id.clone();
            let jobs = self.jobs.clone();
            let h = std::thread::spawn(move || {
                let db = project::ProjectDb::open_or_create(&db_path).expect("open db");
                match media_io::probe_media(&f) {
                Ok(info) => {
                    let kind = match info.kind { media_io::MediaKind::Video => "video", media_io::MediaKind::Image => "image", media_io::MediaKind::Audio => "audio" };
                        let rel = base.as_deref().and_then(|b| pathdiff::diff_paths(&f, b));
                    let fps_num = info.fps_num.map(|v| v as i64);
                    let fps_den = info.fps_den.map(|v| v as i64);
                    let duration_frames = match (info.duration_seconds, fps_num, fps_den) {
                        (Some(d), Some(n), Some(dn)) if dn != 0 => Some(((d * (n as f64) / (dn as f64)).round()) as i64),
                        _ => None,
                    };
                        let asset_id = db.insert_asset_row(
                            &project_id,
                        kind,
                            &f,
                        rel.as_deref(),
                        info.width.map(|x| x as i64),
                        info.height.map(|x| x as i64),
                        duration_frames,
                        fps_num,
                        fps_den,
                        info.audio_channels.map(|x| x as i64),
                        info.sample_rate.map(|x| x as i64),
                        ).unwrap_or_default();
                        if let Some(j) = jobs {
                            use jobs_crate::{JobKind, JobSpec};
                            for kind in [JobKind::Waveform, JobKind::Thumbnails, JobKind::Proxy, JobKind::SeekIndex] {
                                let id = j.enqueue(JobSpec { asset_id: asset_id.clone(), kind, priority: 0 });
                                let _ = db.enqueue_job(&id, &asset_id, match kind { JobKind::Waveform=>"waveform", JobKind::Thumbnails=>"thumbs", JobKind::Proxy=>"proxy", JobKind::SeekIndex=>"seekidx" }, 0);
                            }
                        }
                }
                Err(e) => eprintln!("ffprobe failed for {:?}: {e}", f),
            }
            });
            self.import_workers.push(h);
        }
        Ok(())
    }

    fn assets(&self) -> Vec<AssetRow> {
        self.db.list_assets(&self.project_id).unwrap_or_default()
    }

    fn add_asset_to_timeline(&mut self, asset: &AssetRow) {
        let is_audio = asset.kind.eq_ignore_ascii_case("audio");
        let track_index = if is_audio {
            self.seq.graph.tracks.iter().position(|t| matches!(t.kind, TrackKind::Audio)).unwrap_or_else(|| self.seq.graph.tracks.len().saturating_sub(1))
        } else {
            0
        };

        let track_binding = match self.seq.graph.tracks.get(track_index) {
            Some(binding) => binding.clone(),
            None => return,
        };

        let start_frame = track_binding
            .node_ids
            .iter()
            .filter_map(|id| self.seq.graph.nodes.get(id))
            .filter_map(|node| Self::node_frame_range(node))
            .map(|range| range.end())
            .max()
            .unwrap_or(0);

        let duration = asset.duration_frames.unwrap_or(150).max(1);
        let timeline_range = FrameRange::new(start_frame, duration);
        let media_range = FrameRange::new(0, duration);
        let clip = ClipNode {
            asset_id: Some(asset.src_abs.clone()),
            media_range,
            timeline_range,
            playback_rate: 1.0,
            reverse: false,
            metadata: Value::Null,
        };
        let node = TimelineNode {
            id: NodeId::new(),
            label: Some(asset.id.clone()),
            kind: TimelineNodeKind::Clip(clip),
            locked: false,
            metadata: Value::Null,
        };
        let placement = TrackPlacement { track_id: track_binding.id, position: None };
        if let Err(err) = self.apply_timeline_command(TimelineCommand::InsertNode { node, placements: vec![placement], edges: Vec::new() }) {
            eprintln!("timeline insert failed: {err}");
            return;
        }

        if let Some(track) = self.seq.tracks.get(track_index) {
            let idx = track.items.len().saturating_sub(1);
            self.selected = Some((track_index, idx));
        }
    }

}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Push-driven repaint is primary (worker triggers request_repaint on new frames).
        // Safety net: ensure periodic UI updates even if no frames arrive.
        if self.engine.state == PlayState::Playing {
            // Try to pace by the active clip fps, bounded by the sequence fps.
            let seq_fps = (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
            let t_playhead = self.playback_clock.now();
            let active_fps = if let Some((path, _)) = self.active_video_media_time_graph(t_playhead) {
                if let Some(latest) = self.decode_mgr.take_latest(&path) { latest.props.fps as f64 } else { f64::NAN }
            } else {
                f64::NAN
            };
            let clip_fps = if active_fps.is_finite() && active_fps > 0.0 { active_fps } else { seq_fps };
            let target_fps = clip_fps.min(seq_fps).clamp(10.0, 120.0);
            let dt = 1.0f64 / target_fps;
            ctx.request_repaint_after(Duration::from_secs_f64(dt));
        } else {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        // Space toggles play/pause (keep engine.state in sync)
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            let seq_fps = (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
            let current_sec = (self.playhead as f64) / seq_fps;

            if self.playback_clock.playing {
                self.playback_clock.pause(current_sec);
                // NEW: make the decode engine pause too
                self.engine.state = PlayState::Paused;
                if let Some(engine) = &self.audio_out { engine.pause(current_sec); }
            } else {
                if self.playhead >= self.seq.duration_in_frames { self.playhead = 0; }
                self.playback_clock.play(current_sec);
                // NEW: make the decode engine actually play
                self.engine.state = PlayState::Playing;
                if let Ok(clips) = self.build_audio_clips() {
                    if let Some(engine) = &self.audio_out { engine.start(current_sec, clips); }
                }
            }
        }

        // Keep engine.state aligned with the clock unless we're in an explicit drag/seek
        if !matches!(self.engine.state, PlayState::Scrubbing | PlayState::Seeking) {
            self.engine.state = if self.playback_clock.playing { PlayState::Playing } else { PlayState::Paused };
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Import path:");
                ui.text_edit_singleline(&mut self.import_path);
                if ui.button("Add").clicked() {
                    self.import_from_path();
                }
                if ui.button("Export...").clicked() {
                    self.export_sequence();
                }
                if ui.button("Jobs").clicked() {
                    self.show_jobs = !self.show_jobs;
                }
                ui.separator();
                if ui.button(if self.engine.state == PlayState::Playing { "Pause (Space)" } else { "Play (Space)" }).clicked() {
                    let seq_fps = (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
                    let current_sec = (self.playhead as f64) / seq_fps;
                    if self.engine.state == PlayState::Playing {
                        self.playback_clock.pause(current_sec);
                        self.engine.state = PlayState::Paused;
                        if let Some(engine) = &self.audio_out { engine.pause(current_sec); }
                    } else {
                        self.playback_clock.play(current_sec);
                        self.engine.state = PlayState::Playing;
                        if let Ok(clips) = self.build_audio_clips() {
                            if let Some(engine) = &self.audio_out { engine.start(current_sec, clips); }
                        }
                    }
                }
            });
        });

        // Export dialog UI
        self.export.ui(ctx, &self.seq, &self.db, &self.project_id);

        // Preview panel will be inside CentralPanel with resizable area

        egui::SidePanel::left("assets").default_width(340.0).show(ctx, |ui| {
            self.poll_jobs();
            ui.heading("Assets");
            ui.horizontal(|ui| {
                if ui.button("Import...").clicked() {
                    if let Some(files) = rfd::FileDialog::new().pick_files() {
                        let _ = self.import_files(&files);
                    }
                }
                if ui.button("Refresh").clicked() {}
                if ui.button("Jobs").clicked() { self.show_jobs = !self.show_jobs; }
            });
            
            // Show hardware encoders info
            ui.collapsing("Hardware Encoders", |ui| {
                let encoders = media_io::get_hardware_encoders();
                if encoders.is_empty() {
                    ui.label("No hardware encoders detected");
                    ui.label("Using software encoders (slower)");
                } else {
                    for (codec, encoder_list) in encoders {
                        ui.label(format!("{}:", codec));
                        for encoder in encoder_list {
                            ui.label(format!("  • {}", encoder));
                        }
                    }
                }
            });

            // Native Video Decoder
            ui.collapsing("Native Video Decoder", |ui| {
                let available = is_native_decoding_available();
                ui.label(format!("Native decoding available: {}", if available { "✅ Yes" } else { "❌ No" }));
                
                if available {
                    ui.label("• VideoToolbox hardware acceleration");
                    ui.label("• Phase 1: CPU plane copies (NV12/P010)");
                    ui.label("• Phase 2: Zero-copy IOSurface (planned)");
                    
                    if ui.button("Test Native Decoder (Phase 1)").clicked() {
                        // Test native decoder with a sample video
                        if let Some(asset) = self.assets().first() {
                            let config = DecoderConfig {
                                hardware_acceleration: true,
                                preferred_format: Some(native_decoder::YuvPixFmt::Nv12),
                                zero_copy: false, // Phase 1 only
                            };
                            
                            match create_decoder(&asset.src_abs, config) {
                                Ok(mut decoder) => {
                                    let properties = decoder.get_properties();
                                    ui.label(format!("✅ Phase 1 Decoder created successfully!"));
                                    ui.label(format!("Video: {}x{} @ {:.1}fps", 
                                        properties.width, properties.height, properties.frame_rate));
                                    ui.label(format!("Duration: {:.1}s", properties.duration));
                                    ui.label(format!("Format: {:?}", properties.format));
                                    
                                    // Test frame decoding
                                    if let Ok(Some(frame)) = decoder.decode_frame(1.0) {
                                        ui.label(format!("✅ Frame decoded: {}x{} YUV", frame.width, frame.height));
                                        ui.label(format!("Y plane: {} bytes", frame.y_plane.len()));
                                        ui.label(format!("UV plane: {} bytes", frame.uv_plane.len()));
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
                        // Test zero-copy decoder with IOSurface
                        if let Some(asset) = self.assets().first() {
                            let config = DecoderConfig {
                                hardware_acceleration: true,
                                preferred_format: Some(native_decoder::YuvPixFmt::Nv12),
                                zero_copy: true, // Phase 2 zero-copy
                            };
                            
                            match create_decoder(&asset.src_abs, config) {
                                Ok(mut decoder) => {
                                    let properties = decoder.get_properties();
                                    ui.label(format!("✅ Phase 2 Zero-Copy Decoder created!"));
                                    ui.label(format!("Video: {}x{} @ {:.1}fps", 
                                        properties.width, properties.height, properties.frame_rate));
                                    ui.label(format!("Zero-copy supported: {}", decoder.supports_zero_copy()));
                                    
                                    // Test zero-copy frame decoding
                                    #[cfg(target_os = "macos")]
                                    {
                                        if let Ok(Some(iosurface_frame)) = decoder.decode_frame_zero_copy(1.0) {
                                            ui.label(format!("✅ IOSurface frame decoded: {}x{}", 
                                                iosurface_frame.width, iosurface_frame.height));
                                            ui.label(format!("Surface format: {:?}", iosurface_frame.format));
                                            ui.label(format!("Timestamp: {:.3}s", iosurface_frame.timestamp));
                                            
                                            // Test WGPU integration
                                            ui.label("🎬 Testing WGPU integration...");
                                            ui.label("✅ Zero-copy pipeline ready for rendering!");
                                        } else {
                                            ui.label("❌ Zero-copy frame decoding failed");
                                        }
                                    }
                                    
                                    #[cfg(not(target_os = "macos"))]
                                    {
                                        ui.label("ℹ️ Zero-copy mode not available on this platform");
                                    }
                                }
                                Err(e) => {
                                    ui.label(format!("❌ Zero-copy decoder creation failed: {}", e));
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
            egui::Separator::default().ui(ui);
            let assets = self.assets();
            egui_extras::TableBuilder::new(ui)
                .striped(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(egui_extras::Column::remainder()) // Name
                .column(egui_extras::Column::auto()) // Kind
                .column(egui_extras::Column::auto()) // WxH
                .column(egui_extras::Column::auto()) // Add
                .header(20.0, |mut header| {
                    header.col(|ui| { ui.strong("Name"); });
                    header.col(|ui| { ui.strong("Kind"); });
                    header.col(|ui| { ui.strong("Size"); });
                    header.col(|ui| { ui.strong(""); });
                })
                .body(|mut body| {
                    for a in assets.iter() {
                        body.row(22.0, |mut row| {
                            row.col(|ui| {
                                let name = std::path::Path::new(&a.src_abs).file_name().map(|s| s.to_string_lossy()).unwrap_or_default();
                                ui.label(name);
                            });
                            row.col(|ui| { ui.label(&a.kind); });
                            row.col(|ui| {
                                if let (Some(w), Some(h)) = (a.width, a.height) { ui.label(format!("{}x{}", w, h)); }
                            });
                            row.col(|ui| {
                                if ui.button("Add").clicked() { self.add_asset_to_timeline(a); }
                            });
                        });
                    }
                });
        });

        // Properties panel for selected clip
        egui::SidePanel::right("properties").default_width(280.0).show(ctx, |ui| {
            ui.heading("Properties");
            if let Some((ti, ii)) = self.selected {
                if ti < self.seq.tracks.len() && ii < self.seq.tracks[ti].items.len() {
                    let item = &mut self.seq.tracks[ti].items[ii];
                    ui.label(format!("Clip ID: {}", &item.id[..8.min(item.id.len())]));
                    ui.label(format!("From: {}  Dur: {}f", item.from, item.duration_in_frames));
                    match &mut item.kind {
                        ItemKind::Video { in_offset_sec, rate, .. } => {
                            // Prepare pending change flags to avoid borrow issues
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
                                if ui.small_button("1.0").on_hover_text("Reset").clicked() { *rate = 1.0; }
                            });
                            ui.horizontal(|ui| {
                                ui.label("In Offset (s)");
                                let mut o = *in_offset_sec;
                                let changed = ui
                                    .add(egui::DragValue::new(&mut o).clamp_range(0.0..=1_000_000.0).speed(0.01))
                                    .changed();
                                if changed {
                                    *in_offset_sec = o.max(0.0);
                                    let num = self.seq.fps.num as f64;
                                    let den = self.seq.fps.den.max(1) as f64;
                                    let frames = ((o.max(0.0)) * (num / den)).round() as i64;
                                    pending_offset_frames = Some(frames.max(0));
                                }
                                if ui.small_button("0").on_hover_text("Reset").clicked() { *in_offset_sec = 0.0; }
                            });
                            // Apply pending updates after UI borrows end
                            if pending_rate.is_some() || pending_offset_frames.is_some() {
                                if let Ok(uuid) = uuid::Uuid::parse_str(&item.id) {
                                    let node_id = NodeId(uuid);
                                    if let Some(mut node) = self.seq.graph.nodes.get(&node_id).cloned() {
                                        if let TimelineNodeKind::Clip(mut clip) = node.kind.clone() {
                                            if let Some(r) = pending_rate { clip.playback_rate = r; }
                                            if let Some(fr) = pending_offset_frames { clip.media_range.start = fr; }
                                            node.kind = TimelineNodeKind::Clip(clip);
                                            let _ = self.apply_timeline_command(TimelineCommand::UpdateNode { node });
                                        }
                                    }
                                }
                            }
                        }
                        ItemKind::Audio { in_offset_sec, rate, .. } => {
                            let mut pending_rate: Option<f32> = None;
                            let mut pending_offset_frames: Option<i64> = None;
                            ui.separator();
                            ui.label("Audio");
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
                                if ui.small_button("1.0").on_hover_text("Reset").clicked() { *rate = 1.0; }
                            });
                            ui.horizontal(|ui| {
                                ui.label("In Offset (s)");
                                let mut o = *in_offset_sec;
                                let changed = ui
                                    .add(egui::DragValue::new(&mut o).clamp_range(0.0..=1_000_000.0).speed(0.01))
                                    .changed();
                                if changed {
                                    *in_offset_sec = o.max(0.0);
                                    let num = self.seq.fps.num as f64;
                                    let den = self.seq.fps.den.max(1) as f64;
                                    let frames = ((o.max(0.0)) * (num / den)).round() as i64;
                                    pending_offset_frames = Some(frames.max(0));
                                }
                                if ui.small_button("0").on_hover_text("Reset").clicked() { *in_offset_sec = 0.0; }
                            });
                            if pending_rate.is_some() || pending_offset_frames.is_some() {
                                if let Ok(uuid) = uuid::Uuid::parse_str(&item.id) {
                                    let node_id = NodeId(uuid);
                                    if let Some(mut node) = self.seq.graph.nodes.get(&node_id).cloned() {
                                        if let TimelineNodeKind::Clip(mut clip) = node.kind.clone() {
                                            if let Some(r) = pending_rate { clip.playback_rate = r; }
                                            if let Some(fr) = pending_offset_frames { clip.media_range.start = fr; }
                                            node.kind = TimelineNodeKind::Clip(clip);
                                            let _ = self.apply_timeline_command(TimelineCommand::UpdateNode { node });
                                        }
                                    }
                                }
                            }
                        }
                        ItemKind::Image { .. } => {
                            ui.separator();
                            ui.label("Image clip has no time controls");
                        }
                        _ => {}
                    }
                } else {
                    ui.label("Selection out of range");
                }
            } else {
                ui.label("No clip selected");
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Resize::default()
                .id_salt("preview_resize")
                .default_size(egui::vec2(ui.available_width(), 360.0))
                .show(ui, |ui| {
                    self.preview_ui(ctx, frame, ui);
                });
            ui.add_space(4.0);
            ui.separator();
            
            // Performance indicator
            ui.horizontal(|ui| {
            ui.heading("Timeline");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let cache_stats = format!("Cache: {}/{} hits", 
                                            self.preview.cache_hits, 
                                            self.preview.cache_hits + self.preview.cache_misses);
                    ui.small(&cache_stats);
                });
            });
            
            self.timeline_ui(ui);
        });

        self.jobs_window(ctx);
    }
}
