use std::path::Path;

use crate::audio_engine::ActiveAudioClip;
use crate::timeline_crate::{
    Fps, Item, ItemKind, TimelineCommand, TimelineError, TimelineNode, TimelineNodeKind, Track,
    TrackKind,
};
use serde_json::Value;

use super::app_project;
use super::App;

pub(super) fn apply_timeline_command_impl(
    app: &mut App,
    command: TimelineCommand,
) -> Result<(), TimelineError> {
    app.timeline_history.apply(&mut app.seq.graph, command)?;
    sync_tracks_from_graph_impl(app);
    // Autosave timeline after each edit (best-effort)
    let _ = app_project::save_project_timeline_impl(app);
    Ok(())
}

pub(super) fn sync_tracks_from_graph_impl(app: &mut App) {
    let mut tracks: Vec<Track> = Vec::with_capacity(app.seq.graph.tracks.len());
    let mut max_end: i64 = 0;
    for binding in &app.seq.graph.tracks {
        let mut items = Vec::with_capacity(binding.node_ids.len());
        for node_id in &binding.node_ids {
            if let Some(node) = app.seq.graph.nodes.get(node_id) {
                if let Some(item) = item_from_node_impl(node, &binding.kind, app.seq.fps) {
                    max_end = max_end.max(item.from + item.duration_in_frames);
                    items.push(item);
                }
            }
        }
        tracks.push(Track {
            name: binding.name.clone(),
            items,
        });
    }
    app.seq.tracks = tracks;
    app.seq.duration_in_frames = max_end;
}

pub(super) fn item_from_node_impl(
    node: &TimelineNode,
    track_kind: &TrackKind,
    fps: Fps,
) -> Option<Item> {
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
                    in_offset_sec: crate::timeline::ui::frames_to_seconds(
                        clip.media_range.start,
                        fps,
                    ),
                    rate: clip.playback_rate,
                },
            })
        }
        (TimelineNodeKind::Clip(clip), _) => {
            let src = clip.asset_id.clone().unwrap_or_default();
            let is_image = std::path::Path::new(&src)
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| {
                    s.eq_ignore_ascii_case("png")
                        || s.eq_ignore_ascii_case("jpg")
                        || s.eq_ignore_ascii_case("jpeg")
                        || s.eq_ignore_ascii_case("gif")
                        || s.eq_ignore_ascii_case("webp")
                        || s.eq_ignore_ascii_case("bmp")
                        || s.eq_ignore_ascii_case("tif")
                        || s.eq_ignore_ascii_case("tiff")
                        || s.eq_ignore_ascii_case("exr")
                })
                .unwrap_or(false);
            if is_image {
                Some(Item {
                    id,
                    from: clip.timeline_range.start,
                    duration_in_frames: clip.timeline_range.duration,
                    kind: ItemKind::Image { src },
                })
            } else {
                Some(Item {
                    id,
                    from: clip.timeline_range.start,
                    duration_in_frames: clip.timeline_range.duration,
                    kind: ItemKind::Video {
                        src,
                        frame_rate: Some(fps.num as f32 / fps.den.max(1) as f32),
                        in_offset_sec: crate::timeline::ui::frames_to_seconds(
                            clip.media_range.start,
                            fps,
                        ),
                        rate: clip.playback_rate,
                    },
                })
            }
        }
        (
            TimelineNodeKind::Generator {
                generator_id,
                timeline_range,
                metadata,
            },
            _,
        ) => match generator_id.as_str() {
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
                let text = metadata
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
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
        },
        _ => None,
    }
}

