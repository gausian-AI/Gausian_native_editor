//! GStreamer-backed decoder implementing the NativeVideoDecoder trait.
//!
//! This backend targets NV12 output via appsink and maps frames into the
//! existing `VideoFrame { y_plane, uv_plane, .. }` structure. It keeps
//! the API surface identical to the existing VideoToolbox decoder so the
//! desktop app can switch via the feature flag with no UI changes.

#![cfg(feature = "gstreamer")]

#[cfg(target_os = "macos")]
use crate::IOSurfaceFrame;
use crate::{DecoderConfig, NativeVideoDecoder, VideoFrame, VideoProperties, YuvPixFmt};
use anyhow::{anyhow, Context, Result};
use std::env;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
use std::sync::{Mutex, Once};
use tracing::{debug, info, warn};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_pbutils as gst_pbutils;
use gstreamer_pbutils::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::VideoFrameExt; // plane_stride/plane_data access
#[cfg(target_os = "macos")]
use io_surface::{IOSurface, IOSurfaceRef};

#[cfg(target_os = "macos")]
use core_foundation::base::TCFType;
#[cfg(target_os = "macos")]
use core_video::pixel_buffer::{CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferRef};
#[cfg(target_os = "macos")]
use core_video::pixel_buffer_io_surface::CVPixelBufferGetIOSurface;
#[cfg(target_os = "macos")]
use gst::glib;
#[cfg(target_os = "macos")]
use std::ffi::{c_void, CStr};

// Initialize GStreamer once per process.
static GST_INIT_ONCE: AtomicBool = AtomicBool::new(false);

/// Description of the decoder element selected for hardware acceleration.
#[derive(Debug, Clone, Copy)]
pub struct DecoderSelection {
    pub factory_name: &'static str,
    pub description: &'static str,
    pub is_hardware: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoCodecKind {
    H264,
    H265,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    ProRes,
    Other,
}

#[derive(Debug, Clone)]
struct VideoCodecInfo {
    kind: VideoCodecKind,
    caps_name: Option<String>,
}

struct GstPipelineBundle {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    zero_copy: bool,
}

#[cfg(target_os = "macos")]
fn build_pipeline_macos(
    path: &Path,
    codec_info: &VideoCodecInfo,
    config: &DecoderConfig,
) -> Result<GstPipelineBundle> {
    let kind = codec_info.kind;
    let want_zero_copy = config.zero_copy
        && matches!(
            kind,
            VideoCodecKind::H264 | VideoCodecKind::H265 | VideoCodecKind::ProRes
        );

    if want_zero_copy {
        match build_macos_vt_gl_pipeline(path, codec_info, kind) {
            Ok(bundle) => return Ok(bundle),
            Err(err) => {
                warn!(
                    error = ?err,
                    codec = ?codec_info.caps_name,
                    "failed to build macOS zero-copy pipeline, falling back"
                );
            }
        }
    }

    let (pipeline, sink, zero_copy) = match kind {
        VideoCodecKind::H264 => {
            let (pipeline, sink) =
                build_macos_h26x_pipeline(path, codec_info, "h264parse", VideoCodecKind::H264)?;
            (pipeline, sink, false)
        }
        VideoCodecKind::H265 => {
            let (pipeline, sink) =
                build_macos_h26x_pipeline(path, codec_info, "h265parse", VideoCodecKind::H265)?;
            (pipeline, sink, false)
        }
        VideoCodecKind::ProRes => match build_macos_prores_pipeline(path, codec_info) {
            Ok((pipeline, sink)) => (pipeline, sink, false),
            Err(err) => {
                warn!(
                    error = ?err,
                    "failed to build macOS ProRes pipeline; falling back to generic decodebin"
                );
                let (pipeline, sink) = build_generic_pipeline(path, codec_info)?;
                (pipeline, sink, false)
            }
        },
        VideoCodecKind::Other => {
            let (pipeline, sink) = build_generic_pipeline(path, codec_info)?;
            (pipeline, sink, false)
        }
    };

    Ok(GstPipelineBundle {
        pipeline,
        sink,
        zero_copy,
    })
}

#[cfg(target_os = "macos")]
fn build_macos_vt_gl_pipeline(
    path: &Path,
    codec_info: &VideoCodecInfo,
    kind: VideoCodecKind,
) -> Result<GstPipelineBundle> {
    let parser_name = match kind {
        VideoCodecKind::H264 => "h264parse",
        VideoCodecKind::H265 => "h265parse",
        VideoCodecKind::ProRes => "proresparse",
        VideoCodecKind::Other => anyhow::bail!("unsupported codec for VT zero-copy path"),
    };

    let pipeline = gst::Pipeline::with_name("gst-native-decoder-vt-zero-copy");
    let location = path.to_string_lossy().to_string();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", &location)
        .build()
        .with_context(|| format!("make filesrc for {}", path.display()))?;

    let demux = gst::ElementFactory::make("qtdemux")
        .build()
        .context("make qtdemux")?;

    let parser = gst::ElementFactory::make(parser_name)
        .build()
        .with_context(|| format!("make {}", parser_name))?;
    let _ = parser.set_property("config-interval", &-1i32);

    let decoder = gst::ElementFactory::make("vtdec_hw")
        .build()
        .context("make vtdec_hw decoder")?;

    let glupload = gst::ElementFactory::make("glupload")
        .build()
        .context("make glupload")?;

    let glcolor = gst::ElementFactory::make("glcolorconvert")
        .build()
        .context("make glcolorconvert")?;

    let queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 6u32)
        .build()
        .context("make queue")?;

    let caps =
        gst::Caps::from_str("video/x-raw(memory:GLMemory),format=NV12").context("GLMemory caps")?;

    let appsink = gst_app::AppSink::builder()
        .caps(&caps)
        .drop(true)
        .sync(false)
        .max_buffers(6)
        .build();

    info!(
        codec = codec_info.caps_name.as_deref().unwrap_or("unknown"),
        parser = parser_name,
        "macOS GStreamer zero-copy pipeline using vtdec_hw -> GLMemory"
    );

    pipeline.add_many(&[
        &src,
        &demux,
        &parser,
        &decoder,
        &glupload,
        &glcolor,
        &queue,
        appsink.upcast_ref(),
    ])?;

    gst::Element::link_many(&[&src, &demux])
        .context("link filesrc->qtdemux for zero-copy pipeline")?;
    gst::Element::link_many(&[
        &parser,
        &decoder,
        &glupload,
        &glcolor,
        &queue,
        appsink.upcast_ref(),
    ])
    .context("link parser->vtdec_hw->glupload->glcolorconvert->queue->appsink")?;

    let parser_weak = parser.downgrade();
    demux.connect_pad_added(move |_demux, src_pad| {
        let Some(parser) = parser_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = parser.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if !pad_is_video(src_pad) {
            debug!(pad = %src_pad.name(), "ignoring non-video pad from qtdemux");
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            warn!(
                pad = %src_pad.name(),
                error = ?err,
                "failed to link qtdemux video pad to parser for zero-copy pipeline"
            );
        }
    });

    Ok(GstPipelineBundle {
        pipeline,
        sink: appsink,
        zero_copy: true,
    })
}
#[cfg(target_os = "macos")]
#[repr(C)]
struct GstCoreVideoMeta {
    meta: gst::ffi::GstMeta,
    cvbuf: *mut c_void,
    pixbuf: CVPixelBufferRef,
}

