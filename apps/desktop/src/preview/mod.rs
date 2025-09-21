pub mod state;
pub mod ui;

pub(crate) use state::{PreviewShaderMode, PreviewState, StreamMetadata, StreamSlot};

use serde_json::Value;
use timeline_crate::{
    ClipNode, FrameRange, TimelineGraph, TimelineNode, TimelineNodeKind, TrackKind,
};

use crate::VisualSource;
use tracing::trace;

pub(crate) fn visual_source_at(graph: &TimelineGraph, playhead: i64) -> Option<VisualSource> {
    // Priority: lower-numbered tracks first (top-most rows in UI)
    for binding in graph.tracks.iter() {
        if matches!(binding.kind, TrackKind::Audio) {
            continue;
        }
        for node_id in binding.node_ids.iter() {
            // Skip missing nodes instead of aborting the entire search.
            let Some(node) = graph.nodes.get(node_id) else { continue };
            let Some(range) = node_frame_range(node) else { continue };
            if playhead < range.start || playhead >= range.end() { continue; }
            match &node.kind {
                TimelineNodeKind::Clip(clip) => {
                    let asset = clip.asset_id.as_deref().unwrap_or("<unknown>");
                    trace!(node_id = ?node_id, asset, playhead, "preview resolver matched clip");
                    if let Some(src) = clip_source(binding, clip) {
                        return Some(src);
                    }
                }
                TimelineNodeKind::Generator { generator_id, metadata, .. } => {
                    if let Some(src) = generator_source(generator_id, metadata) { return Some(src); }
                }
                _ => {}
            }
        }
    }
    None
}

fn node_frame_range(node: &TimelineNode) -> Option<FrameRange> {
    match &node.kind {
        TimelineNodeKind::Clip(clip) => Some(clip.timeline_range.clone()),
        TimelineNodeKind::Generator { timeline_range, .. } => Some(timeline_range.clone()),
        _ => None,
    }
}

fn clip_source(binding: &timeline_crate::TrackBinding, clip: &ClipNode) -> Option<VisualSource> {
    let path = clip.asset_id.clone()?;
    // Detect images by extension or by explicit image track kind
    let ext_is_image = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.eq_ignore_ascii_case("png")
            || s.eq_ignore_ascii_case("jpg")
            || s.eq_ignore_ascii_case("jpeg")
            || s.eq_ignore_ascii_case("gif")
            || s.eq_ignore_ascii_case("webp")
            || s.eq_ignore_ascii_case("bmp")
            || s.eq_ignore_ascii_case("tif")
            || s.eq_ignore_ascii_case("tiff")
            || s.eq_ignore_ascii_case("exr"))
        .unwrap_or(false);
    let track_hint_image = matches!(binding.kind, TrackKind::Custom(ref id) if id == "image");
    let is_image = ext_is_image || track_hint_image;
    Some(VisualSource { path, is_image })
}

fn generator_source(generator_id: &str, metadata: &Value) -> Option<VisualSource> {
    match generator_id {
        "solid" => {
            let color = metadata
                .get("color")
                .and_then(|v| v.as_str())
                .unwrap_or("#000000");
            Some(VisualSource {
                path: format!("solid:{}", color),
                is_image: true,
            })
        }
        "text" => Some(VisualSource {
            path: "text://generator".into(),
            is_image: true,
        }),
        _ => None,
    }
}