pub(super) fn build_audio_clips_impl(app: &mut App) -> anyhow::Result<Vec<ActiveAudioClip>> {
    let seq_fps = app.seq.fps;
    let mut clips = Vec::new();
    for binding in &app.seq.graph.tracks {
        if !matches!(binding.kind, TrackKind::Audio) {
            continue;
        }
        for node_id in &binding.node_ids {
            let node = match app.seq.graph.nodes.get(node_id) {
                Some(n) => n,
                None => continue,
            };
            let clip = match &node.kind {
                TimelineNodeKind::Clip(c) => c,
                _ => continue,
            };
            let path_str = match &clip.asset_id {
                Some(p) => p,
                None => continue,
            };
            let path = Path::new(path_str);
            let buf = app.audio_buffers.get_or_load(path)?;
            let timeline_start =
                crate::timeline::ui::frames_to_seconds(clip.timeline_range.start, seq_fps);
            let mut timeline_dur =
                crate::timeline::ui::frames_to_seconds(clip.timeline_range.duration, seq_fps);
            let media_fps = clip_media_fps(app, clip);
            let mut media_start =
                crate::timeline::ui::frames_to_seconds(clip.media_range.start, media_fps);
            let media_range_duration =
                crate::timeline::ui::frames_to_seconds(clip.media_range.duration, media_fps);
            let rate = clip.playback_rate.max(0.0001) as f64;
            timeline_dur /= rate;
            media_start /= rate;
            let available_media = media_range_duration
                .max(0.0)
                .min((buf.duration_sec as f64 - media_start).max(0.0));
            let clip_duration = timeline_dur.min(available_media);
            if clip_duration <= 0.0 {
                continue;
            }
            clips.push(ActiveAudioClip {
                start_tl_sec: timeline_start,
                start_media_sec: media_start,
                duration_sec: clip_duration,
                buf: buf.clone(),
            });
        }
    }

    clips.sort_by(|a, b| {
        a.start_tl_sec
            .partial_cmp(&b.start_tl_sec)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(clips)
}

fn clip_media_fps(app: &App, clip: &crate::timeline_crate::ClipNode) -> Fps {
    if let Value::Object(map) = &clip.metadata {
        let num = map
            .get("media_fps_num")
            .and_then(|v| v.as_i64())
            .filter(|n| *n > 0)
            .map(|n| n as u32);
        let den = map
            .get("media_fps_den")
            .and_then(|v| v.as_i64())
            .filter(|d| *d > 0)
            .map(|d| d as u32);
        if let (Some(num), Some(den)) = (num, den) {
            if num > 0 && den > 0 {
                return Fps { num, den };
            }
        }
    }

    if let Some(path) = clip.asset_id.as_deref() {
        if let Ok(Some(asset)) = app.db.find_asset_by_path(&app.project_id, path) {
            if let (Some(num), Some(den)) = (asset.fps_num, asset.fps_den) {
                if num > 0 && den > 0 {
                    return Fps {
                        num: num as u32,
                        den: den as u32,
                    };
                }
            }
        }
    }

    app.seq.fps
}

pub(super) fn active_video_media_time_graph_impl(
    app: &App,
    timeline_sec: f64,
) -> Option<(String, f64)> {
    let seq_fps = (app.seq.fps.num.max(1) as f64) / (app.seq.fps.den.max(1) as f64);
    let playhead = (timeline_sec * seq_fps).round() as i64;
    // Priority: lower-numbered (top-most) video tracks first
    for binding in app.seq.graph.tracks.iter() {
        if matches!(binding.kind, TrackKind::Audio) {
            continue;
        }
        for node_id in &binding.node_ids {
            let Some(node) = app.seq.graph.nodes.get(node_id) else {
                continue;
            };
            let clip = match &node.kind {
                TimelineNodeKind::Clip(c) => c,
                _ => continue,
            };
            if playhead < clip.timeline_range.start || playhead >= clip.timeline_range.end() {
                continue;
            }
            let Some(path) = clip.asset_id.clone() else {
                continue;
            };
            let start_on_timeline_sec = clip.timeline_range.start as f64 / seq_fps;
            let local_t = (timeline_sec - start_on_timeline_sec).max(0.0);
            let media_fps = clip_media_fps(app, clip);
            let media_sec =
                crate::timeline::ui::frames_to_seconds(clip.media_range.start, media_fps)
                    + local_t * clip.playback_rate as f64;
            return Some((path, media_sec));
        }
    }
    None
}

pub(super) fn active_audio_media_time_graph_impl(
    app: &App,
    timeline_sec: f64,
) -> Option<(String, f64)> {
    let seq_fps = (app.seq.fps.num.max(1) as f64) / (app.seq.fps.den.max(1) as f64);
    let playhead = (timeline_sec * seq_fps).round() as i64;
    for binding in app.seq.graph.tracks.iter().rev() {
        if !matches!(binding.kind, TrackKind::Audio) {
            continue;
        }
        for node_id in &binding.node_ids {
            let Some(node) = app.seq.graph.nodes.get(node_id) else {
                continue;
            };
            let clip = match &node.kind {
                TimelineNodeKind::Clip(c) => c,
                _ => continue,
            };
            if playhead < clip.timeline_range.start || playhead >= clip.timeline_range.end() {
                continue;
            }
            let Some(path) = clip.asset_id.clone() else {
                continue;
            };
            let start_on_timeline_sec = clip.timeline_range.start as f64 / seq_fps;
            let local_t = (timeline_sec - start_on_timeline_sec).max(0.0);
            let media_fps = clip_media_fps(app, clip);
            let media_sec =
                crate::timeline::ui::frames_to_seconds(clip.media_range.start, media_fps)
                    + local_t * clip.playback_rate as f64;
            return Some((path, media_sec));
        }
    }
    active_video_media_time_graph_impl(app, timeline_sec)
}

pub(super) fn request_audio_peaks_impl(_app: &mut App, _path: &std::path::Path) {
    // Placeholder: integrate with audio decoding backend to compute peaks.
    // Keep bounded: one job per path. For now, no-op to avoid blocking UI.
}

// save_project_timeline_impl moved to app_project.rs

// Thin App method wrappers to keep app.rs small
impl App {
    pub(crate) fn apply_timeline_command(
        &mut self,
        command: TimelineCommand,
    ) -> Result<(), TimelineError> {
        self::apply_timeline_command_impl(self, command)
    }

    pub(crate) fn sync_tracks_from_graph(&mut self) {
        self::sync_tracks_from_graph_impl(self)
    }

    pub(crate) fn item_from_node(
        node: &TimelineNode,
        track_kind: &TrackKind,
        fps: Fps,
    ) -> Option<Item> {
        self::item_from_node_impl(node, track_kind, fps)
    }

    pub(crate) fn build_audio_clips(&mut self) -> anyhow::Result<Vec<ActiveAudioClip>> {
        self::build_audio_clips_impl(self)
    }

    pub(crate) fn active_video_media_time_graph(&self, timeline_sec: f64) -> Option<(String, f64)> {
        self::active_video_media_time_graph_impl(self, timeline_sec)
    }

    pub(crate) fn active_audio_media_time_graph(&self, timeline_sec: f64) -> Option<(String, f64)> {
        self::active_audio_media_time_graph_impl(self, timeline_sec)
    }

    pub(crate) fn request_audio_peaks(&mut self, path: &std::path::Path) {
        self::request_audio_peaks_impl(self, path)
    }
}
