use crate::media_info::{HardwareCaps, MediaInfo, MediaKind};

/// Determines whether a proxy should be generated for the given media clip.
pub fn should_proxy(info: &MediaInfo, hw: &HardwareCaps) -> bool {
    if info.kind != MediaKind::Video {
        return false;
    }

    if info.is_intra_frame_codec() {
        return false;
    }

    let width = info.width.unwrap_or(0);
    let height = info.height.unwrap_or(0);
    let max_dim = width.max(height);
    let pixels = (width as u64) * (height as u64);
    let fps = info
        .fps
        .or_else(|| match (info.fps_num, info.fps_den) {
            (Some(n), Some(d)) if d != 0 => Some(n as f64 / d as f64),
            _ => None,
        })
        .unwrap_or(0.0);
    let bitrate = info.bitrate_mbps.unwrap_or(0.0);
    let duration_minutes = info.duration_seconds.unwrap_or(0.0) / 60.0;
    let codec = info.codec.as_deref().unwrap_or("").to_ascii_uppercase();

    // Light clips play directly.
    if max_dim <= 1080 && fps <= 30.0 && bitrate < 15.0 {
        return false;
    }

    let mut heavy = false;

    if max_dim >= 2160 || pixels >= 3_500_000 {
        heavy = true;
    }

    if fps >= 60.0 && width >= 1920 {
        heavy = true;
    }

    let inter_frame_codec = matches!(codec.as_str(), "HEVC" | "H265" | "AV1" | "VP9" | "VVC");
    if inter_frame_codec && bitrate >= 25.0 {
        heavy = true;
    }

    if info.is_inter_frame && duration_minutes >= 20.0 {
        heavy = true;
    }

    if info.is_variable_framerate && width >= 1920 {
        heavy = true;
    }

    if info.has_multiple_video_streams {
        heavy = true;
    }

    if codec == "HEVC" && info.bit_depth.unwrap_or(8) >= 10 && info.is_hdr {
        heavy = true;
    }

    let has_hw_hevc = hw
        .decoder_elements
        .iter()
        .any(|name| name.contains("265") || name.contains("hevc"));
    let has_hw_av1 = hw.decoder_elements.iter().any(|name| name.contains("av1"));
    let has_hw_vp9 = hw.decoder_elements.iter().any(|name| name.contains("vp9"));

    if codec == "HEVC" && !has_hw_hevc {
        heavy = true;
    }
    if codec == "HEVC" && info.bit_depth.unwrap_or(8) >= 10 && !hw.supports_hevc_10bit {
        heavy = true;
    }
    if codec == "AV1" && !has_hw_av1 {
        heavy = true;
    }
    if codec == "VP9" && !has_hw_vp9 {
        heavy = true;
    }

    if hw.prefers_prores_proxy && codec == "HEVC" && width >= 1920 {
        heavy = true;
    }

    heavy
}
