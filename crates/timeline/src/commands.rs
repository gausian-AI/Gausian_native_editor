use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AutomationKeyframe, AutomationLane, Frame, FrameRange, LaneId, NodeId, TimelineEdge,
    TimelineError, TimelineGraph, TimelineNode, TimelineNodeKind, TrackBinding, TrackId, TrackKind,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TrackPlacement {
    pub track_id: TrackId,
    pub position: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TimelineCommand {
    InsertNode {
        node: TimelineNode,
        #[serde(default)]
        placements: Vec<TrackPlacement>,
        #[serde(default)]
        edges: Vec<TimelineEdge>,
    },
    RemoveNode {
        node_id: NodeId,
    },
    UpdateNode {
        node: TimelineNode,
    },
    AddEdge {
        edge: TimelineEdge,
    },
    RemoveEdge {
        edge: TimelineEdge,
    },
    UpsertTrack {
        track: TrackBinding,
    },
    RemoveTrack {
        track_id: TrackId,
    },
    MoveTrack {
        track_id: TrackId,
        index: usize,
    },
    AddAutomationLane {
        lane: AutomationLane,
    },
    UpdateAutomationLane {
        lane: AutomationLane,
    },
    RemoveAutomationLane {
        lane_id: LaneId,
    },
    InsertAutomationKeyframe {
        lane_id: LaneId,
        keyframe: AutomationKeyframe,
    },
    RemoveAutomationKeyframe {
        lane_id: LaneId,
        frame: Frame,
    },
}

pub fn apply_command(
    graph: &mut TimelineGraph,
    command: TimelineCommand,
) -> Result<TimelineCommand, TimelineError> {
    match command {
        TimelineCommand::InsertNode {
            node,
            placements,
            edges,
        } => insert_node(graph, node, placements, edges),
        TimelineCommand::RemoveNode { node_id } => remove_node(graph, node_id),
        TimelineCommand::UpdateNode { node } => update_node(graph, node),
        TimelineCommand::AddEdge { edge } => add_edge(graph, edge),
        TimelineCommand::RemoveEdge { edge } => remove_edge(graph, edge),
        TimelineCommand::UpsertTrack { track } => upsert_track(graph, track),
        TimelineCommand::RemoveTrack { track_id } => remove_track(graph, track_id),
        TimelineCommand::MoveTrack { track_id, index } => move_track(graph, track_id, index),
        TimelineCommand::AddAutomationLane { lane } => add_lane(graph, lane),
        TimelineCommand::UpdateAutomationLane { lane } => update_lane(graph, lane),
        TimelineCommand::RemoveAutomationLane { lane_id } => remove_lane(graph, lane_id),
        TimelineCommand::InsertAutomationKeyframe { lane_id, keyframe } => {
            insert_keyframe(graph, lane_id, keyframe)
        }
        TimelineCommand::RemoveAutomationKeyframe { lane_id, frame } => {
            remove_keyframe(graph, lane_id, frame)
        }
    }
}

fn insert_node(
    graph: &mut TimelineGraph,
    node: TimelineNode,
    placements: Vec<TrackPlacement>,
    edges: Vec<TimelineEdge>,
) -> Result<TimelineCommand, TimelineError> {
    if graph.nodes.contains_key(&node.id) {
        return Err(TimelineError::NodeExists(node.id));
    }

    validate_placements(graph, &placements)?;
    validate_edges_for_insert(graph, &edges, node.id)?;

    graph.nodes.insert(node.id, node.clone());

    for placement in &placements {
        if let Some(track) = graph.tracks.iter_mut().find(|t| t.id == placement.track_id) {
            let idx = placement.position.unwrap_or(track.node_ids.len());
            track.node_ids.insert(idx, node.id);
        }
    }

    for edge in edges.iter() {
        graph.edges.push(edge.clone());
    }

    Ok(TimelineCommand::RemoveNode { node_id: node.id })
}

fn remove_node(
    graph: &mut TimelineGraph,
    node_id: NodeId,
) -> Result<TimelineCommand, TimelineError> {
    let node = graph
        .nodes
        .remove(&node_id)
        .ok_or(TimelineError::NodeNotFound(node_id))?;

    let mut placements = Vec::new();
    for track in graph.tracks.iter_mut() {
        let mut i = 0;
        while i < track.node_ids.len() {
            if track.node_ids[i] == node_id {
                track.node_ids.remove(i);
                placements.push(TrackPlacement {
                    track_id: track.id,
                    position: Some(i),
                });
            } else {
                i += 1;
            }
        }
    }

    let mut edges = Vec::new();
    let mut idx = 0;
    while idx < graph.edges.len() {
        let edge = &graph.edges[idx];
        if edge.from == node_id || edge.to == node_id {
            edges.push(graph.edges.remove(idx));
        } else {
            idx += 1;
        }
    }

    Ok(TimelineCommand::InsertNode {
        node,
        placements,
        edges,
    })
}

fn update_node(
    graph: &mut TimelineGraph,
    node: TimelineNode,
) -> Result<TimelineCommand, TimelineError> {
    let node_id = node.id;
    if let Some(entry) = graph.nodes.get_mut(&node_id) {
        let previous = entry.clone();
        *entry = node;
        Ok(TimelineCommand::UpdateNode { node: previous })
    } else {
        Err(TimelineError::NodeNotFound(node_id))
    }
}

fn add_edge(
    graph: &mut TimelineGraph,
    edge: TimelineEdge,
) -> Result<TimelineCommand, TimelineError> {
    if !graph.nodes.contains_key(&edge.from) {
        return Err(TimelineError::NodeNotFound(edge.from));
    }
    if !graph.nodes.contains_key(&edge.to) {
        return Err(TimelineError::NodeNotFound(edge.to));
    }
    if graph
        .edges
        .iter()
        .any(|e| e.from == edge.from && e.to == edge.to && e.kind == edge.kind)
    {
        return Err(TimelineError::EdgeExists(edge.from, edge.to));
    }
    graph.edges.push(edge.clone());
    Ok(TimelineCommand::RemoveEdge { edge })
}

fn remove_edge(
    graph: &mut TimelineGraph,
    edge: TimelineEdge,
) -> Result<TimelineCommand, TimelineError> {
    if let Some(idx) = graph
        .edges
        .iter()
        .position(|e| e.from == edge.from && e.to == edge.to && e.kind == edge.kind)
    {
        graph.edges.remove(idx);
        Ok(TimelineCommand::AddEdge { edge })
    } else {
        Err(TimelineError::EdgeNotFound(edge.from, edge.to))
    }
}

fn upsert_track(
    graph: &mut TimelineGraph,
    track: TrackBinding,
) -> Result<TimelineCommand, TimelineError> {
    if let Some(idx) = graph.tracks.iter().position(|t| t.id == track.id) {
        let previous = std::mem::replace(&mut graph.tracks[idx], track);
        Ok(TimelineCommand::UpsertTrack { track: previous })
    } else {
        graph.tracks.push(track.clone());
        Ok(TimelineCommand::RemoveTrack { track_id: track.id })
    }
}

fn remove_track(
    graph: &mut TimelineGraph,
    track_id: TrackId,
) -> Result<TimelineCommand, TimelineError> {
    if let Some(idx) = graph.tracks.iter().position(|t| t.id == track_id) {
        let track = graph.tracks.remove(idx);
        Ok(TimelineCommand::UpsertTrack { track })
    } else {
        Err(TimelineError::TrackNotFound(track_id))
    }
}

fn move_track(
    graph: &mut TimelineGraph,
    track_id: TrackId,
    index: usize,
) -> Result<TimelineCommand, TimelineError> {
    let current = graph
        .tracks
        .iter()
        .position(|t| t.id == track_id)
        .ok_or(TimelineError::TrackNotFound(track_id))?;
    let track = graph.tracks.remove(current);
    let target = std::cmp::min(index, graph.tracks.len());
    graph.tracks.insert(target, track);
    Ok(TimelineCommand::MoveTrack {
        track_id,
        index: current,
    })
}

fn add_lane(
    graph: &mut TimelineGraph,
    lane: AutomationLane,
) -> Result<TimelineCommand, TimelineError> {
    if graph.automation.iter().any(|l| l.id == lane.id) {
        return Err(TimelineError::InvalidOp(format!(
            "automation lane exists: {}",
            lane.id
        )));
    }
    graph.automation.push(lane.clone());
    Ok(TimelineCommand::RemoveAutomationLane { lane_id: lane.id })
}

fn update_lane(
    graph: &mut TimelineGraph,
    lane: AutomationLane,
) -> Result<TimelineCommand, TimelineError> {
    if let Some(idx) = graph.automation.iter().position(|l| l.id == lane.id) {
        let previous = std::mem::replace(&mut graph.automation[idx], lane);
        Ok(TimelineCommand::UpdateAutomationLane { lane: previous })
    } else {
        Err(TimelineError::LaneNotFound(lane.id))
    }
}

fn remove_lane(
    graph: &mut TimelineGraph,
    lane_id: LaneId,
) -> Result<TimelineCommand, TimelineError> {
    if let Some(idx) = graph.automation.iter().position(|l| l.id == lane_id) {
        let lane = graph.automation.remove(idx);
        Ok(TimelineCommand::AddAutomationLane { lane })
    } else {
        Err(TimelineError::LaneNotFound(lane_id))
    }
}

fn insert_keyframe(
    graph: &mut TimelineGraph,
    lane_id: LaneId,
    keyframe: AutomationKeyframe,
) -> Result<TimelineCommand, TimelineError> {
    let lane = graph
        .automation
        .iter_mut()
        .find(|l| l.id == lane_id)
        .ok_or(TimelineError::LaneNotFound(lane_id))?;

    let mut previous: Option<AutomationKeyframe> = None;
    if let Some(idx) = lane
        .keyframes
        .iter()
        .position(|k| k.frame == keyframe.frame)
    {
        previous = Some(std::mem::replace(
            &mut lane.keyframes[idx],
            keyframe.clone(),
        ));
    } else {
        lane.keyframes.push(keyframe.clone());
        lane.keyframes.sort_by_key(|k| k.frame);
    }

    let inverse = match previous {
        Some(old) => TimelineCommand::InsertAutomationKeyframe {
            lane_id,
            keyframe: old,
        },
        None => TimelineCommand::RemoveAutomationKeyframe {
            lane_id,
            frame: keyframe.frame,
        },
    };
    Ok(inverse)
}

fn remove_keyframe(
    graph: &mut TimelineGraph,
    lane_id: LaneId,
    frame: Frame,
) -> Result<TimelineCommand, TimelineError> {
    let lane = graph
        .automation
        .iter_mut()
        .find(|l| l.id == lane_id)
        .ok_or(TimelineError::LaneNotFound(lane_id))?;

    if let Some(idx) = lane.keyframes.iter().position(|k| k.frame == frame) {
        let removed = lane.keyframes.remove(idx);
        Ok(TimelineCommand::InsertAutomationKeyframe {
            lane_id,
            keyframe: removed,
        })
    } else {
        Err(TimelineError::InvalidOp(format!(
            "keyframe not found at frame {}",
            frame
        )))
    }
}

fn validate_placements(
    graph: &TimelineGraph,
    placements: &[TrackPlacement],
) -> Result<(), TimelineError> {
    for placement in placements {
        let track = graph
            .tracks
            .iter()
            .find(|t| t.id == placement.track_id)
            .ok_or(TimelineError::TrackNotFound(placement.track_id))?;
        if let Some(position) = placement.position {
            if position > track.node_ids.len() {
                return Err(TimelineError::InvalidOp(format!(
                    "placement index {} out of bounds for track {}",
                    position, track.id
                )));
            }
        }
    }
    Ok(())
}

fn validate_edges_for_insert(
    graph: &TimelineGraph,
    edges: &[TimelineEdge],
    new_node: NodeId,
) -> Result<(), TimelineError> {
    for edge in edges {
        if edge.from != new_node && !graph.nodes.contains_key(&edge.from) {
            return Err(TimelineError::NodeNotFound(edge.from));
        }
        if edge.to != new_node && !graph.nodes.contains_key(&edge.to) {
            return Err(TimelineError::NodeNotFound(edge.to));
        }
        if graph
            .edges
            .iter()
            .any(|e| e.from == edge.from && e.to == edge.to && e.kind == edge.kind)
        {
            return Err(TimelineError::EdgeExists(edge.from, edge.to));
        }
    }
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct CommandHistory {
    undo_stack: Vec<TimelineCommand>,
    redo_stack: Vec<TimelineCommand>,
}

impl CommandHistory {
    pub fn apply(
        &mut self,
        graph: &mut TimelineGraph,
        command: TimelineCommand,
    ) -> Result<(), TimelineError> {
        let inverse = apply_command(graph, command)?;
        self.undo_stack.push(inverse);
        self.redo_stack.clear();
        Ok(())
    }

    pub fn undo(&mut self, graph: &mut TimelineGraph) -> Result<(), TimelineError> {
        let command = self
            .undo_stack
            .pop()
            .ok_or(TimelineError::HistoryEmpty("undo stack"))?;
        let inverse = apply_command(graph, command)?;
        self.redo_stack.push(inverse);
        Ok(())
    }

    pub fn redo(&mut self, graph: &mut TimelineGraph) -> Result<(), TimelineError> {
        let command = self
            .redo_stack
            .pop()
            .ok_or(TimelineError::HistoryEmpty("redo stack"))?;
        let inverse = apply_command(graph, command)?;
        self.undo_stack.push(inverse);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }
}

pub fn migrate_sequence_tracks(sequence: &crate::Sequence) -> TimelineGraph {
    let mut result = TimelineGraph::default();
    result.version = sequence.graph.version;
    if sequence.graph.metadata != Value::Null {
        result.metadata = sequence.graph.metadata.clone();
    }

    for (track_index, legacy_track) in sequence.tracks.iter().enumerate() {
        let track_id = TrackId::new();
        let mut binding = TrackBinding {
            id: track_id,
            name: legacy_track.name.clone(),
            kind: {
                // Heuristics:
                // - Empty tracks: infer from name prefix ("A" -> Audio), else Video.
                // - Non-empty: Audio only if all items are audio; otherwise Video.
                if legacy_track.items.is_empty() {
                    if legacy_track.name.to_ascii_uppercase().starts_with('A') {
                        TrackKind::Audio
                    } else {
                        TrackKind::Video
                    }
                } else if legacy_track
                    .items
                    .iter()
                    .all(|item| matches!(item.kind, crate::ItemKind::Audio { .. }))
                {
                    TrackKind::Audio
                } else {
                    TrackKind::Video
                }
            },
            node_ids: Vec::new(),
        };

        for item in &legacy_track.items {
            let node_id = NodeId::new();
            let clip = crate::ClipNode {
                asset_id: match &item.kind {
                    crate::ItemKind::Video { src, .. } => Some(src.clone()),
                    crate::ItemKind::Audio { src, .. } => Some(src.clone()),
                    crate::ItemKind::Image { src, .. } => Some(src.clone()),
                    crate::ItemKind::Solid { .. } | crate::ItemKind::Text { .. } => None,
                },
                media_range: FrameRange::new(0, item.duration_in_frames),
                timeline_range: FrameRange::new(item.from, item.duration_in_frames),
                playback_rate: match &item.kind {
                    crate::ItemKind::Video { rate, .. } => *rate,
                    crate::ItemKind::Audio { rate, .. } => *rate,
                    _ => 1.0,
                },
                reverse: false,
                metadata: Value::Null,
            };
            let node = TimelineNode {
                id: node_id,
                label: Some(item.id.clone()),
                kind: match &item.kind {
                    crate::ItemKind::Solid { color } => TimelineNodeKind::Generator {
                        generator_id: "solid".to_string(),
                        timeline_range: FrameRange::new(item.from, item.duration_in_frames),
                        metadata: serde_json::json!({ "color": color }),
                    },
                    crate::ItemKind::Text { text, color } => TimelineNodeKind::Generator {
                        generator_id: "text".to_string(),
                        timeline_range: FrameRange::new(item.from, item.duration_in_frames),
                        metadata: serde_json::json!({ "text": text, "color": color }),
                    },
                    crate::ItemKind::Video { .. }
                    | crate::ItemKind::Audio { .. }
                    | crate::ItemKind::Image { .. } => TimelineNodeKind::Clip(clip),
                },
                locked: false,
                metadata: Value::Null,
            };
            result.nodes.insert(node_id, node);
            binding.node_ids.push(node_id);
        }

        result.tracks.push(binding);
    }

    result
}
