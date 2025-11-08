use std::path::Path;

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_pbutils as gstpb;
use gstreamer_pbutils::prelude::*;
use std::panic::{catch_unwind, AssertUnwindSafe};
use tracing::{debug, warn};
use url::Url;

use super::job::{CacheJobSpec, PreferredCodec};

const AAC_BITRATE: i32 = 192_000;
const PRORES_TOKENS: &[&str] = &[
    "prores", "apcn", "apcs", "apco", "apch", "ap4h", "ap4x", "ap4a", "ap4o", "ap4n", "ap4b",
    "ap4f",
];
const PRORES_PROFILE_HQ: &str = "hq";

pub fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

pub fn build_prores_pipeline(spec: &CacheJobSpec, output_tmp: &Path) -> Result<gst::Pipeline> {
    match spec.preferred_codec {
        PreferredCodec::ProRes422 => {
            if is_macos() {
                if source_is_prores(spec) {
                    debug!(
                        target = "cache::pipeline",
                        source = %spec.source_path.display(),
                        codec = ?spec.source_codec,
                        "detected ProRes source; using cross-platform fallback pipeline"
                    );
                    build_cross_platform_prores(spec, output_tmp)
                } else {
                    build_macos_prores(spec, output_tmp)
                }
            } else {
                build_cross_platform_prores(spec, output_tmp)
            }
        }
    }
}

fn source_is_prores(spec: &CacheJobSpec) -> bool {
    if metadata_indicates_prores(spec) {
        debug!(
            target = "cache::pipeline",
            source = %spec.source_path.display(),
            codec = ?spec.source_codec,
            "detected ProRes source via metadata"
        );
        return true;
    }

    if detect_prores_via_caps(&spec.source_path) {
        debug!(
            target = "cache::pipeline",
            source = %spec.source_path.display(),
            "detected ProRes source via stream caps"
        );
        return true;
    }

    false
}

fn metadata_indicates_prores(spec: &CacheJobSpec) -> bool {
    spec.source_codec
        .as_deref()
        .map(contains_prores_token)
        .unwrap_or(false)
}

fn contains_prores_token(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    PRORES_TOKENS.iter().any(|token| lower.contains(token))
}

fn structure_indicates_prores(structure: &gst::StructureRef) -> bool {
    if structure.name().eq_ignore_ascii_case("video/x-prores") {
        return true;
    }
    contains_prores_token(&structure.to_string())
}

fn detect_prores_via_caps(path: &Path) -> bool {
    if gst::init().is_err() {
        return false;
    }

    let discoverer = match gstpb::Discoverer::new(gst::ClockTime::from_seconds(3)) {
        Ok(d) => d,
        Err(err) => {
            debug!(
                target = "cache::pipeline",
                error = %err,
                "failed to construct Discoverer for ProRes detection"
            );
            return false;
        }
    };

    let uri = match Url::from_file_path(path) {
        Ok(url) => url,
        Err(_) => {
            debug!(
                target = "cache::pipeline",
                source = %path.display(),
                "failed to convert path to URI for ProRes detection"
            );
            return false;
        }
    };

    let info = match discoverer.discover_uri(uri.as_str()) {
        Ok(info) => info,
        Err(err) => {
            debug!(
                target = "cache::pipeline",
                error = %err,
                "Discoverer failed to analyse source for ProRes detection"
            );
            return false;
        }
    };

    for stream in info.video_streams() {
        if let Some(caps) = stream.caps() {
            for structure in caps.iter() {
                if structure_indicates_prores(&structure) {
                    return true;
                }
            }
        }
    }

    false
}