#[cfg(target_os = "macos")]
fn find_core_video_meta_ptr(buffer: &gst::BufferRef) -> Option<*const GstCoreVideoMeta> {
    let mut state: glib::ffi::gpointer = std::ptr::null_mut();
    loop {
        let meta = unsafe { gst::ffi::gst_buffer_iterate_meta(buffer.as_mut_ptr(), &mut state) };
        if meta.is_null() {
            return None;
        }
        let info = unsafe { (*meta).info };
        if info.is_null() {
            continue;
        }
        let type_id = unsafe { (*info).type_ };
        if type_id == 0 {
            continue;
        }
        let type_name_ptr = unsafe { glib::gobject_ffi::g_type_name(type_id) };
        if type_name_ptr.is_null() {
            continue;
        }
        if let Ok(name) = unsafe { CStr::from_ptr(type_name_ptr) }.to_str() {
            if name == "GstAppleCoreVideoMeta" {
                return Some(meta as *const GstCoreVideoMeta);
            }
        }
    }
}

fn pad_is_video(pad: &gst::Pad) -> bool {
    pad.current_caps()
        .or_else(|| Some(pad.query_caps(None)))
        .and_then(|caps| {
            caps.structure(0).map(|s| {
                let name = s.name();
                name.starts_with("video/")
            })
        })
        .unwrap_or_else(|| pad.name().starts_with("video"))
}

fn detect_video_codec(path: &Path) -> VideoCodecInfo {
    let uri = match gst::glib::filename_to_uri(path, None) {
        Ok(uri) => uri.to_string(),
        Err(_) => {
            return VideoCodecInfo {
                kind: VideoCodecKind::Other,
                caps_name: None,
            }
        }
    };

    if let Ok(discoverer) = gst_pbutils::Discoverer::new(gst::ClockTime::from_seconds(5)) {
        if let Ok(info) = discoverer.discover_uri(&uri) {
            if let Some(video_info) = info.video_streams().into_iter().next() {
                if let Some(caps) = video_info.caps() {
                    if let Some(structure) = caps.structure(0) {
                        let name = structure.name().to_string();
                        let kind = match name.as_str() {
                            "video/x-h264" => VideoCodecKind::H264,
                            "video/x-h265" | "video/x-265" => VideoCodecKind::H265,
                            "video/x-prores" => VideoCodecKind::ProRes,
                            _ => VideoCodecKind::Other,
                        };
                        return VideoCodecInfo {
                            kind,
                            caps_name: Some(name),
                        };
                    }
                }
            }
        }
    }

    VideoCodecInfo {
        kind: VideoCodecKind::Other,
        caps_name: None,
    }
}

fn make_decodebin_element() -> Result<gst::Element> {
    if let Ok(elem) = gst::ElementFactory::make("decodebin3").build() {
        return Ok(elem);
    }

    gst::ElementFactory::make("decodebin")
        .build()
        .context("make decodebin fallback")
}

fn is_element_available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

fn select_from_priority(priority: &[(&'static str, &'static str, bool)]) -> DecoderSelection {
    for &(factory_name, description, is_hardware) in priority {
        if is_element_available(factory_name) {
            return DecoderSelection {
                factory_name,
                description,
                is_hardware,
            };
        }
    }
    let &(factory_name, description, is_hardware) =
        priority
            .last()
            .unwrap_or(&("avdec_h264", "Software libavcodec H.264 decoder", false));
    DecoderSelection {
        factory_name,
        description,
        is_hardware,
    }
}

#[cfg(target_os = "windows")]
#[cfg(target_os = "windows")]
fn select_decoder_factory(kind: VideoCodecKind) -> DecoderSelection {
    match kind {
        VideoCodecKind::H265 => {
            const PRIORITY: [(&str, &str, bool); 4] = [
                ("nvh265dec", "NVIDIA NVDEC hardware decoder (HEVC)", true),
                (
                    "d3d11h265dec",
                    "Direct3D11 DXVA2 hardware decoder (HEVC)",
                    true,
                ),
                (
                    "qsvh265dec",
                    "Intel Quick Sync Video hardware decoder (HEVC)",
                    true,
                ),
                ("avdec_hevc", "libavcodec software HEVC decoder", false),
            ];
            select_from_priority(&PRIORITY)
        }
        _ => {
            const PRIORITY: [(&str, &str, bool); 4] = [
                ("nvdec", "NVIDIA NVDEC hardware decoder (nvcodec)", true),
                (
                    "qsvh264dec",
                    "Intel Quick Sync Video hardware decoder",
                    true,
                ),
                ("d3d11h264dec", "Direct3D11 DXVA2 hardware decoder", true),
                ("avdec_h264", "libavcodec software H.264 decoder", false),
            ];
            select_from_priority(&PRIORITY)
        }
    }
}

#[cfg(target_os = "linux")]
fn select_decoder_factory(kind: VideoCodecKind) -> DecoderSelection {
    match kind {
        VideoCodecKind::H265 => {
            const PRIORITY: [(&str, &str, bool); 3] = [
                (
                    "nvv4l2decoder",
                    "NVIDIA NVDEC hardware decoder (nvv4l2decoder HEVC)",
                    true,
                ),
                ("vah265dec", "VA-API hardware decoder (HEVC)", true),
                ("avdec_hevc", "libavcodec software HEVC decoder", false),
            ];
            select_from_priority(&PRIORITY)
        }
        _ => {
            const PRIORITY: [(&str, &str, bool); 3] = [
                (
                    "nvv4l2decoder",
                    "NVIDIA NVDEC hardware decoder (nvv4l2decoder)",
                    true,
                ),
                ("vah264dec", "VA-API hardware decoder", true),
                ("avdec_h264", "libavcodec software H.264 decoder", false),
            ];
            select_from_priority(&PRIORITY)
        }
    }
}

