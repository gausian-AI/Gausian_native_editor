use std::path::Path;

use crate::timeline_crate::{
    ClipNode, Fps, FrameRange, ItemKind, NodeId, TimelineCommand, TimelineNode, TimelineNodeKind,
    TrackId, TrackKind, TrackPlacement,
};
use eframe::egui::{self, Color32, Rect, Shape, Stroke};
use serde_json::Value;

use crate::decode::PlayState;
use crate::interaction::{DragMode, DragState, LinkedDragNode};
use crate::App;

#[derive(Debug, Clone)]
pub(crate) struct NodeDisplayInfo {
    pub(crate) start: i64,
    pub(crate) duration: i64,
    pub(crate) label: String,
    pub(crate) color: Color32,
    pub(crate) media_src: Option<String>,
}

pub(crate) fn parse_hex_color(hex: &str) -> Option<Color32> {
    let trimmed = hex.trim_start_matches('#');
    if trimmed.len() == 6 {
        if let Ok(v) = u32::from_str_radix(trimmed, 16) {
            let r = ((v >> 16) & 0xff) as u8;
            let g = ((v >> 8) & 0xff) as u8;
            let b = (v & 0xff) as u8;
            return Some(Color32::from_rgb(r, g, b));
        }
    }
    None
}

pub(crate) fn frames_to_seconds(frames: i64, fps: Fps) -> f64 {
    if fps.num == 0 {
        return 0.0;
    }
    let num = fps.num as f64;
    let den = fps.den.max(1) as f64;
    (frames as f64) * (den / num)
}

impl App {
    fn gather_linked_drag_nodes(
        &self,
        primary_id: NodeId,
        primary_clip: &ClipNode,
    ) -> Vec<LinkedDragNode> {
        let asset_id = primary_clip.asset_id.clone();
        let orig_from = primary_clip.timeline_range.start;
        let orig_dur = primary_clip.timeline_range.duration;
        let orig_media_start = primary_clip.media_range.start;

        let mut linked = Vec::new();
        if asset_id.is_none() {
            return linked;
        }

        for (ti, binding) in self.seq.graph.tracks.iter().enumerate() {
            for (idx, node_id) in binding.node_ids.iter().enumerate() {
                if *node_id == primary_id {
                    continue;
                }
                if let Some(node) = self.seq.graph.nodes.get(node_id) {
                    if let TimelineNodeKind::Clip(clip) = &node.kind {
                        if clip.asset_id == asset_id
                            && clip.timeline_range.start == orig_from
                            && clip.timeline_range.duration == orig_dur
                            && clip.media_range.start == orig_media_start
                        {
                            linked.push(LinkedDragNode {
                                node_id: *node_id,
                                original_node: node.clone(),
                                original_track_id: binding.id,
                                original_track_index: ti,
                                current_track_index: ti,
                                original_position: idx,
                                orig_from: clip.timeline_range.start,
                                orig_dur: clip.timeline_range.duration,
                                orig_media_start: clip.media_range.start,
                            });
                        }
                    }
                }
            }
        }

        linked
    }