fn build_macos_prores(spec: &CacheJobSpec, output_tmp: &Path) -> Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::new();

    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", &spec.source_path.to_string_lossy().to_string())
        .build()
        .context("make filesrc")?;
    let demux = gst::ElementFactory::make("qtdemux")
        .build()
        .context("make qtdemux")?;
    let video_queue = make_queue("video_queue")?;
    let decoder_hw = match gst::ElementFactory::make("vtdec_hw").build() {
        Ok(elem) => elem,
        Err(err) => {
            warn!(
                target = "cache::pipeline",
                error = %err,
                "vtdec_hw unavailable; using cross-platform pipeline"
            );
            return build_cross_platform_prores(spec, output_tmp);
        }
    };
    let decoder_sw = match gst::ElementFactory::make("avdec_prores_ks").build() {
        Ok(elem) => elem,
        Err(err) => {
            warn!(
                target = "cache::pipeline",
                error = %err,
                "avdec_prores_ks unavailable; using cross-platform pipeline"
            );
            return build_cross_platform_prores(spec, output_tmp);
        }
    };
    let video_convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("make videoconvert")?;
    let encoder = make_macos_prores_encoder()?;
    let video_caps = gst::Caps::builder("video/x-raw")
        .field("format", &prores_raw_format_for_encoder(&encoder))
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &video_caps)
        .build()
        .context("make capsfilter")?;
    let video_mux_queue = make_queue("video_mux_queue")?;
    let audio_queue = make_queue("audio_queue")?;
    let audio_convert = gst::ElementFactory::make("audioconvert")
        .build()
        .context("make audioconvert")?;
    let audio_resample = gst::ElementFactory::make("audioresample")
        .build()
        .context("make audioresample")?;
    let audio_encoder = gst::ElementFactory::make("avenc_aac")
        .property("bitrate", &AAC_BITRATE)
        .build()
        .context("make avenc_aac")?;
    let audio_mux_queue = make_queue("audio_mux_queue")?;
    let mux = gst::ElementFactory::make("qtmux")
        .property("faststart", &true)
        .build()
        .context("make qtmux")?;
    if !spec.force_container_mov {
        debug!("force_container_mov disabled; using mov container for deterministic outputs");
    }
    let filesink = gst::ElementFactory::make("filesink")
        .property("location", &output_tmp.to_string_lossy().to_string())
        .property("sync", &false)
        .build()
        .context("make filesink")?;

    pipeline
        .add_many(&[
            &filesrc,
            &demux,
            &video_queue,
            &decoder_hw,
            &decoder_sw,
            &video_convert,
            &capsfilter,
            &encoder,
            &video_mux_queue,
            &audio_queue,
            &audio_convert,
            &audio_resample,
            &audio_encoder,
            &audio_mux_queue,
            &mux,
            &filesink,
        ])
        .context("add macOS pipeline elements")?;

    filesrc.link(&demux).context("link filesrc->qtdemux")?;
    gst::Element::link_many(&[&video_convert, &capsfilter, &encoder, &video_mux_queue])
        .context("link macOS video tail chain")?;
    gst::Element::link_many(&[
        &audio_queue,
        &audio_convert,
        &audio_resample,
        &audio_encoder,
        &audio_mux_queue,
    ])
    .context("link macOS audio branch")?;
    mux.link(&filesink).context("link mux->filesink")?;

    let mux_weak = mux.downgrade();
    let video_queue_weak = video_queue.downgrade();
    let video_mux_queue_weak = video_mux_queue.downgrade();
    let decoder_hw_weak = decoder_hw.downgrade();
    let decoder_sw_weak = decoder_sw.downgrade();
    let video_convert_weak = video_convert.downgrade();
    let audio_queue_weak = audio_queue.downgrade();
    let audio_mux_queue_weak = audio_mux_queue.downgrade();
    let source_path = spec.source_path.clone();

    demux.connect_pad_added(move |_demux, src_pad| {
        let Some(caps) = src_pad.current_caps() else {
            return;
        };
        let Some(structure) = caps.structure(0) else {
            return;
        };
        let caps_name = structure.name();
        if caps_name.starts_with("video/") {
            let use_sw = structure_indicates_prores(structure);
            if let (Some(queue), Some(mux_queue), Some(mux), Some(video_convert)) = (
                video_queue_weak.upgrade(),
                video_mux_queue_weak.upgrade(),
                mux_weak.upgrade(),
                video_convert_weak.upgrade(),
            ) {
                let decoder = if use_sw {
                    if let Some(hw) = decoder_hw_weak.upgrade() {
                        let _ = hw.set_state(gst::State::Null);
                    }
                    if let Some(sw) = decoder_sw_weak.upgrade() {
                        debug!(
                            target = "cache::pipeline",
                            source = %source_path.display(),
                            "using software ProRes decoder fallback"
                        );
                        sw
                    } else {
                        warn!(
                            target = "cache::pipeline",
                            "software ProRes decoder unavailable; branch cannot be linked"
                        );
                        return;
                    }
                } else {
                    if let Some(sw) = decoder_sw_weak.upgrade() {
                        let _ = sw.set_state(gst::State::Null);
                    }
                    match decoder_hw_weak.upgrade() {
                        Some(hw) => hw,
                        None => return,
                    }
                };

                if let Err(err) =
                    link_video_branch(src_pad, &queue, &decoder, &video_convert, &mux_queue, &mux)
                {
                    warn!(
                        target = "cache::pipeline",
                        error = %err,
                        "failed to link macOS video branch"
                    );
                }
            }
        } else if caps_name.starts_with("audio/") {
            if let (Some(queue), Some(mux_queue), Some(mux)) = (
                audio_queue_weak.upgrade(),
                audio_mux_queue_weak.upgrade(),
                mux_weak.upgrade(),
            ) {
                link_branch_pad(src_pad, &queue, &mux_queue, &mux, "audio_%u");
            }
        } else {
            debug!(caps = %caps_name, "ignoring unexpected qtdemux pad");
        }
    });

    Ok(pipeline)
}

