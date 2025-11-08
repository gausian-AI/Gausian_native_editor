use std::path::{Path, PathBuf};

use eframe::egui;
use project::AssetRow;
use serde_json::{json, Map, Value};

use super::App;

fn asset_timeline_duration(asset: &AssetRow, fps: timeline_crate::Fps) -> i64 {
    let seq_fps = {
        let num = fps.num.max(1) as f64;
        let den = fps.den.max(1) as f64;
        if den <= 0.0 {
            return asset.duration_frames.unwrap_or(150).max(1);
        }
        num / den
    };

    if seq_fps <= 0.0 {
        return asset.duration_frames.unwrap_or(150).max(1);
    }

    if let (Some(frames), Some(num), Some(den)) =
        (asset.duration_frames, asset.fps_num, asset.fps_den)
    {
        if num > 0 && den > 0 {
            let asset_fps = (num as f64) / (den as f64);
            if asset_fps > 0.0 {
                let timeline_frames = ((frames as f64 / asset_fps) * seq_fps).round() as i64;
                return timeline_frames.max(1);
            }
        }
    }

    let timeline_frames = asset
        .duration_seconds()
        .map(|sec| (sec * seq_fps).round() as i64)
        .filter(|v| *v > 0)
        .or_else(|| asset.duration_frames.map(|v| v.max(1)))
        .unwrap_or(150);

    timeline_frames.max(1)
}

