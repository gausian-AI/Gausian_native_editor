use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_pbutils as gstpb;
use gstreamer_pbutils::prelude::*;
use media_io::{self, MediaKind as LegacyKind};
use once_cell::sync::OnceCell;
use serde::Serialize;

/// Expanded metadata captured for an imported media asset.
#[derive(Debug, Clone, Serialize)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub fps_num: Option<u32>,
    pub fps_den: Option<u32>,
    pub fps: Option<f64>,
    pub is_variable_framerate: bool,
    pub codec: Option<String>,
    pub codec_profile: Option<String>,
    pub bitrate_mbps: Option<f64>,
    pub bit_depth: Option<u32>,
    pub is_hdr: bool,
    pub is_inter_frame: bool,
    pub audio_channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub has_alpha: bool,
    pub has_multiple_video_streams: bool,
    pub file_size_bytes: Option<u64>,
}

impl MediaInfo {
    pub fn is_intra_frame_codec(&self) -> bool {
        self.codec
            .as_deref()
            .map(|codec| matches!(codec, "ProRes" | "DNxHR" | "DNxHD"))
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Image,
    Audio,
}

/// Hardware decode capabilities detected from available GStreamer elements.
#[derive(Debug, Clone)]
pub struct HardwareCaps {
    pub decoder_elements: Vec<String>,
    pub supports_hevc_10bit: bool,
    pub supports_prores_proxy: bool,
    pub supports_hdr_upload: bool,
    pub prefers_prores_proxy: bool,
    pub logical_cores: usize,
}

static GST_INIT: OnceCell<Result<(), gst::glib::Error>> = OnceCell::new();

fn ensure_gstreamer_initialized() -> Result<()> {
    GST_INIT
        .get_or_init(|| gst::init())
        .clone()
        .map_err(|e| anyhow!("failed to initialise GStreamer: {e}"))
}

pub fn probe_media_info(path: &Path) -> Result<MediaInfo> {
    let legacy = media_io::probe_media(path).ok();

    let mut kind = legacy
        .as_ref()
        .map(|l| match l.kind {
            LegacyKind::Video => MediaKind::Video,
            LegacyKind::Image => MediaKind::Image,
            LegacyKind::Audio => MediaKind::Audio,
        })
        .unwrap_or(MediaKind::Video);

    let mut width = legacy.as_ref().and_then(|l| l.width);
    let mut height = legacy.as_ref().and_then(|l| l.height);
    let mut duration_seconds = legacy.as_ref().and_then(|l| l.duration_seconds);
    let mut fps_num = legacy.as_ref().and_then(|l| l.fps_num);
    let mut fps_den = legacy.as_ref().and_then(|l| l.fps_den);
    let mut audio_channels = legacy.as_ref().and_then(|l| l.audio_channels);
    let mut sample_rate = legacy.as_ref().and_then(|l| l.sample_rate);

    let mut codec = None;
    let mut codec_profile = None;
    let mut bitrate_mbps = None;
    let mut bit_depth = None;
    let mut is_hdr = false;
    let mut is_variable = false;
    let mut is_inter_frame = true;
    let mut has_alpha = false;
    let mut has_multiple_video_streams = false;

    if ensure_gstreamer_initialized().is_ok() {
        if let Ok(discoverer) = gstpb::Discoverer::new(gst::ClockTime::from_seconds(15)) {
            if let Ok(uri) = url::Url::from_file_path(path) {
                match discoverer.discover_uri(uri.as_str()) {
                    Ok(info) => {
                        if duration_seconds.is_none() {
                            duration_seconds = info
                                .duration()
                                .map(|d| d.nseconds() as f64 / 1_000_000_000.0);
                        }

                        let video_streams = info.video_streams();
                        has_multiple_video_streams = video_streams.len() > 1;

                        if let Some(primary) = video_streams.first() {
                            if width.is_none() {
                                width = Some(primary.width().max(1));
                            }
                            if height.is_none() {
                                height = Some(primary.height().max(1));
                            }
                            if bitrate_mbps.is_none() {
                                let bitrate = primary.bitrate();
                                if bitrate > 0 {
                                    bitrate_mbps = Some(bitrate as f64 / 1_000_000.0);
                                }
                            }
                            if bit_depth.is_none() {
                                let depth = primary.depth();
                                if depth > 0 {
                                    bit_depth = Some(depth);
                                }
                            }
                            if let Some(caps) = primary.caps() {
                                if let Some(structure) = caps.structure(0) {
                                    if codec.is_none() {
                                        codec =
                                            Some(normalize_caps_name(structure.name().as_str()));
                                    }
                                    if codec_profile.is_none() {
                                        codec_profile = structure
                                            .get::<&str>("profile")
                                            .ok()
                                            .map(|p| p.to_string());
                                    }
                                    if bit_depth.is_none() {
                                        if let Ok(format) = structure.get::<&str>("format") {
                                            bit_depth = detect_bit_depth(format);
                                            has_alpha = format.contains('A');
                                        }
                                    }
                                    if !is_hdr {
                                        if let Ok(colorimetry) =
                                            structure.get::<&str>("colorimetry")
                                        {
                                            is_hdr = detect_hdr_from_colorimetry(colorimetry);
                                        }
                                    }
                                    if structure.has_field("max-framerate") {
                                        is_variable = true;
                                    }
                                }
                            }
                        }

                        let audio_streams = info.audio_streams();
                        if !audio_streams.is_empty() {
                            if kind == MediaKind::Image && video_streams.is_empty() {
                                kind = MediaKind::Audio;
                            }
                            if audio_channels.is_none() {
                                audio_channels = Some(audio_streams[0].channels() as u32);
                            }
                            if sample_rate.is_none() {
                                sample_rate = Some(audio_streams[0].sample_rate() as u32);
                            }
                        } else if video_streams.is_empty() {
                            kind = MediaKind::Audio;
                        }

                        if codec.is_none() {
                            codec = info
                                .stream_info()
                                .and_then(|stream| stream.caps())
                                .and_then(|caps| {
                                    caps.structure(0)
                                        .map(|s| normalize_caps_name(s.name().as_str()))
                                });
                        }
                    }
                    Err(err) => {
                        tracing::warn!("GStreamer discoverer failed for {}: {err}", path.display());
                    }
                }
            }
        }
    }

    if bitrate_mbps.is_none() {
        if let Some(duration) = duration_seconds {
            if duration > 0.0 {
                if let Ok(meta) = fs::metadata(path) {
                    let bits = (meta.len() as f64) * 8.0;
                    bitrate_mbps = Some(bits / duration / 1_000_000.0);
                }
            }
        }
    }

    let codec_name = codec
        .as_ref()
        .map(|c| c.to_ascii_lowercase())
        .unwrap_or_default();

    if codec_name.contains("prores") || codec_name.contains("dnx") {
        is_inter_frame = false;
    }

    if codec_name.contains("hevc") || codec_name.contains("h265") {
        if let Some(depth) = bit_depth {
            if depth >= 10 {
                is_hdr = true;
            }
        }
    }

    let file_size_bytes = fs::metadata(path).ok().map(|m| m.len());
    let fps = match (fps_num, fps_den) {
        (Some(n), Some(d)) if d != 0 => Some(n as f64 / d as f64),
        _ => None,
    };

    let is_variable_framerate = is_variable
        || fps_num.map(|v| v == 0).unwrap_or(false)
        || fps_den.map(|v| v == 0).unwrap_or(false);

    Ok(MediaInfo {
        path: path.to_path_buf(),
        kind,
        width,
        height,
        duration_seconds,
        fps_num,
        fps_den,
        fps,
        is_variable_framerate,
        codec,
        codec_profile,
        bitrate_mbps,
        bit_depth,
        is_hdr,
        is_inter_frame,
        audio_channels,
        sample_rate,
        has_alpha,
        has_multiple_video_streams,
        file_size_bytes,
    })
}

pub fn detect_hardware_caps() -> HardwareCaps {
    let mut decoder_elements = Vec::new();
    let mut supports_hevc_10bit = false;
    let mut supports_prores_proxy = cfg!(target_os = "macos");
    let mut supports_hdr_upload = cfg!(target_os = "macos");

    if ensure_gstreamer_initialized().is_ok() {
        let known_decoders = [
            "vtdec_h264",
            "vtdec_h265",
            "vtdec_h265_10bit",
            "d3d11h264dec",
            "d3d11h265dec",
            "nvh264dec",
            "nvh265dec",
            "vaapih264dec",
            "vaapih265dec",
            "av1dec",
        ];

        for name in known_decoders {
            if gst::ElementFactory::find(name).is_some() {
                decoder_elements.push(name.to_string());
                if name.contains("265") || name.contains("hevc") {
                    supports_hevc_10bit = true;
                }
            }
        }

        if gst::ElementFactory::find("vtproresdec").is_some() {
            supports_prores_proxy = true;
        }

        if gst::ElementFactory::find("vtpixelformatter") // macOS color conversion
            .or_else(|| gst::ElementFactory::find("d3d11convert"))
            .or_else(|| gst::ElementFactory::find("vaapipostproc"))
            .is_some()
        {
            supports_hdr_upload = true;
        }
    }

    let logical = num_cpus::get_physical().max(1);
    HardwareCaps {
        decoder_elements,
        supports_hevc_10bit,
        supports_prores_proxy,
        supports_hdr_upload,
        prefers_prores_proxy: cfg!(target_os = "macos"),
        logical_cores: logical,
    }
}

fn detect_bit_depth(format: &str) -> Option<u32> {
    if format.contains("10") {
        Some(10)
    } else if format.contains("12") {
        Some(12)
    } else if format.contains("16") {
        Some(16)
    } else if format.to_ascii_lowercase().contains("p010") {
        Some(10)
    } else {
        None
    }
}

fn detect_hdr_from_colorimetry(colorimetry: &str) -> bool {
    let lower = colorimetry.to_ascii_lowercase();
    lower.contains("st2084")
        || lower.contains("hlg")
        || lower.contains("pq")
        || lower.contains("bt2020")
}

fn normalize_caps_name(name: &str) -> String {
    match name {
        "video/x-h264" => "H264".to_string(),
        "video/x-h265" | "video/x-hevc" => "HEVC".to_string(),
        "video/x-h266" => "VVC".to_string(),
        "video/x-vp9" => "VP9".to_string(),
        "video/x-av1" => "AV1".to_string(),
        "video/x-prores" => "ProRes".to_string(),
        "video/x-dnxhd" | "video/x-dnxhr" => "DNxHR".to_string(),
        other => other.replace("video/x-", "").to_uppercase(),
    }
}