fn build_cross_platform_prores(spec: &CacheJobSpec, output_tmp: &Path) -> Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::new();

    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", &spec.source_path.to_string_lossy().to_string())
        .build()
        .context("make filesrc")?;
    let decodebin = gst::ElementFactory::make("decodebin3")
        .build()
        .context("make decodebin3")?;
    let video_queue = make_queue("video_queue")?;
    let video_convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("make videoconvert")?;
    let encoder = gst::ElementFactory::make("avenc_prores_ks")
        .build()
        .context("make avenc_prores_ks")?;
    configure_avenc_prores_profile(&encoder);
    let video_caps = gst::Caps::builder("video/x-raw")
        .field("format", &prores_raw_format_for_encoder(&encoder))
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &video_caps)
        .build()
        .context("make capsfilter")?;
    let video_mux_queue = make_queue("video_mux_queue")?;
    let audio_queue = make_queue("audio_queue")?;
    let audio_convert = gst::ElementFactory::make("audioconvert")
        .build()
        .context("make audioconvert")?;
    let audio_resample = gst::ElementFactory::make("audioresample")
        .build()
        .context("make audioresample")?;
    let audio_encoder = gst::ElementFactory::make("avenc_aac")
        .property("bitrate", &AAC_BITRATE)
        .build()
        .context("make avenc_aac")?;
    let audio_mux_queue = make_queue("audio_mux_queue")?;
    let mux = gst::ElementFactory::make("qtmux")
        .property("faststart", &true)
        .build()
        .context("make qtmux")?;
    let filesink = gst::ElementFactory::make("filesink")
        .property("location", &output_tmp.to_string_lossy().to_string())
        .property("sync", &false)
        .build()
        .context("make filesink")?;

    pipeline
        .add_many(&[
            &filesrc,
            &decodebin,
            &video_queue,
            &video_convert,
            &capsfilter,
            &encoder,
            &video_mux_queue,
            &audio_queue,
            &audio_convert,
            &audio_resample,
            &audio_encoder,
            &audio_mux_queue,
            &mux,
            &filesink,
        ])
        .context("add cross-platform pipeline elements")?;

    filesrc
        .link(&decodebin)
        .context("link filesrc->decodebin3")?;
    gst::Element::link_many(&[
        &video_queue,
        &video_convert,
        &capsfilter,
        &encoder,
        &video_mux_queue,
    ])
    .context("link cross-platform video branch")?;
    gst::Element::link_many(&[
        &audio_queue,
        &audio_convert,
        &audio_resample,
        &audio_encoder,
        &audio_mux_queue,
    ])
    .context("link cross-platform audio branch")?;
    mux.link(&filesink).context("link mux->filesink")?;

    let mux_weak = mux.downgrade();
    let video_queue_weak = video_queue.downgrade();
    let video_mux_queue_weak = video_mux_queue.downgrade();
    let audio_queue_weak = audio_queue.downgrade();
    let audio_mux_queue_weak = audio_mux_queue.downgrade();

    decodebin.connect_pad_added(move |_dbin, src_pad| {
        let Some(structure) = src_pad
            .current_caps()
            .and_then(|caps| caps.structure(0).map(|s| s.to_owned()))
        else {
            return;
        };
        let caps_name = structure.name();
        if caps_name.starts_with("video/") {
            if let (Some(queue), Some(mux_queue), Some(mux)) = (
                video_queue_weak.upgrade(),
                video_mux_queue_weak.upgrade(),
                mux_weak.upgrade(),
            ) {
                link_branch_pad(src_pad, &queue, &mux_queue, &mux, "video_%u");
            }
        } else if caps_name.starts_with("audio/") {
            if let (Some(queue), Some(mux_queue), Some(mux)) = (
                audio_queue_weak.upgrade(),
                audio_mux_queue_weak.upgrade(),
                mux_weak.upgrade(),
            ) {
                link_branch_pad(src_pad, &queue, &mux_queue, &mux, "audio_%u");
            }
        } else {
            debug!(caps = %caps_name, "ignoring unexpected decodebin pad");
        }
    });

    Ok(pipeline)
}