    fn clip_media_fps(&self, clip: &ClipNode) -> Fps {
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
            if let Ok(Some(asset)) = self.db.find_asset_by_path(&self.project_id, path) {
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

        self.seq.fps
    }

    pub(crate) fn display_info_for_node(
        node: &TimelineNode,
        track_kind: &TrackKind,
    ) -> Option<NodeDisplayInfo> {
        match &node.kind {
            TimelineNodeKind::Clip(clip) => {
                let label = clip
                    .asset_id
                    .as_ref()
                    .and_then(|id| {
                        Path::new(id)
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                    })
                    .or_else(|| node.label.clone())
                    .unwrap_or_else(|| "Clip".to_string());
                let color = match track_kind {
                    TrackKind::Audio => egui::Color32::from_rgb(40, 120, 40),
                    TrackKind::Automation => egui::Color32::from_rgb(200, 140, 60),
                    _ => egui::Color32::from_rgb(40, 90, 160),
                };
                Some(NodeDisplayInfo {
                    start: clip.timeline_range.start,
                    duration: clip.timeline_range.duration,
                    label,
                    color,
                    media_src: clip.asset_id.clone(),
                })
            }
            TimelineNodeKind::Generator {
                generator_id,
                timeline_range,
                metadata,
            } => {
                let base_color = match generator_id.as_str() {
                    "solid" => {
                        if let Some(color_str) = metadata.get("color").and_then(|v| v.as_str()) {
                            parse_hex_color(color_str)
                                .unwrap_or(egui::Color32::from_rgb(80, 80, 80))
                        } else {
                            egui::Color32::from_rgb(80, 80, 80)
                        }
                    }
                    "text" => egui::Color32::from_rgb(150, 80, 150),
                    _ => egui::Color32::from_rgb(110, 110, 110),
                };
                Some(NodeDisplayInfo {
                    start: timeline_range.start,
                    duration: timeline_range.duration,
                    label: generator_id.clone(),
                    color: base_color,
                    media_src: None,
                })
            }
            TimelineNodeKind::Transition(_) | TimelineNodeKind::Effect { .. } => None,
        }
    }

    pub(crate) fn node_frame_range(node: &TimelineNode) -> Option<FrameRange> {
        match &node.kind {
            TimelineNodeKind::Clip(clip) => Some(clip.timeline_range.clone()),
            TimelineNodeKind::Generator { timeline_range, .. } => Some(timeline_range.clone()),
            _ => None,
        }
    }

    pub(crate) fn update_selection_for_node(&mut self, node_id: NodeId) {
        for (ti, binding) in self.seq.graph.tracks.iter().enumerate() {
            if let Some(idx) = binding.node_ids.iter().position(|id| *id == node_id) {
                self.selected = Some((ti, idx));
                return;
            }
        }
    }

    pub(crate) fn move_node_between_tracks(&mut self, drag: &mut DragState, target_track: usize) {
        if target_track >= self.seq.graph.tracks.len() || drag.current_track_index == target_track {
            return;
        }

        let original_kind = self
            .seq
            .graph
            .tracks
            .get(drag.original_track_index)
            .map(|t| t.kind.clone());
        let target_kind = self
            .seq
            .graph
            .tracks
            .get(target_track)
            .map(|t| t.kind.clone());
        if let (Some(orig), Some(target)) = (original_kind, target_kind) {
            let orig_is_audio = matches!(orig, TrackKind::Audio);
            let target_is_audio = matches!(target, TrackKind::Audio);
            if orig_is_audio != target_is_audio {
                return;
            }
        }

        if let Some(binding) = self.seq.graph.tracks.get_mut(drag.current_track_index) {
            if let Some(pos) = binding.node_ids.iter().position(|id| *id == drag.node_id) {
                binding.node_ids.remove(pos);
            }
        }
        if let Some(binding) = self.seq.graph.tracks.get_mut(target_track) {
            binding.node_ids.push(drag.node_id);
        }
        drag.current_track_index = target_track;
    }

    fn restore_single_drag_node(
        &mut self,
        node_id: NodeId,
        original_node: &TimelineNode,
        original_track_id: TrackId,
        original_position: usize,
    ) {
        for binding in &mut self.seq.graph.tracks {
            if let Some(pos) = binding.node_ids.iter().position(|id| *id == node_id) {
                binding.node_ids.remove(pos);
            }
        }
        if let Some(binding) = self
            .seq
            .graph
            .tracks
            .iter_mut()
            .find(|b| b.id == original_track_id)
        {
            let pos = original_position.min(binding.node_ids.len());
            binding.node_ids.insert(pos, node_id);
        }
        self.seq.graph.nodes.insert(node_id, original_node.clone());
    }

    pub(crate) fn restore_drag_preview(&mut self, drag: &DragState) {
        self.restore_single_drag_node(
            drag.node_id,
            &drag.original_node,
            drag.original_track_id,
            drag.original_position,
        );
        for linked in &drag.linked {
            self.restore_single_drag_node(
                linked.node_id,
                &linked.original_node,
                linked.original_track_id,
                linked.original_position,
            );
        }
    }

    pub(crate) fn preview_move_node(&mut self, drag: &DragState, new_from: i64) {
        let delta = new_from - drag.orig_from;
        if let Some(node) = self.seq.graph.nodes.get_mut(&drag.node_id) {
            match &mut node.kind {
                TimelineNodeKind::Clip(clip) => {
                    clip.timeline_range.start = new_from;
                    clip.timeline_range.duration = drag.orig_dur;
                }
                TimelineNodeKind::Generator { timeline_range, .. } => {
                    timeline_range.start = new_from;
                    timeline_range.duration = drag.orig_dur;
                }
                _ => {}
            }
        }

        for linked in &drag.linked {
            let target_start = (linked.orig_from + delta).max(0);
            if let Some(node) = self.seq.graph.nodes.get_mut(&linked.node_id) {
                match &mut node.kind {
                    TimelineNodeKind::Clip(clip) => {
                        clip.timeline_range.start = target_start;
                        clip.timeline_range.duration = linked.orig_dur;
                        clip.media_range.start = linked.orig_media_start;
                        clip.media_range.duration = linked.orig_dur;
                    }
                    TimelineNodeKind::Generator { timeline_range, .. } => {
                        timeline_range.start = target_start;
                        timeline_range.duration = linked.orig_dur;
                    }
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn preview_trim_start_node(
        &mut self,
        drag: &DragState,
        new_from: i64,
        new_duration: i64,
        delta_frames: i64,
    ) {
        if let Some(node) = self.seq.graph.nodes.get_mut(&drag.node_id) {
            match (&mut node.kind, &drag.original_node.kind) {
                (TimelineNodeKind::Clip(clip), TimelineNodeKind::Clip(orig_clip)) => {
                    let media_start = orig_clip.media_range.start + delta_frames;
                    clip.timeline_range.start = new_from;
                    clip.timeline_range.duration = new_duration;
                    clip.media_range.start = media_start;
                    clip.media_range.duration = new_duration;
                }
                (
                    TimelineNodeKind::Generator { timeline_range, .. },
                    TimelineNodeKind::Generator { .. },
                ) => {
                    timeline_range.start = new_from;
                    timeline_range.duration = new_duration;
                }
                _ => {}
            }
        }

        for linked in &drag.linked {
            let target_start = (linked.orig_from + delta_frames).max(0);
            let target_duration = (linked.orig_dur - delta_frames).max(1);
            if let Some(node) = self.seq.graph.nodes.get_mut(&linked.node_id) {
                if let (TimelineNodeKind::Clip(clip), TimelineNodeKind::Clip(orig_clip)) =
                    (&mut node.kind, &linked.original_node.kind)
                {
                    let media_start = orig_clip.media_range.start + delta_frames;
                    clip.timeline_range.start = target_start;
                    clip.timeline_range.duration = target_duration;
                    clip.media_range.start = media_start;
                    clip.media_range.duration = target_duration;
                }
            }
        }
    }

    pub(crate) fn preview_trim_end_node(&mut self, drag: &DragState, new_duration: i64) {
        if let Some(node) = self.seq.graph.nodes.get_mut(&drag.node_id) {
            match &mut node.kind {
                TimelineNodeKind::Clip(clip) => {
                    clip.timeline_range.duration = new_duration;
                    clip.media_range.duration = new_duration;
                }
                TimelineNodeKind::Generator { timeline_range, .. } => {
                    timeline_range.duration = new_duration;
                }
                _ => {}
            }
        }

        for linked in &drag.linked {
            if let Some(node) = self.seq.graph.nodes.get_mut(&linked.node_id) {
                match &mut node.kind {
                    TimelineNodeKind::Clip(clip) => {
                        let clamped = new_duration.max(1);
                        clip.timeline_range.duration = clamped;
                        clip.media_range.duration = clamped;
                    }
                    TimelineNodeKind::Generator { timeline_range, .. } => {
                        timeline_range.duration = new_duration.max(1);
                    }
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn update_drag_preview(
        &mut self,
        drag: &mut DragState,
        pointer: egui::Pos2,
        rect: egui::Rect,
        track_h: f32,
    ) {
        let target_track = ((pointer.y - rect.top()) / track_h).floor() as isize;
        let track_count = self.seq.graph.tracks.len() as isize;
        let clamped_track = target_track.clamp(0, track_count.saturating_sub(1)) as usize;
        self.move_node_between_tracks(drag, clamped_track);

        let mx = pointer.x;
        let dx_px = mx - drag.start_mouse_x;
        let df = (dx_px / self.zoom_px_per_frame).round() as i64;
        let fpsf = self.seq.fps.num.max(1) as f32 / self.seq.fps.den.max(1) as f32;
        let eps = 3.0;

        match drag.mode {
            DragMode::Move => {
                let mut new_from = (drag.orig_from + df).max(0);
                let secf = (new_from as f32 / fpsf).round() * fpsf;
                if (secf - new_from as f32).abs() <= eps {
                    new_from = secf as i64;
                }
                self.preview_move_node(drag, new_from);
            }
            DragMode::TrimStart => {
                let mut new_from =
                    (drag.orig_from + df).clamp(0, drag.orig_from + drag.orig_dur - 1);
                let secf = (new_from as f32 / fpsf).round() * fpsf;
                if (secf - new_from as f32).abs() <= eps {
                    new_from = secf as i64;
                }
                let delta_frames = (new_from - drag.orig_from).max(0);
                let new_duration = (drag.orig_dur - delta_frames).max(1);
                self.preview_trim_start_node(drag, new_from, new_duration, delta_frames);
            }
            DragMode::TrimEnd => {
                let mut new_duration = (drag.orig_dur + df).max(1);
                let end = drag.orig_from + new_duration;
                let secf = (end as f32 / fpsf).round() * fpsf;
                if (secf - end as f32).abs() <= eps {
                    new_duration = (secf as i64 - drag.orig_from).max(1);
                }
                self.preview_trim_end_node(drag, new_duration);
            }
        }

        self.sync_tracks_from_graph();
        self.update_selection_for_node(drag.node_id);
    }

    pub(crate) fn finish_drag(&mut self, drag: DragState) {
        let target_track_id = self
            .seq
            .graph
            .tracks
            .get(drag.current_track_index)
            .map(|b| b.id);
        let final_node = self.seq.graph.nodes.get(&drag.node_id).cloned();
        let linked_finals: Vec<(LinkedDragNode, Option<TimelineNode>)> = drag
            .linked
            .iter()
            .cloned()
            .map(|ln| {
                let node = self.seq.graph.nodes.get(&ln.node_id).cloned();
                (ln, node)
            })
            .collect();
        self.restore_drag_preview(&drag);

        if let Some(node) = final_node {
            let track_changed = drag.current_track_index != drag.original_track_index;
            if !track_changed && node == drag.original_node {
                self.sync_tracks_from_graph();
                self.update_selection_for_node(drag.node_id);
                return;
            }

            if track_changed {
                let target_id = target_track_id.unwrap_or(drag.original_track_id);
                if let Err(err) = self.apply_timeline_command(TimelineCommand::RemoveNode {
                    node_id: drag.node_id,
                }) {
                    eprintln!("timeline remove failed: {err}");
                    return;
                }
                if let Err(err) = self.apply_timeline_command(TimelineCommand::InsertNode {
                    node,
                    placements: vec![TrackPlacement {
                        track_id: target_id,
                        position: None,
                    }],
                    edges: Vec::new(),
                }) {
                    eprintln!("timeline insert failed: {err}");
                    return;
                }
            } else {
                if let Err(err) = self.apply_timeline_command(TimelineCommand::UpdateNode { node })
                {
                    eprintln!("timeline update failed: {err}");
                    return;
                }
            }
            self.update_selection_for_node(drag.node_id);
        } else {
            self.sync_tracks_from_graph();
        }

        for (linked, final_node) in linked_finals {
            if let Some(node) = final_node {
                if node == linked.original_node {
                    continue;
                }
                if let Err(err) = self.apply_timeline_command(TimelineCommand::UpdateNode { node })
                {
                    eprintln!("timeline update failed: {err}");
                }
            }
        }
    }

    pub(crate) fn split_clip_at_frame(&mut self, track: usize, item: usize, split_frame: i64) {
        let track_binding = match self.seq.graph.tracks.get(track) {
            Some(binding) => binding.clone(),
            None => return,
        };
        let node_id = match track_binding.node_ids.get(item) {
            Some(id) => *id,
            None => return,
        };
        let node = match self.seq.graph.nodes.get(&node_id) {
            Some(n) => n.clone(),
            None => return,
        };

        match node.kind {
            TimelineNodeKind::Clip(ref clip) => {
                let mut targets: Vec<(usize, TrackId, usize, NodeId, TimelineNode)> = Vec::new();
                targets.push((track, track_binding.id, item, node_id, node.clone()));
                for ln in self.gather_linked_drag_nodes(node_id, clip) {
                    targets.push((
                        ln.original_track_index,
                        ln.original_track_id,
                        ln.original_position,
                        ln.node_id,
                        ln.original_node.clone(),
                    ));
                }

                for (target_track, target_track_id, target_pos, target_node_id, target_node) in
                    targets
                {
                    if let TimelineNodeKind::Clip(ref clip) = target_node.kind {
                        let start = clip.timeline_range.start;
                        let end = clip.timeline_range.end();
                        if split_frame <= start || split_frame >= end {
                            continue;
                        }
                        let left_dur = split_frame - start;
                        let right_dur = end - split_frame;

                        let mut left_clip = clip.clone();
                        left_clip.timeline_range = FrameRange::new(start, left_dur);
                        left_clip.media_range = FrameRange::new(clip.media_range.start, left_dur);

                        let mut updated_node = target_node.clone();
                        updated_node.kind = TimelineNodeKind::Clip(left_clip);
                        if let Err(err) = self.apply_timeline_command(TimelineCommand::UpdateNode {
                            node: updated_node,
                        }) {
                            eprintln!("timeline update failed: {err}");
                            continue;
                        }

                        let right_media_start = clip.media_range.start + left_dur;
                        let mut right_clip = clip.clone();
                        right_clip.timeline_range = FrameRange::new(split_frame, right_dur);
                        right_clip.media_range = FrameRange::new(right_media_start, right_dur);

                        let right_node = TimelineNode {
                            id: NodeId::new(),
                            label: target_node.label.clone(),
                            kind: TimelineNodeKind::Clip(right_clip),
                            locked: target_node.locked,
                            metadata: target_node.metadata.clone(),
                        };
                        let placement = TrackPlacement {
                            track_id: target_track_id,
                            position: Some(target_pos + 1),
                        };
                        if let Err(err) = self.apply_timeline_command(TimelineCommand::InsertNode {
                            node: right_node,
                            placements: vec![placement],
                            edges: Vec::new(),
                        }) {
                            eprintln!("timeline insert failed: {err}");
                            continue;
                        }

                        if target_node_id == node_id {
                            self.selected = Some((target_track, target_pos + 1));
                        }
                    }
                }
                if self.selected.is_none() {
                    self.selected = Some((track, item + 1));
                }
            }
            TimelineNodeKind::Generator {
                ref generator_id,
                ref timeline_range,
                ref metadata,
            } => {
                let start = timeline_range.start;
                let end = timeline_range.end();
                if split_frame <= start || split_frame >= end {
                    return;
                }
                let left_dur = split_frame - start;
                let right_dur = end - split_frame;

                let mut updated_node = node.clone();
                if let TimelineNodeKind::Generator {
                    ref mut timeline_range,
                    ..
                } = updated_node.kind
                {
                    *timeline_range = FrameRange::new(start, left_dur);
                }
                if let Err(err) =
                    self.apply_timeline_command(TimelineCommand::UpdateNode { node: updated_node })
                {
                    eprintln!("timeline update failed: {err}");
                    return;
                }

                let right_node = TimelineNode {
                    id: NodeId::new(),
                    label: node.label.clone(),
                    kind: TimelineNodeKind::Generator {
                        generator_id: generator_id.clone(),
                        timeline_range: FrameRange::new(split_frame, right_dur),
                        metadata: metadata.clone(),
                    },
                    locked: node.locked,
                    metadata: node.metadata.clone(),
                };
                let placement = TrackPlacement {
                    track_id: track_binding.id,
                    position: Some(item + 1),
                };
                if let Err(err) = self.apply_timeline_command(TimelineCommand::InsertNode {
                    node: right_node,
                    placements: vec![placement],
                    edges: Vec::new(),
                }) {
                    eprintln!("timeline insert failed: {err}");
                    return;
                }
                self.selected = Some((track, item + 1));
            }
            _ => {}
        }
    }

    pub(crate) fn remove_clip(&mut self, track: usize, item: usize) {
        let track_binding = match self.seq.graph.tracks.get(track) {
            Some(binding) => binding,
            None => return,
        };
        let node_id = match track_binding.node_ids.get(item) {
            Some(id) => *id,
            None => return,
        };
        if let Err(err) = self.apply_timeline_command(TimelineCommand::RemoveNode { node_id }) {
            eprintln!("timeline remove failed: {err}");
        } else {
            self.selected = None;
        }
    }

    pub(crate) fn timeline_ui(&mut self, ui: &mut egui::Ui) {
        // Reset scrubbing flag; set true only while background dragging
        ui.horizontal(|ui| {
            ui.label("Zoom");
            ui.add(egui::Slider::new(&mut self.zoom_px_per_frame, 0.2..=20.0).logarithmic(true));
            if ui.button("Fit").clicked() {
                let width = ui.available_width().max(1.0);
                self.zoom_px_per_frame =
                    (width / (self.seq.duration_in_frames.max(1) as f32)).max(0.1);
            }
        });

        let track_h = 48.0;
        let content_w = (self.seq.duration_in_frames as f32 * self.zoom_px_per_frame).max(1000.0);
        let track_count = self.seq.graph.tracks.len().max(1);
        let content_h = (track_count as f32 * track_h).max(200.0);
        egui::ScrollArea::both()
            .drag_to_scroll(false)
            .show(ui, |ui| {
                let mut to_request: Vec<std::path::PathBuf> = Vec::new();
                let mut clicked_item = false;
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(content_w, content_h),
                    egui::Sense::click_and_drag(),
                );
                self.timeline_drop_rect = Some(rect);
                let painter = ui.painter_at(rect);
                // Background
                painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 18, 20));
                // If dragging an asset, show tentative drop indicator under cursor (ghost clip)
                if let Some(asset) = self.dragging_asset.as_ref() {
                    if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                        if rect.contains(pos) {
                            let track_idx =
                                ((pos.y - rect.top()) / track_h).floor().max(0.0) as usize;
                            let y0 = rect.top() + track_idx as f32 * track_h;
                            let start_frames = ((pos.x - rect.left()).max(0.0) as f64
                                / self.zoom_px_per_frame as f64)
                                .round() as i64;
                            let seq_fps = self.seq.fps;
                            let seq_rate = {
                                let num = seq_fps.num.max(1) as f64;
                                let den = seq_fps.den.max(1) as f64;
                                if den > 0.0 {
                                    num / den
                                } else {
                                    30.0
                                }
                            };
                            let dur = asset
                                .duration_seconds()
                                .map(|sec| (sec * seq_rate).round() as i64)
                                .filter(|v| *v > 0)
                                .or_else(|| asset.duration_frames.map(|v| v.max(1)))
                                .unwrap_or(150);
                            let x0 = rect.left() + start_frames as f32 * self.zoom_px_per_frame;
                            let x1 = x0 + (dur as f32 * self.zoom_px_per_frame).max(12.0);
                            let r = egui::Rect::from_min_max(
                                egui::pos2(x0, y0 + 4.0),
                                egui::pos2(x1, y0 + track_h - 4.0),
                            );
                            let is_audio = asset.kind.eq_ignore_ascii_case("audio");
                            let fill = if is_audio {
                                egui::Color32::from_rgba_unmultiplied(40, 160, 60, 120)
                            } else {
                                egui::Color32::from_rgba_unmultiplied(60, 120, 220, 120)
                            };
                            painter.rect_filled(r, 4.0, fill);
                            painter.rect_stroke(
                                r,
                                4.0,
                                egui::Stroke::new(1.0, egui::Color32::from_rgb(250, 250, 180)),
                            );
                            let name = std::path::Path::new(&asset.src_abs)
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| asset.src_abs.clone());
                            painter.text(
                                r.center_top() + egui::vec2(0.0, 12.0),
                                egui::Align2::CENTER_TOP,
                                name,
                                egui::FontId::monospace(12.0),
                                egui::Color32::WHITE,
                            );
                        }
                    }
                }
                // Vertical grid each second
                let fps =
                    (self.seq.fps.num.max(1) as f32 / self.seq.fps.den.max(1) as f32).max(1.0);
                let px_per_sec = self.zoom_px_per_frame * fps;
                let start_x = rect.left();
                let mut x = start_x;
                while x < rect.right() {
                    painter.line_segment(
                        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                        egui::Stroke::new(1.0, egui::Color32::from_gray(50)),
                    );
                    x += px_per_sec;
                }
                // Tracks and clips
                let mut completed_drag: Option<DragState> = None;
                // Separate counters for video/audio labels (V1, V2, A1, ...)
                let mut v_idx: usize = 0;
                let mut a_idx: usize = 0;
                let mut aut_idx: usize = 0;
                for (ti, binding) in self.seq.graph.tracks.iter().enumerate() {
                    let y = rect.top() + ti as f32 * track_h;
                    let row_rect = egui::Rect::from_min_max(
                        egui::pos2(rect.left(), y),
                        egui::pos2(rect.right(), y + track_h),
                    );
                    // Subtle shaded background per track kind
                    let row_color = match &binding.kind {
                        TrackKind::Audio => egui::Color32::from_rgba_unmultiplied(30, 50, 30, 40),
                        TrackKind::Automation => {
                            egui::Color32::from_rgba_unmultiplied(50, 40, 20, 30)
                        }
                        TrackKind::Custom(id) if id == "image" => {
                            egui::Color32::from_rgba_unmultiplied(35, 40, 55, 40)
                        }
                        _ => egui::Color32::from_rgba_unmultiplied(40, 40, 55, 40), // video/default
                    };
                    painter.rect_filled(row_rect, 0.0, row_color);
                    // track separator
                    painter.line_segment(
                        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                        egui::Stroke::new(1.0, egui::Color32::from_gray(60)),
                    );
                    // Track label with number (Vn/An) at the left
                    let track_label = match &binding.kind {
                        TrackKind::Audio => {
                            a_idx += 1;
                            format!("A{}", a_idx)
                        }
                        TrackKind::Automation => {
                            aut_idx += 1;
                            format!("AUT{}", aut_idx)
                        }
                        _ => {
                            v_idx += 1;
                            format!("V{}", v_idx)
                        }
                    };
                    let name = if binding.name.trim().is_empty() {
                        track_label.clone()
                    } else {
                        format!("{}  ·  {}", track_label, binding.name)
                    };
                    painter.text(
                        egui::pos2(rect.left() + 8.0, y + 14.0),
                        egui::Align2::LEFT_TOP,
                        name,
                        egui::FontId::monospace(12.0),
                        egui::Color32::from_gray(210),
                    );
                    // items
                    for (ii, node_id) in binding.node_ids.iter().enumerate() {
                        let Some(node) = self.seq.graph.nodes.get(node_id) else {
                            continue;
                        };
                        let Some(display) = Self::display_info_for_node(node, &binding.kind)
                            .or_else(|| {
                                Self::item_from_node(node, &binding.kind, self.seq.fps).map(
                                    |item| NodeDisplayInfo {
                                        start: item.from,
                                        duration: item.duration_in_frames,
                                        label: item.id.clone(),
                                        color: egui::Color32::from_rgb(90, 90, 90),
                                        media_src: match item.kind {
                                            ItemKind::Audio { ref src, .. } => Some(src.clone()),
                                            ItemKind::Video { ref src, .. } => Some(src.clone()),
                                            ItemKind::Image { ref src } => Some(src.clone()),
                                            _ => None,
                                        },
                                    },
                                )
                            })
                        else {
                            continue;
                        };
                        let x0 = rect.left() + display.start as f32 * self.zoom_px_per_frame;
                        let x1 = x0 + display.duration as f32 * self.zoom_px_per_frame;
                        let r = egui::Rect::from_min_max(
                            egui::pos2(x0, y + 4.0),
                            egui::pos2(x1, y + track_h - 4.0),
                        );
                        let mut border = egui::Stroke::new(1.0, egui::Color32::BLACK);
                        if let Some(sel) = self.selected {
                            if sel == (ti, ii) {
                                border = egui::Stroke::new(2.0, egui::Color32::WHITE);
                            }
                        }
                        let label = display.label.clone();
                        let color = display.color;
                        painter.rect_filled(r, 4.0, color);
                        painter.rect_stroke(r, 4.0, border);
                        painter.text(
                            r.center_top() + egui::vec2(0.0, 12.0),
                            egui::Align2::CENTER_TOP,
                            label,
                            egui::FontId::monospace(12.0),
                            egui::Color32::WHITE,
                        );

                        // Optional lightweight waveform lane under clips (audio or video)
                        if let Some(src_path) = display.media_src.as_deref() {
                            let pbuf = std::path::PathBuf::from(src_path);
                            if let Some(peaks) = self.audio_cache.map.get(&pbuf) {
                                let rect_lane = r.shrink2(egui::vec2(2.0, 6.0));
                                let n = peaks.peaks.len().max(1);
                                let mut pts_top: Vec<egui::Pos2> = Vec::with_capacity(n);
                                let mut pts_bot: Vec<egui::Pos2> = Vec::with_capacity(n);
                                for (i, (mn, mx)) in peaks.peaks.iter().enumerate() {
                                    let t = if n > 1 {
                                        i as f32 / (n as f32 - 1.0)
                                    } else {
                                        0.0
                                    };
                                    let x = egui::lerp(rect_lane.left()..=rect_lane.right(), t);
                                    let y0 = egui::lerp(
                                        rect_lane.center().y..=rect_lane.top(),
                                        mx.abs().min(1.0),
                                    );
                                    let y1 = egui::lerp(
                                        rect_lane.center().y..=rect_lane.bottom(),
                                        mn.abs().min(1.0),
                                    );
                                    pts_top.push(egui::pos2(x, y0));
                                    pts_bot.push(egui::pos2(x, y1));
                                }
                                let stroke =
                                    egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 180, 240));
                                ui.painter().add(egui::Shape::line(pts_top, stroke));
                                ui.painter().add(egui::Shape::line(pts_bot, stroke));
                            } else {
                                to_request.push(pbuf);
                            }
                        }

                        // Make the clip rect an interactive drag target so ScrollArea doesn't pan
                        let resp = ui.interact(
                            r,
                            egui::Id::new(("clip", ti, ii)),
                            egui::Sense::click_and_drag(),
                        );
                        if resp.clicked() {
                            self.selected = Some((ti, ii));
                            clicked_item = true;
                        }
                        if resp.drag_started() {
                            if let Some(binding) = self.seq.graph.tracks.get(ti) {
                                if let Some(node_id) = binding.node_ids.get(ii) {
                                    if let Some(node) = self.seq.graph.nodes.get(node_id) {
                                        let mx = resp
                                            .interact_pointer_pos()
                                            .unwrap_or(egui::pos2(0.0, 0.0))
                                            .x;
                                        let mode = if (mx - r.left()).abs() <= 6.0 {
                                            DragMode::TrimStart
                                        } else if (mx - r.right()).abs() <= 6.0 {
                                            DragMode::TrimEnd
                                        } else {
                                            DragMode::Move
                                        };
                                        let range = Self::node_frame_range(node)
                                            .unwrap_or(FrameRange::new(0, 0));
                                        let (asset_id, linked) = match &node.kind {
                                            TimelineNodeKind::Clip(clip) => (
                                                clip.asset_id.clone(),
                                                self.gather_linked_drag_nodes(*node_id, clip),
                                            ),
                                            _ => (None, Vec::new()),
                                        };
                                        self.selected = Some((ti, ii));
                                        self.drag = Some(DragState {
                                            original_track_index: ti,
                                            current_track_index: ti,
                                            mode,
                                            start_mouse_x: mx,
                                            orig_from: range.start,
                                            orig_dur: range.duration,
                                            node_id: *node_id,
                                            original_node: node.clone(),
                                            original_track_id: binding.id,
                                            original_position: ii,
                                            asset_id,
                                            linked,
                                        });
                                    }
                                }
                            }
                        }
                        if resp.drag_released() {
                            if let Some(drag) = self.drag.take() {
                                completed_drag = Some(drag);
                            }
                        }
                    }
                }
                // Playhead
                let phx = rect.left() + self.playhead as f32 * self.zoom_px_per_frame;
                painter.line_segment(
                    [egui::pos2(phx, rect.top()), egui::pos2(phx, rect.bottom())],
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(220, 60, 60)),
                );

                // Click/drag background to scrub (when not dragging a clip)
                if response.clicked() && !clicked_item {
                    self.selected = None;
                }

                if self.drag.is_none() && self.dragging_asset.is_none() {
                    let was_playing = self.playback_clock.playing;
                    // Single click: move playhead on mouse up as well
                    if response.clicked() {
                        if let Some(pos) = response.interact_pointer_pos() {
                            let local_px = (pos.x - rect.left()).max(0.0) as f64;
                            let fps =
                                (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
                            let frames = (local_px / self.zoom_px_per_frame as f64).round() as i64;
                            let sec = (frames as f64) / fps;
                            self.playback_clock.seek_to(sec);
                            self.playhead = frames.clamp(0, self.seq.duration_in_frames);
                            if let Some(engine) = &self.audio_out {
                                engine.seek(sec);
                            }
                            if was_playing {
                                self.engine.state = PlayState::Playing;
                                self.last_sent = None;
                                self.last_seek_sent_pts = None;
                            } else {
                                self.engine.state = PlayState::Seeking;
                            }
                        }
                    }
                    // Drag: continuously update while primary is down
                    if response.dragged() && ui.input(|i| i.pointer.primary_down()) {
                        if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                            let local_px = (pos.x - rect.left()).max(0.0) as f64;
                            let fps =
                                (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
                            let frames = (local_px / self.zoom_px_per_frame as f64).round() as i64;
                            let sec = (frames as f64) / fps;
                            self.playback_clock.seek_to(sec);
                            self.playhead = frames.clamp(0, self.seq.duration_in_frames);
                            if was_playing {
                                self.engine.state = PlayState::Playing;
                                self.last_sent = None;
                                self.last_seek_sent_pts = None;
                            } else {
                                self.engine.state = PlayState::Scrubbing;
                            }
                            if let Some(engine) = &self.audio_out {
                                engine.seek(sec);
                            }
                        }
                    }
                }

                // Timeline hotkeys: split/delete
                let pressed_split = ui.input(|i| {
                    i.key_pressed(egui::Key::K)
                        || (i.modifiers.command && i.key_pressed(egui::Key::S))
                });
                let pressed_delete = ui.input(|i| {
                    i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)
                });
                if pressed_split {
                    if let Some((t, iidx)) = self.selected {
                        let fps =
                            (self.seq.fps.num.max(1) as f64) / (self.seq.fps.den.max(1) as f64);
                        let t_sec = self.playback_clock.now();
                        let split_frame = (t_sec * fps).round() as i64;
                        self.split_clip_at_frame(t, iidx, split_frame);
                    }
                }
                if pressed_delete {
                    if let Some((t, iidx)) = self.selected.take() {
                        self.remove_clip(t, iidx);
                    }
                }

                if !ui.input(|i| i.pointer.primary_down()) {
                    if let Some(drag) = self.drag.take() {
                        completed_drag = Some(drag);
                    }
                    // Drop asset onto timeline if any
                    if let Some(asset) = self.dragging_asset.take() {
                        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                            if rect.contains(pos) {
                                let local_x = (pos.x - rect.left()).max(0.0) as f64;
                                let frames =
                                    (local_x / self.zoom_px_per_frame as f64).round() as i64;
                                let track_idx = ((pos.y - rect.top()) / track_h).floor() as isize;
                                let track_idx = track_idx.clamp(
                                    0,
                                    (self.seq.graph.tracks.len().saturating_sub(1)) as isize,
                                ) as usize;
                                self.insert_asset_at(&asset, track_idx, frames);
                            }
                        }
                    }
                } else if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                    if let Some(mut drag) = self.drag.take() {
                        self.update_drag_preview(&mut drag, pos, rect, track_h);
                        self.drag = Some(drag);
                    }
                }

                if let Some(drag) = completed_drag.take() {
                    self.finish_drag(drag);
                }
                // Defer any peak requests until after immutable borrows end
                for p in to_request {
                    self.request_audio_peaks(&p);
                }
            });
    }
}