#[cfg(target_os = "macos")]
fn select_decoder_factory(kind: VideoCodecKind) -> DecoderSelection {
    match kind {
        VideoCodecKind::H265 => {
            const PRIORITY: [(&str, &str, bool); 3] = [
                ("vtdec_hevc", "VideoToolbox HEVC hardware decoder", true),
                ("avdec_hevc", "libavcodec software HEVC decoder", false),
                ("avdec_h264", "libavcodec software H.264 decoder", false),
            ];
            select_from_priority(&PRIORITY)
        }
        _ => {
            const PRIORITY: [(&str, &str, bool); 3] = [
                ("vtdec_h264", "VideoToolbox H.264 hardware decoder", true),
                ("vtdec_hw", "VideoToolbox hardware decoder (generic)", true),
                ("avdec_h264", "libavcodec software H.264 decoder", false),
            ];
            select_from_priority(&PRIORITY)
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn select_decoder_factory(_kind: VideoCodecKind) -> DecoderSelection {
    const PRIORITY: [(&str, &str, bool); 1] =
        [("avdec_h264", "libavcodec software H.264 decoder", false)];
    select_from_priority(&PRIORITY)
}

fn ensure_gst_init() -> Result<()> {
    if !GST_INIT_ONCE.load(Ordering::SeqCst) {
        gst::init().map_err(|e| anyhow!("gst::init() failed: {e}"))?;
        // On macOS, prefer VideoToolbox decoders over software avdec
        #[cfg(target_os = "macos")]
        {
            let reg = gst::Registry::get();
            let promote = |name: &str| {
                if let Some(f) = reg.find_feature(name, gst::ElementFactory::static_type()) {
                    f.set_rank(gst::Rank::PRIMARY + 100);
                }
            };
            promote("vtdec_h264");
            promote("vtdec_hevc");
            if let Some(f) = gst::ElementFactory::find("vtdec_hw") {
                info!(rank = ?f.rank(), "Found vtdec_hw");
            } else {
                warn!("vtdec_hw not found; zero-copy pipeline will fall back to CPU");
            }
        }
        if env::var_os("GST_PLUGIN_FEATURE_RANK").is_some() {
            info!("GST_PLUGIN_FEATURE_RANK detected; custom feature ranks will be honoured.");
        }
        if env::var_os("GST_DECODER_DIAG").is_some() {
            info!("GST_DECODER_DIAG set; dumping decoder inventory");
            log_decoder_inventory();
        }
        GST_INIT_ONCE.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// Determine the highest-priority decoder available on the current platform.
pub fn select_best_decoder() -> Result<DecoderSelection> {
    ensure_gst_init()?;
    Ok(select_decoder_factory(VideoCodecKind::H264))
}

fn select_decoder_for_codec(kind: VideoCodecKind) -> Result<DecoderSelection> {
    ensure_gst_init()?;
    Ok(select_decoder_factory(kind))
}

fn log_decoder_inventory() {
    static INVENTORY_ONCE: Once = Once::new();
    INVENTORY_ONCE.call_once(|| {
        let candidates = [
            "vtdec_h264",
            "vtdec_hevc",
            "vtdec_hw",
            "avdec_h264",
            "avdec_hevc",
            "nvdec",
            "nvh264dec",
            "nvh265dec",
            "d3d11h264dec",
            "d3d11h265dec",
            "qsvh264dec",
            "qsvh265dec",
            "nvv4l2decoder",
            "vah264dec",
            "vah265dec",
        ];
        for name in candidates {
            if let Some(feature) = gst::ElementFactory::find(name) {
                info!(
                    element = name,
                    rank = ?feature.rank(),
                    "decoder element available"
                );
            } else {
                debug!(element = name, "decoder element unavailable");
            }
        }
    });
}

/// Provide a human-readable description of the preferred decoder for this platform.
pub fn describe_platform_decoder() -> Result<String> {
    let selection = select_best_decoder()?;
    let mode = if selection.is_hardware {
        "hardware acceleration"
    } else {
        "software fallback"
    };
    Ok(format!(
        "{} ({}, {})",
        selection.factory_name, selection.description, mode
    ))
}

fn build_pipeline(path: &Path, config: &DecoderConfig) -> Result<GstPipelineBundle> {
    if !path.exists() {
        return Err(anyhow!(
            "input video path does not exist: {}",
            path.display()
        ));
    }

    let codec_info = detect_video_codec(path);

    #[cfg(target_os = "macos")]
    {
        return build_pipeline_macos(path, &codec_info, config);
    }

    #[cfg(not(target_os = "macos"))]
    {
        build_pipeline_non_macos(path, &codec_info)
    }
}

#[cfg(not(target_os = "macos"))]
fn build_pipeline_non_macos(path: &Path, codec_info: &VideoCodecInfo) -> Result<GstPipelineBundle> {
    match codec_info.kind {
        VideoCodecKind::H264 => {
            let (pipeline, sink) =
                build_h26x_pipeline_generic(path, codec_info, "h264parse", VideoCodecKind::H264)?;
            Ok(GstPipelineBundle {
                pipeline,
                sink,
                zero_copy: false,
            })
        }
        VideoCodecKind::H265 => {
            let (pipeline, sink) =
                build_h26x_pipeline_generic(path, codec_info, "h265parse", VideoCodecKind::H265)?;
            Ok(GstPipelineBundle {
                pipeline,
                sink,
                zero_copy: false,
            })
        }
        VideoCodecKind::ProRes | VideoCodecKind::Other => {
            let (pipeline, sink) = build_generic_pipeline(path, codec_info)?;
            Ok(GstPipelineBundle {
                pipeline,
                sink,
                zero_copy: false,
            })
        }
    }
}

#[cfg(target_os = "macos")]
fn build_macos_prores_pipeline(
    path: &Path,
    codec_info: &VideoCodecInfo,
) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    info!(
        codec = codec_info.caps_name.as_deref().unwrap_or("video/x-prores"),
        "macOS GStreamer pipeline selecting ProRes decoder"
    );

    let pipeline = gst::Pipeline::with_name("gst-native-decoder-prores");
    let location = path.to_string_lossy().to_string();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", &location)
        .build()
        .with_context(|| format!("make filesrc for {}", path.display()))?;

    let demux = gst::ElementFactory::make("qtdemux")
        .build()
        .context("make qtdemux")?;

    let queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 8u32)
        .build()
        .context("make queue")?;

    let parser_available = gst::ElementFactory::find("proresparse").is_some();
    let parser = if parser_available {
        let elem = gst::ElementFactory::make("proresparse")
            .build()
            .context("make proresparse")?;
        let _ = elem.set_property("config-interval", &-1i32);
        Some(elem)
    } else {
        warn!("proresparse unavailable; linking qtdemux directly to decoder");
        None
    };

    let decoder = match gst::ElementFactory::make("vtdec_hw").build() {
        Ok(elem) => elem,
        Err(err) => {
            warn!(
                error = %err,
                "vtdec_hw unavailable for ProRes; trying vtdec fallback"
            );
            gst::ElementFactory::make("vtdec")
                .build()
                .context("make vtdec fallback")?
        }
    };

    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("make videoconvert")?;

    let caps = gst::Caps::builder("video/x-raw")
        .field("format", &"NV12")
        .field("interlace-mode", &"progressive")
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()
        .context("make capsfilter")?;

    let appsink = gst_app::AppSink::builder()
        .caps(&caps)
        .max_buffers(8)
        .drop(true)
        .build();
    let appsink_elem = appsink.upcast_ref();

    pipeline.add_many(&[
        &src,
        &demux,
        &queue,
        &decoder,
        &convert,
        &capsfilter,
        appsink_elem,
    ])?;
    if let Some(ref parser_elem) = parser {
        pipeline.add(parser_elem)?;
    }

    let mut downstream_chain: Vec<&gst::Element> = vec![&queue];
    if let Some(ref parser_elem) = parser {
        downstream_chain.push(parser_elem);
    }
    downstream_chain.push(&decoder);
    downstream_chain.push(&convert);
    downstream_chain.push(&capsfilter);
    downstream_chain.push(appsink_elem);

    gst::Element::link_many(&downstream_chain)
        .context("link queue->[proresparse]->decoder->videoconvert->capsfilter->appsink")?;
    src.link(&demux).context("link filesrc->qtdemux (ProRes)")?;

    let queue_weak = queue.downgrade();
    demux.connect_pad_added(move |_demux, src_pad| {
        let Some(queue) = queue_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = queue.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if !pad_is_video(src_pad) {
            debug!(pad = %src_pad.name(), "ignoring non-video pad from qtdemux (ProRes)");
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            warn!(
                pad = %src_pad.name(),
                error = ?err,
                "failed to link qtdemux video pad to queue (ProRes pipeline)"
            );
        }
    });

    Ok((pipeline, appsink))
}

#[cfg(target_os = "macos")]
fn build_macos_h26x_pipeline(
    path: &Path,
    codec_info: &VideoCodecInfo,
    parser_name: &str,
    kind: VideoCodecKind,
) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    let selection = select_decoder_for_codec(kind)?;
    info!(
        decoder = selection.factory_name,
        hardware = selection.is_hardware,
        desc = selection.description,
        codec = codec_info
            .caps_name
            .as_deref()
            .unwrap_or_else(|| match kind {
                VideoCodecKind::H265 => "video/x-h265",
                VideoCodecKind::H264 => "video/x-h264",
                VideoCodecKind::ProRes => "video/x-prores",
                VideoCodecKind::Other => "video/x-raw",
            }),
        "macOS GStreamer pipeline selecting decoder"
    );

    let pipeline = gst::Pipeline::with_name("gst-native-decoder");
    let location = path.to_string_lossy().to_string();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", &location)
        .build()
        .with_context(|| format!("make filesrc for {}", path.display()))?;

    let demux = gst::ElementFactory::make("qtdemux")
        .build()
        .context("make qtdemux")?;

    let queue = gst::ElementFactory::make("queue")
        .build()
        .context("make queue")?;

    let parser = gst::ElementFactory::make(parser_name)
        .build()
        .with_context(|| format!("make {}", parser_name))?;
    let _ = parser.set_property("config-interval", &-1i32);

    let decoder = gst::ElementFactory::make(selection.factory_name)
        .build()
        .with_context(|| format!("make decoder {}", selection.factory_name))?;

    let caps_str = if selection.is_hardware {
        "video/x-raw,interlace-mode=progressive"
    } else {
        "video/x-raw,interlace-mode=progressive"
    };
    let caps =
        gst::Caps::from_str(caps_str).with_context(|| format!("parse caps string {}", caps_str))?;

    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()
        .context("make capsfilter")?;

    let mut convert = None;
    if !selection.is_hardware {
        convert = Some(
            gst::ElementFactory::make("videoconvert")
                .build()
                .context("make videoconvert fallback")?,
        );
    }

    let caps_for_sink = caps.clone();
    info!(caps = %caps_for_sink, "decoder target caps for appsink");
    let appsink = gst_app::AppSink::builder()
        .caps(&caps_for_sink)
        .max_buffers(8)
        .drop(true)
        .build();

    info!(caps = %caps_for_sink, "macOS decoder target caps for appsink");

    pipeline.add_many(&[&src, &demux, &queue, &parser, &decoder])?;
    if let Some(ref convert_elem) = convert {
        pipeline.add(convert_elem)?;
    }
    pipeline.add_many(&[&capsfilter, appsink.upcast_ref()])?;

    let mut link_chain: Vec<&gst::Element> = vec![&queue, &parser, &decoder];
    if let Some(ref convert_elem) = convert {
        link_chain.push(convert_elem);
    }
    link_chain.push(&capsfilter);
    link_chain.push(appsink.upcast_ref());

    gst::Element::link_many(&link_chain)
        .context("link queue->parser->decoder->[videoconvert]->capsfilter->appsink")?;
    src.link(&demux).context("link filesrc->qtdemux")?;

    let queue_weak = queue.downgrade();
    demux.connect_pad_added(move |_demux, src_pad| {
        let Some(queue) = queue_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = queue.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if !pad_is_video(src_pad) {
            debug!(pad = %src_pad.name(), "ignoring non-video pad from qtdemux");
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            warn!(
                pad = %src_pad.name(),
                error = ?err,
                "failed to link qtdemux video pad to queue"
            );
        }
    });

    Ok((pipeline, appsink))
}

#[cfg(not(target_os = "macos"))]
fn build_h26x_pipeline_generic(
    path: &Path,
    codec_info: &VideoCodecInfo,
    parser_name: &str,
    kind: VideoCodecKind,
) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    let selection = select_decoder_for_codec(kind)?;
    info!(
        decoder = selection.factory_name,
        hardware = selection.is_hardware,
        desc = selection.description,
        codec = codec_info
            .caps_name
            .as_deref()
            .unwrap_or_else(|| match kind {
                VideoCodecKind::H265 => "video/x-h265",
                VideoCodecKind::H264 => "video/x-h264",
                VideoCodecKind::ProRes => "video/x-prores",
                VideoCodecKind::Other => "video/x-raw",
            }),
        "GStreamer backend selecting decoder"
    );

    let pipeline = gst::Pipeline::with_name("gst-native-decoder");

    let location = path.to_string_lossy().to_string();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", &location)
        .build()
        .with_context(|| format!("make filesrc for {}", path.display()))?;

    let demux = gst::ElementFactory::make("qtdemux")
        .build()
        .context("make qtdemux")?;

    let queue = gst::ElementFactory::make("queue")
        .build()
        .context("make queue")?;

    let parser = gst::ElementFactory::make(parser_name)
        .build()
        .with_context(|| format!("make {}", parser_name))?;
    let _ = parser.set_property("config-interval", &-1i32);

    let decoder = gst::ElementFactory::make(selection.factory_name)
        .build()
        .with_context(|| format!("make decoder {}", selection.factory_name))?;

    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("make videoconvert")?;

    let caps = gst::Caps::builder("video/x-raw")
        .field("format", &"NV12")
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()
        .context("make capsfilter")?;

    let caps_for_sink = caps.clone();
    info!(caps = %caps_for_sink, "decoder target caps for appsink");
    let appsink = gst_app::AppSink::builder()
        .caps(&caps_for_sink)
        .max_buffers(8)
        .drop(true)
        .build();

    pipeline
        .add_many(&[
            &src,
            &demux,
            &queue,
            &parser,
            &decoder,
            &convert,
            &capsfilter,
            appsink.upcast_ref(),
        ])
        .context("add pipeline elements")?;

    gst::Element::link_many(&[
        &queue,
        &parser,
        &decoder,
        &convert,
        &capsfilter,
        appsink.upcast_ref(),
    ])
    .context("link queue->parser->decoder->convert->capsfilter->appsink")?;
    src.link(&demux).context("link filesrc->qtdemux")?;

    let queue_weak = queue.downgrade();
    demux.connect_pad_added(move |_demux, src_pad| {
        let Some(queue) = queue_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = queue.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if !pad_is_video(src_pad) {
            debug!(pad = %src_pad.name(), "ignoring non-video pad from qtdemux");
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            warn!(
                pad = %src_pad.name(),
                error = ?err,
                "failed to link qtdemux video pad to queue"
            );
        }
    });

    Ok((pipeline, appsink))
}

fn build_generic_pipeline(
    path: &Path,
    codec_info: &VideoCodecInfo,
) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    info!(
        codec = codec_info.caps_name.as_deref().unwrap_or("unknown"),
        "GStreamer backend falling back to generic decodebin pipeline"
    );

    let pipeline = gst::Pipeline::with_name("gst-native-decoder");
    let location = path.to_string_lossy().to_string();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", &location)
        .build()
        .with_context(|| format!("make filesrc for {}", path.display()))?;

    let decode = make_decodebin_element()?;

    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("make videoconvert")?;

    let caps = gst::Caps::builder("video/x-raw")
        .field("format", &"NV12")
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()
        .context("make capsfilter")?;

    let appsink = gst_app::AppSink::builder()
        .caps(&caps)
        .max_buffers(8)
        .drop(true)
        .build();

    pipeline
        .add_many(&[&src, &decode, &convert, &capsfilter, appsink.upcast_ref()])
        .context("add generic pipeline elements")?;

    gst::Element::link_many(&[&convert, &capsfilter, appsink.upcast_ref()])
        .context("link convert->capsfilter->appsink")?;
    src.link(&decode).context("link filesrc->decodebin")?;

    let convert_weak = convert.downgrade();
    decode.connect_pad_added(move |_dbin, src_pad| {
        let Some(convert) = convert_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = convert.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if !pad_is_video(src_pad) {
            debug!(pad = %src_pad.name(), "ignoring non-video pad from decodebin");
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            warn!(
                pad = %src_pad.name(),
                error = ?err,
                "failed to link decodebin video pad to videoconvert"
            );
        }
    });

    Ok((pipeline, appsink))
}