fn make_queue(name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make("queue")
        .build()
        .with_context(|| format!("make queue '{name}'"))
}

fn make_macos_prores_encoder() -> Result<gst::Element> {
    match gst::ElementFactory::make("vtenc_prores").build() {
        Ok(enc) => {
            let has_profile = enc.has_property("profile", None);
            let has_quality = enc.has_property("quality", None);
            let has_realtime = enc.has_property("realtime", None);

            let mut ok = true;

            if has_profile {
                if catch_unwind(AssertUnwindSafe(|| {
                    enc.set_property_from_str("profile", "apcn");
                }))
                .is_err()
                {
                    ok = false;
                    warn!("vtenc_prores profile property rejected value 'apcn'");
                }
            } else {
                warn!("vtenc_prores has no 'profile' property; using encoder defaults");
            }

            if has_quality {
                if catch_unwind(AssertUnwindSafe(|| {
                    enc.set_property("quality", &0.5f64);
                }))
                .is_err()
                {
                    ok = false;
                    warn!("vtenc_prores failed to set 'quality' property");
                }
            } else {
                warn!("vtenc_prores has no 'quality' property");
            }

            if has_realtime {
                if catch_unwind(AssertUnwindSafe(|| {
                    enc.set_property("realtime", &true);
                }))
                .is_err()
                {
                    ok = false;
                    warn!("vtenc_prores failed to set 'realtime' property");
                }
            } else {
                warn!("vtenc_prores has no 'realtime' property");
            }

            if ok {
                return Ok(enc);
            } else {
                warn!("vtenc_prores property setup failed; falling back to avenc_prores_ks");
            }
        }
        Err(err) => {
            warn!(error = %err, "vtenc_prores unavailable; falling back to avenc_prores_ks");
        }
    }

    let enc = gst::ElementFactory::make("avenc_prores_ks")
        .build()
        .context("make avenc_prores_ks fallback")?;
    configure_avenc_prores_profile(&enc);
    Ok(enc)
}