fn track_end(graph: &timeline_crate::TimelineGraph, binding: &timeline_crate::TrackBinding) -> i64 {
    binding
        .node_ids
        .iter()
        .filter_map(|id| graph.nodes.get(id))
        .filter_map(|node| match &node.kind {
            timeline_crate::TimelineNodeKind::Clip(clip) => Some(clip.timeline_range.end()),
            timeline_crate::TimelineNodeKind::Generator { timeline_range, .. } => {
                Some(timeline_range.end())
            }
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn nearest_track_by_kind<F>(
    app: &App,
    preferred: Option<usize>,
    predicate: F,
) -> Option<(usize, timeline_crate::TrackBinding)>
where
    F: Fn(&timeline_crate::TrackKind) -> bool,
{
    let pref = preferred.unwrap_or(0) as isize;
    app.seq
        .graph
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| predicate(&track.kind))
        .min_by_key(|(idx, _)| ((*idx as isize - pref).abs() as usize))
        .map(|(idx, track)| (idx, track.clone()))
}

fn collect_target_tracks(
    app: &App,
    asset: &AssetRow,
    preferred_track: Option<usize>,
) -> Vec<(usize, timeline_crate::TrackBinding)> {
    let mut targets = Vec::new();

    if asset.kind.eq_ignore_ascii_case("audio") {
        if let Some(audio_track) = nearest_track_by_kind(app, preferred_track, |kind| {
            matches!(kind, timeline_crate::TrackKind::Audio)
        }) {
            targets.push(audio_track);
        } else if let Some(idx) = preferred_track {
            if let Some(track) = app.seq.graph.tracks.get(idx) {
                targets.push((idx, track.clone()));
            }
        }
        return targets;
    }

    // Primary video (or generic) track
    let video_track = nearest_track_by_kind(app, preferred_track, |kind| {
        !matches!(kind, timeline_crate::TrackKind::Audio)
    })
    .or_else(|| app.seq.graph.tracks.get(0).map(|track| (0, track.clone())));

    if let Some(v) = &video_track {
        targets.push(v.clone());
    }

    if asset.has_audio() {
        if let Some(audio_track) = nearest_track_by_kind(app, preferred_track, |kind| {
            matches!(kind, timeline_crate::TrackKind::Audio)
        }) {
            let video_track_id = video_track.as_ref().map(|(_, t)| t.id);
            if video_track_id != Some(audio_track.1.id) {
                targets.push(audio_track);
            }
        }
    }

    targets
}

pub(super) fn load_thumb_texture(
    app: &mut App,
    ctx: &egui::Context,
    asset: &project::AssetRow,
    desired_w: u32,
    desired_h: u32,
) -> Option<egui::TextureHandle> {
    if let Some(tex) = app.asset_thumb_textures.get(&asset.id) {
        return Some(tex.clone());
    }
    let thumb_path = project::app_data_dir()
        .join("cache")
        .join("thumbnails")
        .join(format!("{}-thumb.jpg", asset.id));
    if !thumb_path.exists() {
        return None;
    }
    if let Ok(img) = image::open(&thumb_path) {
        let resized = img.resize(desired_w, desired_h, image::imageops::FilterType::Triangle);
        let rgba = resized.to_rgba8();
        let (w, h) = rgba.dimensions();
        let color =
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba.into_raw());
        let tex = ctx.load_texture(
            format!("asset_thumb_{}", asset.id),
            color,
            egui::TextureOptions::LINEAR,
        );
        app.asset_thumb_textures
            .insert(asset.id.clone(), tex.clone());
        Some(tex)
    } else {
        None
    }
}

// Thin App method wrappers to keep app.rs small
impl App {
    // Load or retrieve cached thumbnail texture for an asset
    pub(crate) fn load_thumb_texture(
        &mut self,
        ctx: &egui::Context,
        asset: &project::AssetRow,
        desired_w: u32,
        desired_h: u32,
    ) -> Option<egui::TextureHandle> {
        self::load_thumb_texture(self, ctx, asset, desired_w, desired_h)
    }

    pub(crate) fn insert_asset_at(
        &mut self,
        asset: &project::AssetRow,
        track_index: usize,
        start_frame: i64,
    ) {
        self::insert_asset_at(self, asset, track_index, start_frame)
    }

    pub(crate) fn import_from_path(&mut self) {
        self::import_from_path(self)
    }

    pub(crate) fn export_sequence(&mut self) {
        self::export_sequence(self)
    }

    pub(crate) fn import_files(&mut self, files: &[std::path::PathBuf]) -> anyhow::Result<()> {
        self::import_files(self, files)
    }

    pub(crate) fn import_files_for(
        &mut self,
        project_id: &str,
        files: &[std::path::PathBuf],
    ) -> anyhow::Result<()> {
        self::import_files_for(self, project_id, files, None)
    }

    pub(crate) fn import_files_for_with_metadata(
        &mut self,
        project_id: &str,
        files: &[std::path::PathBuf],
        metadata: Option<std::collections::HashMap<std::path::PathBuf, serde_json::Value>>,
    ) -> anyhow::Result<()> {
        self::import_files_for(self, project_id, files, metadata)
    }

    pub(crate) fn assets(&self) -> Vec<AssetRow> {
        self::assets(self)
    }

    pub(crate) fn add_asset_to_timeline(&mut self, asset: &AssetRow) {
        self::add_asset_to_timeline(self, asset)
    }
}

pub(super) fn insert_asset_at(
    app: &mut App,
    asset: &project::AssetRow,
    track_index: usize,
    start_frame: i64,
) {
    let targets = collect_target_tracks(app, asset, Some(track_index));
    if targets.is_empty() {
        return;
    }

    let duration = asset_timeline_duration(asset, app.seq.fps);
    let timeline_range = timeline_crate::FrameRange::new(start_frame.max(0), duration);
    let media_frames = asset.duration_frames.unwrap_or(duration).max(1);
    let media_range = timeline_crate::FrameRange::new(0, media_frames);
    let metadata = clip_metadata_for_asset(asset);
    let clip = timeline_crate::ClipNode {
        asset_id: Some(asset.src_abs.clone()),
        media_range,
        timeline_range,
        playback_rate: 1.0,
        reverse: false,
        metadata,
    };
    let node = timeline_crate::TimelineNode {
        id: timeline_crate::NodeId::new(),
        label: Some(asset.id.clone()),
        kind: timeline_crate::TimelineNodeKind::Clip(clip),
        locked: false,
        metadata: serde_json::Value::Null,
    };
    let placements: Vec<_> = targets
        .iter()
        .map(|(_, binding)| timeline_crate::TrackPlacement {
            track_id: binding.id,
            position: None,
        })
        .collect();
    if let Err(err) = super::app_timeline::apply_timeline_command_impl(
        app,
        timeline_crate::TimelineCommand::InsertNode {
            node,
            placements,
            edges: Vec::new(),
        },
    ) {
        eprintln!("timeline insert failed: {err}");
        return;
    }
    super::app_timeline::sync_tracks_from_graph_impl(app);
}

pub(super) fn import_from_path(app: &mut App) {
    let p = std::mem::take(&mut app.import_path);
    if p.trim().is_empty() {
        return;
    }
    let path = PathBuf::from(p);
    let _ = import_files(app, &[path]);
}

pub(super) fn export_sequence(app: &mut App) {
    // Open the export dialog UI
    app.export.open = true;
}

pub(super) fn import_files(app: &mut App, files: &[PathBuf]) -> anyhow::Result<()> {
    let pid = app.project_id.clone();
    import_files_for(app, &pid, files, None)
}

pub(super) fn import_files_for(
    app: &mut App,
    project_id: &str,
    files: &[PathBuf],
    metadata: Option<std::collections::HashMap<PathBuf, serde_json::Value>>,
) -> anyhow::Result<()> {
    use anyhow::Result;
    if files.is_empty() {
        return Ok(());
    }
    let ancestor = super::nearest_common_ancestor(files);
    if let Some(base) = ancestor.as_deref() {
        app.db.set_project_base_path(project_id, base)?;
    }
    let metadata_map = std::sync::Arc::new(metadata.unwrap_or_default());
    let db_path = app.db.path().to_path_buf();
    let project_id = project_id.to_string();
    for f in files.to_vec() {
        let base = ancestor.clone();
        let db_path = db_path.clone();
        let project_id = project_id.clone();
        let metadata_map = metadata_map.clone();
        let h = std::thread::spawn(move || {
            let db = project::ProjectDb::open_or_create(&db_path).expect("open db");
            match crate::media_info::probe_media_info(&f) {
                Ok(info) => {
                    let kind = match info.kind {
                        crate::media_info::MediaKind::Video => "video",
                        crate::media_info::MediaKind::Image => "image",
                        crate::media_info::MediaKind::Audio => "audio",
                    };
                    let rel = base.as_deref().and_then(|b| pathdiff::diff_paths(&f, b));
                    let fps_num = info.fps_num.map(|v| v as i64);
                    let fps_den = info.fps_den.map(|v| v as i64);
                    let duration_frames = match (info.duration_seconds, fps_num, fps_den) {
                        (Some(d), Some(n), Some(dn)) if dn != 0 => {
                            Some(((d * (n as f64) / (dn as f64)).round()) as i64)
                        }
                        _ => None,
                    };
                    let comfy_meta = metadata_map.get(&f).cloned();
                    let meta_json = comfy_meta.map(|value| value.to_string());
                    let asset_id = db
                        .insert_asset_row(
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
                            info.duration_seconds,
                            info.codec.as_deref(),
                            info.bitrate_mbps,
                            info.bit_depth.map(|x| x as i64),
                            info.is_hdr,
                            info.is_variable_framerate,
                            meta_json.as_deref(),
                        )
                        .unwrap_or_default();
                }
                Err(err) => {
                    eprintln!("Import probe failed {}: {}", f.to_string_lossy(), err);
                }
            }
        });
        app.import_workers.push(h);
    }
    Ok(())
}

pub(super) fn assets(app: &App) -> Vec<project::AssetRow> {
    app.db.list_assets(&app.project_id).unwrap_or_default()
}

pub(super) fn add_asset_to_timeline(app: &mut App, asset: &project::AssetRow) {
    use crate::timeline_crate::{ClipNode, FrameRange, TimelineNode, TimelineNodeKind};
    let targets = collect_target_tracks(app, asset, None);
    if targets.is_empty() {
        return;
    }

    let start_frame = targets
        .iter()
        .map(|(_, binding)| track_end(&app.seq.graph, binding))
        .max()
        .unwrap_or(0);

    let duration = asset_timeline_duration(asset, app.seq.fps);
    let timeline_range = FrameRange::new(start_frame, duration);
    let media_frames = asset.duration_frames.unwrap_or(duration).max(1);
    let media_range = FrameRange::new(0, media_frames);
    let metadata = clip_metadata_for_asset(asset);
    let clip = ClipNode {
        asset_id: Some(asset.src_abs.clone()),
        media_range,
        timeline_range,
        playback_rate: 1.0,
        reverse: false,
        metadata,
    };
    let node = TimelineNode {
        id: timeline_crate::NodeId::new(),
        label: Some(asset.id.clone()),
        kind: TimelineNodeKind::Clip(clip),
        locked: false,
        metadata: serde_json::Value::Null,
    };
    let placements: Vec<_> = targets
        .iter()
        .map(|(_, binding)| timeline_crate::TrackPlacement {
            track_id: binding.id,
            position: None,
        })
        .collect();
    let _ = super::app_timeline::apply_timeline_command_impl(
        app,
        timeline_crate::TimelineCommand::InsertNode {
            node,
            placements,
            edges: Vec::new(),
        },
    );

    super::app_timeline::sync_tracks_from_graph_impl(app);

    let selected_track_index = targets
        .iter()
        .find(|(_, binding)| !matches!(binding.kind, timeline_crate::TrackKind::Audio))
        .or_else(|| targets.first())
        .map(|(idx, _)| *idx);

    if let Some(track_idx) = selected_track_index {
        if let Some(track) = app.seq.tracks.get(track_idx) {
            let item_idx = track.items.len().saturating_sub(1);
            app.selected = Some((track_idx, item_idx));
        }

        app.prime_asset_for_timeline(asset);
    }
}

fn clip_metadata_for_asset(asset: &AssetRow) -> Value {
    let mut map = Map::new();
    if let Some(num) = asset.fps_num {
        if num > 0 {
            map.insert("media_fps_num".to_string(), json!(num));
        }
    }
    if let Some(den) = asset.fps_den {
        if den > 0 {
            map.insert("media_fps_den".to_string(), json!(den));
        }
    }
    if let Some(frames) = asset.duration_frames {
        if frames > 0 {
            map.insert("media_duration_frames".to_string(), json!(frames));
        }
    }
    if map.is_empty() {
        Value::Null
    } else {
        Value::Object(map)
    }
}