/// Construct a playback pipeline that explicitly wires the best hardware decoder.
pub fn build_platform_accelerated_pipeline(uri: &str) -> Result<gst::Pipeline> {
    let location = if let Some(stripped) = uri.strip_prefix("file://") {
        stripped.to_string()
    } else {
        uri.to_string()
    };
    let path_check = Path::new(&location);
    if !path_check.exists() {
        return Err(anyhow!(
            "input video path does not exist: {}",
            path_check.display()
        ));
    }

    let codec_info = detect_video_codec(path_check);
    match codec_info.kind {
        VideoCodecKind::H264 => {
            let selection = select_decoder_for_codec(VideoCodecKind::H264)?;
            build_h264_playback_pipeline(&location, selection, &codec_info)
        }
        VideoCodecKind::H265 => {
            let selection = select_decoder_for_codec(VideoCodecKind::H265)?;
            build_h265_playback_pipeline(&location, selection, &codec_info)
        }
        VideoCodecKind::ProRes | VideoCodecKind::Other => {
            build_generic_playback_pipeline(&location, &codec_info)
        }
    }
}

fn build_h264_playback_pipeline(
    location: &str,
    selection: DecoderSelection,
    codec_info: &VideoCodecInfo,
) -> Result<gst::Pipeline> {
    #[cfg(target_os = "macos")]
    {
        return build_macos_h26x_playback_pipeline(
            location,
            selection,
            codec_info,
            "h264parse",
            VideoCodecKind::H264,
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        build_h26x_playback_pipeline_generic(location, selection, codec_info, "h264parse")
    }
}

fn build_h265_playback_pipeline(
    location: &str,
    selection: DecoderSelection,
    codec_info: &VideoCodecInfo,
) -> Result<gst::Pipeline> {
    #[cfg(target_os = "macos")]
    {
        return build_macos_h26x_playback_pipeline(
            location,
            selection,
            codec_info,
            "h265parse",
            VideoCodecKind::H265,
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        build_h26x_playback_pipeline_generic(location, selection, codec_info, "h265parse")
    }
}

#[cfg(target_os = "macos")]
fn build_macos_h26x_playback_pipeline(
    location: &str,
    selection: DecoderSelection,
    codec_info: &VideoCodecInfo,
    parser_name: &str,
    kind: VideoCodecKind,
) -> Result<gst::Pipeline> {
    info!(
        decoder = selection.factory_name,
        hardware = selection.is_hardware,
        desc = selection.description,
        codec = codec_info
            .caps_name
            .as_deref()
            .unwrap_or_else(|| match kind {
                VideoCodecKind::H265 => "video/x-h265",
                VideoCodecKind::H264 => "video/x-h264",
                VideoCodecKind::ProRes => "video/x-prores",
                VideoCodecKind::Other => "video/x-raw",
            }),
        "building macOS hardware playback pipeline"
    );

    let pipeline = gst::Pipeline::with_name("platform-accelerated-playback");
    let src = gst::ElementFactory::make("filesrc")
        .property("location", &location)
        .build()
        .with_context(|| format!("make filesrc for {}", location))?;

    let demux = gst::ElementFactory::make("qtdemux")
        .build()
        .context("make qtdemux")?;

    let queue = gst::ElementFactory::make("queue")
        .build()
        .context("make queue")?;

    let parser = gst::ElementFactory::make(parser_name)
        .build()
        .with_context(|| format!("make {}", parser_name))?;
    let _ = parser.set_property("config-interval", &-1i32);

    let decoder = gst::ElementFactory::make(selection.factory_name)
        .build()
        .with_context(|| format!("make decoder {}", selection.factory_name))?;

    let mut convert = None;
    if !selection.is_hardware {
        convert = Some(
            gst::ElementFactory::make("videoconvert")
                .build()
                .context("make videoconvert fallback")?,
        );
    }

    let sink = gst::ElementFactory::make("autovideosink")
        .build()
        .context("make autovideosink")?;

    pipeline.add_many(&[&src, &demux, &queue, &parser, &decoder])?;
    if let Some(ref convert_elem) = convert {
        pipeline.add(convert_elem)?;
    }
    pipeline.add(&sink)?;

    let mut chain: Vec<&gst::Element> = vec![&queue, &parser, &decoder];
    if let Some(ref convert_elem) = convert {
        chain.push(convert_elem);
    }
    chain.push(&sink);

    gst::Element::link_many(&chain).context("link playback chain")?;
    src.link(&demux).context("link filesrc->qtdemux")?;

    let queue_weak = queue.downgrade();
    demux.connect_pad_added(move |_demux, src_pad| {
        let Some(queue) = queue_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = queue.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if !pad_is_video(src_pad) {
            debug!(pad = %src_pad.name(), "ignoring non-video pad from qtdemux");
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            warn!(
                pad = %src_pad.name(),
                error = ?err,
                "failed to link qtdemux video pad to queue"
            );
        }
    });

    Ok(pipeline)
}

#[cfg(not(target_os = "macos"))]
fn build_h26x_playback_pipeline_generic(
    location: &str,
    selection: DecoderSelection,
    codec_info: &VideoCodecInfo,
    parser_name: &str,
) -> Result<gst::Pipeline> {
    info!(
        decoder = selection.factory_name,
        hardware = selection.is_hardware,
        desc = selection.description,
        codec = codec_info.caps_name.as_deref().unwrap_or("video/x-h264"),
        "building platform-accelerated playback pipeline"
    );

    let pipeline = gst::Pipeline::with_name("platform-accelerated-playback");
    let src = gst::ElementFactory::make("filesrc")
        .property("location", &location)
        .build()
        .with_context(|| format!("make filesrc for {}", location))?;

    let demux = gst::ElementFactory::make("qtdemux")
        .build()
        .context("make qtdemux")?;

    let queue = gst::ElementFactory::make("queue")
        .build()
        .context("make queue")?;

    let parser = gst::ElementFactory::make(parser_name)
        .build()
        .with_context(|| format!("make {}", parser_name))?;
    let _ = parser.set_property("config-interval", &-1i32);

    let decoder = gst::ElementFactory::make(selection.factory_name)
        .build()
        .with_context(|| format!("make decoder {}", selection.factory_name))?;

    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("make videoconvert")?;

    let sink = gst::ElementFactory::make("autovideosink")
        .build()
        .context("make autovideosink")?;

    pipeline
        .add_many(&[&src, &demux, &queue, &parser, &decoder, &convert, &sink])
        .context("add playback pipeline elements")?;

    gst::Element::link_many(&[&queue, &parser, &decoder, &convert, &sink])
        .context("link queue->parser->decoder->convert->sink")?;
    src.link(&demux).context("link filesrc->qtdemux")?;

    let queue_weak = queue.downgrade();
    demux.connect_pad_added(move |_demux, src_pad| {
        let Some(queue) = queue_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = queue.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if !pad_is_video(src_pad) {
            debug!(pad = %src_pad.name(), "ignoring non-video pad from qtdemux");
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            warn!(
                pad = %src_pad.name(),
                error = ?err,
                "failed to link qtdemux video pad to queue"
            );
        }
    });

    Ok(pipeline)
}

fn build_generic_playback_pipeline(
    location: &str,
    codec_info: &VideoCodecInfo,
) -> Result<gst::Pipeline> {
    info!(
        codec = codec_info.caps_name.as_deref().unwrap_or("unknown"),
        "building generic decodebin playback pipeline"
    );

    let pipeline = gst::Pipeline::with_name("platform-generic-playback");
    let src = gst::ElementFactory::make("filesrc")
        .property("location", &location)
        .build()
        .with_context(|| format!("make filesrc for {}", location))?;

    let decode = make_decodebin_element()?;

    let queue = gst::ElementFactory::make("queue")
        .build()
        .context("make queue")?;

    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("make videoconvert")?;

    let sink = gst::ElementFactory::make("autovideosink")
        .build()
        .context("make autovideosink")?;

    pipeline
        .add_many(&[&src, &decode, &queue, &convert, &sink])
        .context("add generic playback elements")?;

    gst::Element::link_many(&[&queue, &convert, &sink]).context("link queue->convert->sink")?;
    src.link(&decode).context("link filesrc->decodebin")?;

    let queue_weak = queue.downgrade();
    decode.connect_pad_added(move |_decode, src_pad| {
        let Some(queue) = queue_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = queue.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if !pad_is_video(src_pad) {
            debug!(pad = %src_pad.name(), "ignoring non-video pad from decodebin");
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            warn!(
                pad = %src_pad.name(),
                error = ?err,
                "failed to link decodebin video pad to queue"
            );
        }
    });

    Ok(pipeline)
}

pub struct GstDecoder {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    bus: gst::Bus,
    props: Mutex<VideoProperties>,
    config: DecoderConfig,
    started: AtomicBool,
    last_seek: Mutex<f64>,
    ring: FrameRing,
    last_out_pts: Mutex<f64>,
    strict_paused: bool,
    zero_copy_active: bool,
    #[cfg(target_os = "macos")]
    zc_ring: Option<IOSurfaceRing>,
    #[cfg(target_os = "macos")]
    missing_core_video_meta: OnceLock<()>,
}

impl GstDecoder {
    fn frame_rate(&self) -> f64 {
        let p = self.props.lock().unwrap();
        if p.frame_rate.is_finite() && p.frame_rate > 0.0 {
            p.frame_rate
        } else {
            30.0
        }
    }

    fn strict_wait_budget_ms(&self) -> u64 {
        // Wait ~8 frames for preroll; clamp to [500, 1500] ms
        let fps = self.frame_rate();
        let per_frame_ms = 1000.0 / fps.max(0.001);
        let ms = (per_frame_ms * 8.0).round();
        ms.max(500.0).min(1500.0) as u64
    }

    fn strict_pull_params(&self) -> (usize, u64) {
        // Attempts and slice tuned to fps; default (60, 12ms)
        let fps = self.frame_rate();
        let attempts = ((fps * 2.0).round() as usize).max(40).min(100);
        let slice = {
            let per_frame_ms = 1000.0 / fps.max(0.001);
            let s = (per_frame_ms / 2.0).round();
            s.max(8.0).min(16.0) as u64
        };
        (attempts, slice)
    }
    #[cfg(target_os = "macos")]
    fn advance_zero_copy(&mut self, timestamp: f64) -> Result<()> {
        if !self.strict_paused {
            self.ensure_started()?;
        } else {
            let elem: &gst::Element = self.sink.upcast_ref();
            let _ = elem.set_property("drop", &false);
            let _ = elem.set_property("max-buffers", &1u32);
            let _ = self
                .pipeline
                .set_state(gst::State::Paused)
                .map_err(|e| anyhow!("set PAUSED: {e}"))?;
        }

        let ring_empty = self.zc_ring.as_ref().map_or(true, |ring| ring.len() == 0);
        let last_out = *self.last_out_pts.lock().unwrap();
        let never_output = !last_out.is_finite();
        let big_jump = last_out.is_finite() && (timestamp - last_out).abs() > 0.25;
        let need_seek = ring_empty && (never_output || big_jump);

        if need_seek && !self.strict_paused {
            self.seek_to_internal(timestamp)?;
            if let Some(ref mut ring) = self.zc_ring {
                ring.clear();
            }
        }

        self.drain_bus();
        self.fill_ring_from_sink()?;

        if let Ok(mut last_out) = self.last_out_pts.lock() {
            *last_out = timestamp;
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    fn push_zero_copy_frame(
        &mut self,
        sample: &gst::Sample,
        buffer: &gst::BufferRef,
    ) -> Result<()> {
        let Some(meta_ptr) = find_core_video_meta_ptr(buffer) else {
            if self.missing_core_video_meta.set(()).is_ok() {
                warn!("CoreVideo metadata missing on vtdec_hw output; zero-copy frame skipped");
            }
            return Ok(());
        };

        let meta = unsafe { &*meta_ptr };
        let pixbuf = meta.pixbuf;
        if pixbuf.is_null() {
            return Ok(());
        }

        let surface_ref = unsafe { CVPixelBufferGetIOSurface(pixbuf) };
        if surface_ref.is_null() {
            warn!("CVPixelBufferGetIOSurface returned null; zero-copy frame skipped");
            return Ok(());
        }
        let iosurface = unsafe { IOSurface::wrap_under_get_rule(surface_ref as IOSurfaceRef) };

        let width = unsafe { CVPixelBufferGetWidth(pixbuf) } as u32;
        let height = unsafe { CVPixelBufferGetHeight(pixbuf) } as u32;
        let fallback_ts = *self.last_seek.lock().unwrap();
        let pts = buffer
            .pts()
            .map(|t| t.nseconds() as f64 / 1e9)
            .unwrap_or(fallback_ts);

        let frame = IOSurfaceFrame {
            surface: iosurface,
            format: YuvPixFmt::Nv12,
            width,
            height,
            timestamp: pts,
        };

        if let Some(ref mut ring) = self.zc_ring {
            ring.push(frame);
        }

        if let Some(caps) = sample.caps() {
            if let Ok(info) = gst_video::VideoInfo::from_caps(&caps) {
                if let Ok(mut props) = self.props.lock() {
                    if props.width == 0 || props.height == 0 {
                        props.width = info.width();
                        props.height = info.height();
                    }
                }
            }
        }

        Ok(())
    }
    pub fn new<P: AsRef<Path>>(path: P, config: DecoderConfig) -> Result<Self> {
        ensure_gst_init()?;
        let path = path.as_ref();
        let GstPipelineBundle {
            pipeline,
            sink,
            zero_copy,
        } = build_pipeline(path, &config)?;
        let bus = pipeline
            .bus()
            .ok_or_else(|| anyhow!("pipeline has no bus"))?;

        // Start paused so we can seek on first decode.
        pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| anyhow!("set PAUSED: {e}"))?;

        // Properties will be filled on first decoded frame.
        let props = VideoProperties {
            width: 0,
            height: 0,
            duration: f64::NAN,
            frame_rate: f64::NAN,
            format: YuvPixFmt::Nv12,
        };

        Ok(Self {
            pipeline,
            sink,
            bus,
            props: Mutex::new(props),
            config,
            started: AtomicBool::new(false),
            last_seek: Mutex::new(-1.0),
            ring: FrameRing::new(12),
            last_out_pts: Mutex::new(f64::NAN),
            strict_paused: false,
            zero_copy_active: zero_copy,
            #[cfg(target_os = "macos")]
            zc_ring: if zero_copy {
                Some(IOSurfaceRing::new(8))
            } else {
                None
            },
            #[cfg(target_os = "macos")]
            missing_core_video_meta: OnceLock::new(),
        })
    }

    fn ensure_started(&self) -> Result<()> {
        if !self.started.load(Ordering::SeqCst) {
            // Configure sink for streaming
            let elem: &gst::Element = self.sink.upcast_ref();
            let _ = elem.set_property("drop", &true);
            let _ = elem.set_property("max-buffers", &8u32);
            self.pipeline
                .set_state(gst::State::Playing)
                .map_err(|e| anyhow!("set PLAYING: {e}"))?;
            self.started.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn seek_to_internal(&self, timestamp: f64) -> Result<()> {
        let t = if timestamp.is_finite() && timestamp >= 0.0 {
            gst::ClockTime::from_nseconds((timestamp * 1_000_000_000.0) as u64)
        } else {
            gst::ClockTime::ZERO
        };
        self.pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, t)
            .map_err(|_| anyhow!("pipeline seek_simple failed"))?;
        if let Ok(mut last) = self.last_seek.lock() {
            *last = timestamp;
        }
        // In strict paused mode, wait for ASYNC_DONE to ensure preroll readiness.
        if self.strict_paused {
            // Give the pipeline time (adaptive) to complete accurate seek + preroll
            let wait_ms = self.strict_wait_budget_ms();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
            while std::time::Instant::now() < deadline {
                if let Some(msg) = self.bus.timed_pop_filtered(
                    gst::ClockTime::from_mseconds(33),
                    &[
                        gst::MessageType::AsyncDone,
                        gst::MessageType::Error,
                        gst::MessageType::Eos,
                    ],
                ) {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::AsyncDone(_) => break,
                        MessageView::Error(e) => {
                            debug!(
                                "GStreamer seek error from {}: {} ({:?})",
                                e.src().map(|s| s.path_string()).unwrap_or_default(),
                                e.error(),
                                e.debug()
                            );
                            break;
                        }
                        MessageView::Eos(_) => break,
                        _ => {}
                    }
                } else {
                    // No message this slice; continue waiting
                }
            }
        }
        // Drop any old frames in ring by posting a FLUSH and letting caller clear ring
        Ok(())
    }

    fn drain_bus(&self) {
        while let Some(msg) = self.bus.pop() {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => {
                    debug!("GStreamer: EOS");
                }
                MessageView::Error(e) => {
                    warn!(
                        "GStreamer error from {}: {} ({:?})",
                        e.src().map(|s| s.path_string()).unwrap_or_default(),
                        e.error(),
                        e.debug()
                    );
                }
                _ => {}
            }
        }
    }

    fn fill_ring_from_sink(&mut self) -> Result<()> {
        // Pull frames from appsink. In strict paused mode the pipeline is PAUSED and
        // produces a preroll buffer, which must be retrieved via try_pull_preroll.
        // In streaming mode (PLAYING), use try_pull_sample.
        let (attempts, slice_ms) = if self.strict_paused {
            self.strict_pull_params()
        } else {
            (6usize, 5u64)
        };
        for _ in 0..attempts {
            let sample = if self.strict_paused {
                self.sink
                    .try_pull_preroll(Some(gst::ClockTime::from_mseconds(slice_ms)))
            } else {
                self.sink
                    .try_pull_sample(Some(gst::ClockTime::from_mseconds(slice_ms)))
            };
            if let Some(sample) = sample {
                let buffer = sample
                    .buffer()
                    .ok_or_else(|| anyhow!("appsink sample without buffer"))?;

                // Determine width/height from caps if possible.
                if let Some(caps) = sample.caps() {
                    if let Ok(info) = gst_video::VideoInfo::from_caps(&caps) {
                        if let Ok(mut p) = self.props.lock() {
                            // Only update when unknown.
                            if p.width == 0 || p.height == 0 {
                                p.width = info.width();
                                p.height = info.height();
                            }
                            // Try to extract framerate from caps structure.
                            if let Some(s) = caps.structure(0) {
                                if let Ok(fps) = s.get::<gst::Fraction>("framerate") {
                                    if fps.denom() != 0 {
                                        p.frame_rate = fps.numer() as f64 / fps.denom() as f64;
                                    }
                                }
                            }
                            // Try to obtain duration from the pipeline if unknown.
                            if p.duration.is_nan() {
                                if let Some(d) = self.pipeline.query_duration::<gst::ClockTime>() {
                                    p.duration = d.nseconds() as f64 / 1e9;
                                }
                            }
                            p.format = YuvPixFmt::Nv12;
                        }
                    }
                }

                #[cfg(target_os = "macos")]
                if self.zero_copy_active {
                    if let Some(_) = self.zc_ring {
                        self.push_zero_copy_frame(&sample, buffer)?;
                    } else {
                        warn!(
                            "zero-copy pipeline active but IOSurface ring not initialised; dropping frame"
                        );
                    }
                    if self.strict_paused {
                        break;
                    }
                    continue;
                }
                #[cfg(not(target_os = "macos"))]
                if self.zero_copy_active {
                    // Zero-copy is only supported on macOS; this branch should never run.
                    continue;
                }

                let (w, h) = {
                    let p = self.props.lock().unwrap();
                    (p.width.max(1), p.height.max(1))
                };

                // Prefer mapping via gst_video to handle stride/padding.
                if let Some(caps) = sample.caps() {
                    if let Ok(info) = gst_video::VideoInfo::from_caps(&caps) {
                        if let Ok(vf) =
                            gst_video::VideoFrameRef::from_buffer_ref_readable(&buffer, &info)
                        {
                            // Plane 0: Y, Plane 1: interleaved UV
                            let (w0, h0) = (info.width() as usize, info.height() as usize);
                            let y_sz = w0 * h0;
                            let mut y = vec![0u8; y_sz];
                            // Copy row-by-row to drop padding
                            let strides = vf.plane_stride();
                            if strides.len() >= 1 {
                                let stride0 = strides[0].max(0) as usize;
                                let src0 = vf.plane_data(0).unwrap();
                                for row in 0..h0 {
                                    let src_off = row * stride0;
                                    let dst_off = row * w0;
                                    y[dst_off..dst_off + w0]
                                        .copy_from_slice(&src0[src_off..src_off + w0]);
                                }
                            }

                            let (w1, h1) = (info.width() as usize / 2, info.height() as usize / 2);
                            let uv_sz = w1 * h1 * 2;
                            let mut uv = vec![0u8; uv_sz];
                            if strides.len() >= 2 {
                                let stride1 = strides[1].max(0) as usize;
                                let src1 = vf.plane_data(1).unwrap();
                                for row in 0..h1 {
                                    let src_off = row * stride1;
                                    let dst_off = row * (w1 * 2);
                                    uv[dst_off..dst_off + (w1 * 2)]
                                        .copy_from_slice(&src1[src_off..src_off + (w1 * 2)]);
                                }
                            }

                            let fallback_ts = *self.last_seek.lock().unwrap();
                            let pts = buffer
                                .pts()
                                .map(|t| t.nseconds() as f64 / 1e9)
                                .unwrap_or(fallback_ts);

                            self.ring.push(VideoFrame {
                                format: YuvPixFmt::Nv12,
                                y_plane: y,
                                uv_plane: uv,
                                width: info.width(),
                                height: info.height(),
                                timestamp: pts,
                            });
                            if self.strict_paused {
                                break;
                            }
                            continue; // next sample
                        }
                    }
                }

                // Fallback: tightly packed NV12 (Y w*h, UV w*h/2)
                if let Ok(map) = buffer.map_readable() {
                    let data = map.as_slice();
                    let y_sz = (w as usize) * (h as usize);
                    let uv_sz = y_sz / 2;
                    if data.len() >= y_sz + uv_sz {
                        let mut y = vec![0u8; y_sz];
                        let mut uv = vec![0u8; uv_sz];
                        y.copy_from_slice(&data[..y_sz]);
                        uv.copy_from_slice(&data[y_sz..y_sz + uv_sz]);
                        let fallback_ts = *self.last_seek.lock().unwrap();
                        let pts = buffer
                            .pts()
                            .map(|t| t.nseconds() as f64 / 1e9)
                            .unwrap_or(fallback_ts);
                        self.ring.push(VideoFrame {
                            format: YuvPixFmt::Nv12,
                            y_plane: y,
                            uv_plane: uv,
                            width: w,
                            height: h,
                            timestamp: pts,
                        });
                        if self.strict_paused {
                            break;
                        }
                        continue;
                    }
                }
                warn!("GStreamer: unsupported buffer layout; skipping frame");
            }
        }
        Ok(())
    }
}

impl Drop for GstDecoder {
    fn drop(&mut self) {
        let _ = self
            .pipeline
            .set_state(gst::State::Null)
            .map_err(|e| debug!("GStreamer set NULL failed: {e:?}"));
    }
}

impl NativeVideoDecoder for GstDecoder {
    fn decode_frame(&mut self, timestamp: f64) -> Result<Option<VideoFrame>> {
        if self.zero_copy_active {
            #[cfg(target_os = "macos")]
            {
                self.advance_zero_copy(timestamp)?;
            }
            #[cfg(not(target_os = "macos"))]
            {
                // Zero-copy should never be active on non-macOS platforms.
            }
            return Ok(None);
        }

        if !self.strict_paused {
            self.ensure_started()?;
        } else {
            // In strict paused mode, keep pipeline paused and configure sink to hold preroll
            let elem: &gst::Element = self.sink.upcast_ref();
            let _ = elem.set_property("drop", &false);
            let _ = elem.set_property("max-buffers", &1u32);
            let _ = self
                .pipeline
                .set_state(gst::State::Paused)
                .map_err(|e| anyhow!("set PAUSED: {e}"));
        }
        // Seek policy:
        // - Seek on first call, or when ring is empty AND jump is significant (> 0.25s).
        // - Avoid constantly re-seeking during normal playback.
        let need_seek = {
            let ring_empty = self.ring.len() == 0;
            let last_out = *self.last_out_pts.lock().unwrap();
            let never_output = !last_out.is_finite();
            let big_jump = last_out.is_finite() && (timestamp - last_out).abs() > 0.25;
            ring_empty && (never_output || big_jump)
        };
        if need_seek && !self.strict_paused {
            self.seek_to_internal(timestamp)?;
            if let Ok(mut last) = self.last_seek.lock() {
                *last = timestamp;
            }
            self.ring.clear();
            #[cfg(target_os = "macos")]
            if let Some(ref mut ring) = self.zc_ring {
                ring.clear();
            }
        }
        // Drain bus and pull a few samples into the ring
        self.drain_bus();
        self.fill_ring_from_sink()?;
        // Choose nearest at or before target
        let out = self.ring.pop_nearest_at_or_before(timestamp);
        if let Some(ref f) = out {
            if let Ok(mut last_out) = self.last_out_pts.lock() {
                *last_out = f.timestamp;
            }
        }
        Ok(out)
    }

    #[cfg(target_os = "macos")]
    fn decode_frame_zero_copy(&mut self, timestamp: f64) -> Result<Option<IOSurfaceFrame>> {
        if !self.zero_copy_active {
            return Ok(None);
        }
        self.advance_zero_copy(timestamp)?;
        if let Some(ref mut ring) = self.zc_ring {
            return Ok(ring.pop_nearest_at_or_before(timestamp));
        }
        Ok(None)
    }

    fn get_properties(&self) -> VideoProperties {
        self.props.lock().unwrap().clone()
    }

    fn seek_to(&mut self, timestamp: f64) -> Result<()> {
        // Enter strict paused mode for accurate preroll
        self.strict_paused = true;
        // Configure sink to hold preroll
        let elem: &gst::Element = self.sink.upcast_ref();
        let _ = elem.set_property("drop", &false);
        let _ = elem.set_property("max-buffers", &1u32);
        // Pause pipeline
        let _ = self
            .pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| anyhow!("set PAUSED: {e}"));
        self.ring.clear();
        #[cfg(target_os = "macos")]
        if let Some(ref mut ring) = self.zc_ring {
            ring.clear();
        }
        self.seek_to_internal(timestamp)
    }

    fn set_strict_paused(&mut self, strict: bool) {
        self.strict_paused = strict;
        let elem: &gst::Element = self.sink.upcast_ref();
        if strict {
            let _ = elem.set_property("drop", &false);
            let _ = elem.set_property("max-buffers", &1u32);
            let _ = self
                .pipeline
                .set_state(gst::State::Paused)
                .map_err(|e| debug!("set PAUSED failed: {e:?}"));
        } else {
            let _ = elem.set_property("drop", &true);
            let _ = elem.set_property("max-buffers", &8u32);
            let _ = self
                .pipeline
                .set_state(gst::State::Playing)
                .map_err(|e| debug!("set PLAYING failed: {e:?}"));
            self.started.store(true, Ordering::SeqCst);
        }
    }

    fn seek_to_keyframe(&mut self, timestamp: f64) -> Result<()> {
        // Fast paused seek: KEY_UNIT to nearest keyframe
        self.strict_paused = true;
        let elem: &gst::Element = self.sink.upcast_ref();
        let _ = elem.set_property("drop", &false);
        let _ = elem.set_property("max-buffers", &1u32);
        let _ = self
            .pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| anyhow!("set PAUSED: {e}"));
        self.ring.clear();
        let t = if timestamp.is_finite() && timestamp >= 0.0 {
            gst::ClockTime::from_nseconds((timestamp * 1_000_000_000.0) as u64)
        } else {
            gst::ClockTime::ZERO
        };
        self.pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, t)
            .map_err(|_| anyhow!("pipeline key-unit seek failed"))?;
        if let Ok(mut last) = self.last_seek.lock() {
            *last = timestamp;
        }
        #[cfg(target_os = "macos")]
        if let Some(ref mut ring) = self.zc_ring {
            ring.clear();
        }
        // Wait for ASYNC_DONE like accurate seek to ensure preroll readiness (adaptive)
        if self.strict_paused {
            let wait_ms = self.strict_wait_budget_ms();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
            while std::time::Instant::now() < deadline {
                if let Some(msg) = self.bus.timed_pop_filtered(
                    gst::ClockTime::from_mseconds(33),
                    &[
                        gst::MessageType::AsyncDone,
                        gst::MessageType::Error,
                        gst::MessageType::Eos,
                    ],
                ) {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::AsyncDone(_) => break,
                        MessageView::Error(e) => {
                            debug!(
                                "GStreamer key-unit seek error from {}: {} ({:?})",
                                e.src().map(|s| s.path_string()).unwrap_or_default(),
                                e.error(),
                                e.debug()
                            );
                            break;
                        }
                        MessageView::Eos(_) => break,
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn supports_zero_copy(&self) -> bool {
        self.zero_copy_active
    }
}

/// Public constructor used by lib::create_decoder when the feature is enabled.
pub fn create_gst_decoder<P: AsRef<Path>>(
    path: P,
    config: DecoderConfig,
) -> Result<Box<dyn NativeVideoDecoder>> {
    info!(
        path = %path.as_ref().display(),
        hw = config.hardware_acceleration,
        zero_copy = config.zero_copy,
        "native decoder: GStreamer pipeline initializing"
    );
    let dec = GstDecoder::new(path, config)?;
    info!("native decoder: GStreamer pipeline ready");
    Ok(Box::new(dec))
}

/// Report availability by attempting to initialize GStreamer.
pub fn is_available() -> bool {
    ensure_gst_init().is_ok()
}

// -----------------------------------------------------------------------------
// Simple ring buffer to choose nearest frame at/before target
// -----------------------------------------------------------------------------

struct FrameRing {
    frames: Vec<VideoFrame>,
    cap: usize,
}

impl FrameRing {
    fn new(cap: usize) -> Self {
        Self {
            frames: Vec::with_capacity(cap),
            cap,
        }
    }
    fn clear(&mut self) {
        self.frames.clear();
    }
    fn len(&self) -> usize {
        self.frames.len()
    }
    fn push(&mut self, f: VideoFrame) {
        if self.frames.len() >= self.cap {
            self.frames.remove(0);
        }
        self.frames.push(f);
    }
    fn pop_nearest_at_or_before(&mut self, target: f64) -> Option<VideoFrame> {
        if self.frames.is_empty() {
            return None;
        }
        // Find candidate with pts <= target and maximum pts
        let mut best_idx: Option<usize> = None;
        let mut best_dt = f64::INFINITY;
        for (i, f) in self.frames.iter().enumerate() {
            let pts = f.timestamp;
            if pts.is_finite() && pts <= target {
                let dt = target - pts;
                if dt < best_dt {
                    best_dt = dt;
                    best_idx = Some(i);
                }
            }
        }
        if let Some(i) = best_idx {
            return Some(self.frames.remove(i));
        }
        // else: return the oldest
        Some(self.frames.remove(0))
    }
}

#[cfg(target_os = "macos")]
struct IOSurfaceRing {
    frames: Vec<IOSurfaceFrame>,
    cap: usize,
}

#[cfg(target_os = "macos")]
impl IOSurfaceRing {
    fn new(cap: usize) -> Self {
        Self {
            frames: Vec::with_capacity(cap),
            cap,
        }
    }
    fn clear(&mut self) {
        self.frames.clear();
    }
    fn len(&self) -> usize {
        self.frames.len()
    }
    fn push(&mut self, frame: IOSurfaceFrame) {
        if self.frames.len() >= self.cap {
            self.frames.remove(0);
        }
        self.frames.push(frame);
    }
    fn pop_nearest_at_or_before(&mut self, target: f64) -> Option<IOSurfaceFrame> {
        if self.frames.is_empty() {
            return None;
        }
        let mut best_idx: Option<usize> = None;
        let mut best_dt = f64::INFINITY;
        for (i, frame) in self.frames.iter().enumerate() {
            let pts = frame.timestamp;
            if pts.is_finite() && pts <= target {
                let dt = target - pts;
                if dt < best_dt {
                    best_dt = dt;
                    best_idx = Some(i);
                }
            }
        }
        if let Some(i) = best_idx {
            return Some(self.frames.remove(i));
        }
        Some(self.frames.remove(0))
    }
}