fn link_branch_pad(
    src_pad: &gst::Pad,
    branch_queue: &gst::Element,
    mux_queue: &gst::Element,
    mux: &gst::Element,
    pad_template: &str,
) {
    let Some(queue_sink) = branch_queue.static_pad("sink") else {
        warn!("branch queue missing sink pad");
        return;
    };
    if queue_sink.is_linked() {
        debug!("branch queue already linked; skipping duplicate pad");
        return;
    }
    if let Err(err) = src_pad.link(&queue_sink) {
        warn!(error = %err, "link branch queue failed");
        return;
    }
    if let Err(err) = ensure_mux_link(mux_queue, mux, pad_template) {
        warn!(error = %err, "link queue to mux failed");
    }
}

fn ensure_mux_link(queue: &gst::Element, mux: &gst::Element, template: &str) -> Result<()> {
    let queue_src = queue
        .static_pad("src")
        .ok_or_else(|| anyhow!("queue missing src pad"))?;
    if queue_src.is_linked() {
        return Ok(());
    }
    let mux_pad = mux
        .request_pad_simple(template)
        .ok_or_else(|| anyhow!("request mux pad '{template}' failed"))?;
    queue_src
        .link(&mux_pad)
        .map_err(|err| anyhow!("link queue to mux ({template}): {err}"))?;
    Ok(())
}

fn configure_avenc_prores_profile(enc: &gst::Element) {
    if !enc.has_property("profile", None) {
        warn!("avenc_prores_ks has no 'profile' property; using encoder defaults");
        return;
    }

    if catch_unwind(AssertUnwindSafe(|| {
        enc.set_property_from_str("profile", PRORES_PROFILE_HQ);
    }))
    .is_err()
    {
        warn!(
            "avenc_prores_ks rejected profile '{}'; using encoder defaults",
            PRORES_PROFILE_HQ
        );
    }
}

fn prores_raw_format_for_encoder(enc: &gst::Element) -> &'static str {
    if let Some(factory) = enc.factory() {
        if factory.name() == "avenc_prores_ks" {
            return "I422_10LE";
        }
    }

    "UYVY"
}

fn link_video_branch(
    src_pad: &gst::Pad,
    queue: &gst::Element,
    decoder: &gst::Element,
    video_convert: &gst::Element,
    video_mux_queue: &gst::Element,
    mux: &gst::Element,
) -> Result<()> {
    let queue_sink = queue
        .static_pad("sink")
        .ok_or_else(|| anyhow!("video queue missing sink pad"))?;
    if !queue_sink.is_linked() {
        src_pad
            .link(&queue_sink)
            .map_err(|e| anyhow!("link demux -> video queue failed: {e:?}"))?;
    }

    let queue_src = queue
        .static_pad("src")
        .ok_or_else(|| anyhow!("video queue missing src pad"))?;
    let decoder_sink = decoder
        .static_pad("sink")
        .ok_or_else(|| anyhow!("video decoder missing sink pad"))?;
    if !decoder_sink.is_linked() {
        queue_src
            .link(&decoder_sink)
            .map_err(|e| anyhow!("link video queue -> decoder failed: {e:?}"))?;
    }

    let decoder_src = decoder
        .static_pad("src")
        .ok_or_else(|| anyhow!("video decoder missing src pad"))?;
    let convert_sink = video_convert
        .static_pad("sink")
        .ok_or_else(|| anyhow!("videoconvert missing sink pad"))?;
    if !convert_sink.is_linked() {
        decoder_src
            .link(&convert_sink)
            .map_err(|e| anyhow!("link decoder -> videoconvert failed: {e:?}"))?;
    }

    decoder.sync_state_with_parent().ok();

    ensure_mux_link(video_mux_queue, mux, "video_%u")
}
