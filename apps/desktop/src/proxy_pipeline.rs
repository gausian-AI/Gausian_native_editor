use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, error, info, warn};

/// Proxy encoders available to the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyPreset {
    /// macOS-only ProRes Proxy 960p MOV output.
    MacProRes,
    /// Cross-platform DNxHR LB MXF output.
    DnxhrLb,
}

/// Messages emitted from the proxy pipeline worker thread.
#[derive(Debug, Clone)]
pub enum ProxyPipelineStatus {
    Progress(String),
    Completed,
    Error(String),
}

fn emit_status(status_tx: &Sender<ProxyPipelineStatus>, status: ProxyPipelineStatus) {
    match &status {
        ProxyPipelineStatus::Progress(msg) => info!(message = %msg, "proxy pipeline status"),
        ProxyPipelineStatus::Completed => info!("proxy pipeline completed"),
        ProxyPipelineStatus::Error(msg) => error!(message = %msg, "proxy pipeline error"),
    }
    if let Err(send_err) = status_tx.send(status) {
        warn!(error = %send_err, "failed to send proxy pipeline status to main thread");
    }
}

/// Parameters describing a single proxy render.
#[derive(Debug, Clone)]
pub struct ProxyPipelineConfig {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub preset: ProxyPreset,
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
    pub decoder: Option<String>,
}

impl ProxyPipelineConfig {
    pub fn ensure_dirs(&self) -> Result<()> {
        if let Some(parent) = self.destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create proxy directory {}", parent.display()))?;
        }
        Ok(())
    }
}

pub fn run_proxy_pipeline(
    cfg: ProxyPipelineConfig,
    status_tx: Sender<ProxyPipelineStatus>,
) -> Result<thread::JoinHandle<Result<()>>> {
    if let Err(err) = cfg.ensure_dirs() {
        emit_status(
            &status_tx,
            ProxyPipelineStatus::Error(format!("failed to prepare proxy destination: {err}")),
        );
        return Err(err);
    }

    info!(
        source = %cfg.source.display(),
        destination = %cfg.destination.display(),
        width = cfg.width,
        height = cfg.height,
        bitrate_kbps = cfg.bitrate_kbps,
        "spawning proxy pipeline thread"
    );

    let handle = thread::spawn(move || run_pipeline_thread(cfg, status_tx));
    Ok(handle)
}

fn run_pipeline_thread(
    cfg: ProxyPipelineConfig,
    status_tx: Sender<ProxyPipelineStatus>,
) -> Result<()> {
    configure_macos_gst_env();

    if let Err(init_err) = gst::init() {
        let err = anyhow!("failed to init GStreamer: {init_err}");
        emit_status(&status_tx, ProxyPipelineStatus::Error(err.to_string()));
        return Err(err);
    }

    info!(
        source = %cfg.source.display(),
        destination = %cfg.destination.display(),
        "proxy pipeline thread started"
    );
    emit_status(
        &status_tx,
        ProxyPipelineStatus::Progress(format!(
            "Starting proxy pipeline for {}",
            cfg.source.display()
        )),
    );

    let profile = detect_pipeline_profile();
    info!(selected_profile = %profile, "proxy pipeline profile selected");
    emit_status(
        &status_tx,
        ProxyPipelineStatus::Progress(format!("Selected {profile} pipeline")),
    );

    run_pipeline_for_profile(cfg, status_tx, profile)
}

fn run_pipeline_for_profile(
    cfg: ProxyPipelineConfig,
    status_tx: Sender<ProxyPipelineStatus>,
    profile: PipelineProfile,
) -> Result<()> {
    let pipeline_desc = match build_pipeline_description(&cfg, profile) {
        Ok(desc) => desc,
        Err(err) => {
            emit_status(&status_tx, ProxyPipelineStatus::Error(err.to_string()));
            return Err(err);
        }
    };

    debug!(
        profile = %profile,
        pipeline = %pipeline_desc,
        "proxy pipeline description built"
    );

    let element = match gst::parse::launch(&pipeline_desc) {
        Ok(element) => element,
        Err(parse_err) => {
            let err = anyhow!("failed to parse proxy pipeline: {parse_err}");
            emit_status(&status_tx, ProxyPipelineStatus::Error(err.to_string()));
            return Err(err);
        }
    };

    let pipeline = match element.dynamic_cast::<gst::Pipeline>() {
        Ok(pipeline) => pipeline,
        Err(_) => {
            let err = anyhow!("proxy pipeline description did not produce a GstPipeline");
            emit_status(&status_tx, ProxyPipelineStatus::Error(err.to_string()));
            return Err(err);
        }
    };

    let bus = match pipeline.bus() {
        Some(bus) => bus,
        None => {
            let err = anyhow!("pipeline missing message bus");
            emit_status(&status_tx, ProxyPipelineStatus::Error(err.to_string()));
            return Err(err);
        }
    };

    match pipeline.set_state(gst::State::Playing) {
        Ok(_) => {
            info!("proxy pipeline entered Playing state");
            emit_status(
                &status_tx,
                ProxyPipelineStatus::Progress("Proxy pipeline entered Playing state".to_string()),
            );
        }
        Err(state_err) => {
            let detail = pop_gst_error(&bus);
            pipeline.set_state(gst::State::Null).ok();
            let err = match detail {
                Some(extra) => {
                    anyhow!("set proxy pipeline to playing state: {state_err}; gst_error={extra}")
                }
                None => {
                    anyhow!("set proxy pipeline to playing state: {state_err}; no gst error on bus")
                }
            };
            emit_status(&status_tx, ProxyPipelineStatus::Error(err.to_string()));
            return Err(err);
        }
    }

    loop {
        match bus.timed_pop(gst::ClockTime::from_seconds(1)) {
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(..) => {
                    emit_status(&status_tx, ProxyPipelineStatus::Completed);
                    break;
                }
                gst::MessageView::Error(err_msg) => {
                    let src = err_msg
                        .src()
                        .map(|s| s.path_string().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let err = err_msg.error();
                    let debug = err_msg.debug().map(|d| d.to_string()).unwrap_or_default();
                    let message = format!("proxy pipeline error from {src}: {err} ({debug})");
                    emit_status(&status_tx, ProxyPipelineStatus::Error(message.clone()));
                    pipeline.set_state(gst::State::Null).ok();
                    return Err(anyhow!(message));
                }
                gst::MessageView::Buffering(buffering) => {
                    let percent = buffering.percent();
                    debug!(percent, "proxy pipeline buffering");
                    emit_status(
                        &status_tx,
                        ProxyPipelineStatus::Progress(format!("Buffering {percent}%")),
                    );
                }
                gst::MessageView::Progress(progress) => {
                    let (_, code, text) = progress.get();
                    debug!(code, text, "proxy pipeline progress");
                    emit_status(
                        &status_tx,
                        ProxyPipelineStatus::Progress(format!("Progress {code}: {text}")),
                    );
                }
                gst::MessageView::StateChanged(state_changed) => {
                    if state_changed.current() == gst::State::Playing {
                        if let Some(src) = msg.src() {
                            if src.is::<gst::Pipeline>() {
                                emit_status(
                                    &status_tx,
                                    ProxyPipelineStatus::Progress(
                                        "Pipeline confirmed Playing".to_string(),
                                    ),
                                );
                            }
                        }
                    }
                }
                _ => {}
            },
            None => continue,
        }
    }

    if let Err(reset_err) = pipeline.set_state(gst::State::Null) {
        let err = anyhow!("reset proxy pipeline state: {reset_err}");
        emit_status(&status_tx, ProxyPipelineStatus::Error(err.to_string()));
        return Err(err);
    }

    info!(
        source = %cfg.source.display(),
        destination = %cfg.destination.display(),
        "proxy pipeline finished successfully"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipelineProfile {
    AppleProRes,
    NvidiaNvenc,
    IntelVaapi,
    SoftwareFallback,
}

impl std::fmt::Display for PipelineProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineProfile::AppleProRes => write!(f, "Apple hardware ProRes"),
            PipelineProfile::NvidiaNvenc => write!(f, "NVIDIA NVENC"),
            PipelineProfile::IntelVaapi => write!(f, "Intel VAAPI"),
            PipelineProfile::SoftwareFallback => write!(f, "software DNxHR"),
        }
    }
}

fn detect_pipeline_profile() -> PipelineProfile {
    if profile_available(PipelineProfile::AppleProRes) {
        PipelineProfile::AppleProRes
    } else if profile_available(PipelineProfile::NvidiaNvenc) {
        PipelineProfile::NvidiaNvenc
    } else if profile_available(PipelineProfile::IntelVaapi) {
        PipelineProfile::IntelVaapi
    } else {
        PipelineProfile::SoftwareFallback
    }
}

fn profile_available(profile: PipelineProfile) -> bool {
    match profile {
        PipelineProfile::AppleProRes => {
            cfg!(target_os = "macos")
                && element_available("vtenc_prores")
                && (element_available("vtdec_hw") || element_available("vtdec"))
                && element_available("qtdemux")
        }
        PipelineProfile::NvidiaNvenc => {
            cfg!(any(target_os = "linux", target_os = "windows"))
                && element_available("nvh264enc")
                && element_available("nvvidconv")
        }
        PipelineProfile::IntelVaapi => {
            cfg!(target_os = "linux")
                && element_available("vah264enc")
                && element_available("vapostproc")
        }
        PipelineProfile::SoftwareFallback => true,
    }
}

fn element_available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

fn build_pipeline_description(
    cfg: &ProxyPipelineConfig,
    profile: PipelineProfile,
) -> Result<String> {
    let src = quote_path_for_gst(&cfg.source);
    let dst = quote_path_for_gst(&cfg.destination);
    let width = cfg.width.max(16);
    let height = cfg.height.max(16);
    let base_caps = format!(
        "video/x-raw,width={width},height={height},pixel-aspect-ratio=1/1,interlace-mode=progressive"
    );
    let decoder_stage = cfg
        .decoder
        .as_deref()
        .map(|name| format!("{name} ! "))
        .unwrap_or_default();

    match profile {
        PipelineProfile::AppleProRes => {
            let parser_stage = if cfg.decoder.is_some() {
                decoder_stage
            } else {
                "h264parse ! ".to_string()
            };
            let vtdec = if element_available("vtdec_hw") {
                "vtdec_hw"
            } else {
                "vtdec"
            };
            Ok(format!(
                "filesrc location={src} ! qtdemux name=demux demux.video_0 ! queue ! \
                 {parser_stage}{vtdec} ! videoscale ! {base_caps} ! \
                 vtenc_prores quality=0.5 realtime=true ! \
                 qtmux faststart=true name=mux ! filesink location={dst} async=false",
                src = src,
                parser_stage = parser_stage,
                vtdec = vtdec,
                base_caps = base_caps,
                dst = dst
            ))
        }
        PipelineProfile::NvidiaNvenc => {
            let decode_stage = if cfg.decoder.is_some() {
                decoder_stage
            } else {
                "decodebin ! ".to_string()
            };
            let nv_caps = format!(
                "video/x-raw(memory:NVMM),width={width},height={height},pixel-aspect-ratio=1/1,interlace-mode=progressive,format=NV12"
            );
            let bitrate_param = if cfg.bitrate_kbps > 0 {
                format!(" bitrate={}", cfg.bitrate_kbps * 1000)
            } else {
                String::new()
            };
            Ok(format!(
                "filesrc location={src} ! {decode_stage}queue ! nvvidconv ! {nv_caps} ! \
                 nvh264enc preset=llhp rc-mode=vbr{bitrate_param} ! h264parse ! \
                 qtmux faststart=true name=mux ! filesink location={dst} async=false",
                src = src,
                decode_stage = decode_stage,
                nv_caps = nv_caps,
                bitrate_param = bitrate_param,
                dst = dst
            ))
        }
        PipelineProfile::IntelVaapi => {
            let decode_stage = if cfg.decoder.is_some() {
                decoder_stage
            } else {
                "decodebin ! ".to_string()
            };
            let va_caps = format!(
                "video/x-raw,width={width},height={height},pixel-aspect-ratio=1/1,interlace-mode=progressive,format=NV12"
            );
            let bitrate_param = if cfg.bitrate_kbps > 0 {
                format!(" bitrate={}", cfg.bitrate_kbps * 1000)
            } else {
                String::new()
            };
            Ok(format!(
                "filesrc location={src} ! {decode_stage}queue ! vapostproc ! {va_caps} ! \
                 vah264enc{bitrate_param} ! h264parse ! \
                 qtmux faststart=true name=mux ! filesink location={dst} async=false",
                src = src,
                decode_stage = decode_stage,
                va_caps = va_caps,
                bitrate_param = bitrate_param,
                dst = dst
            ))
        }
        PipelineProfile::SoftwareFallback => {
            let decode_stage = if cfg.decoder.is_some() {
                decoder_stage
            } else {
                "decodebin3 ! ".to_string()
            };
            Ok(format!(
                "filesrc location={src} ! {decode_stage}videoconvert ! videoscale ! {base_caps} ! \
                 videoconvert dither=none ! video/x-raw,format=YUV422P ! \
                 avenc_dnxhd profile=dnxhr_lb ! queue ! \
                 qtmux faststart=true name=mux ! filesink location={dst} async=false",
                src = src,
                decode_stage = decode_stage,
                base_caps = base_caps,
                dst = dst
            ))
        }
    }
}

#[cfg(target_os = "macos")]
fn configure_macos_gst_env() {
    use std::env;

    env::set_var("GST_PLUGIN_PATH", "/opt/homebrew/lib/gstreamer-1.0");
    env::set_var("GST_PLUGIN_SYSTEM_PATH", "/opt/homebrew/lib/gstreamer-1.0");
    env::set_var(
        "GST_PLUGIN_SCANNER",
        "/opt/homebrew/libexec/gstreamer-1.0/gst-plugin-scanner",
    );
    env::set_var("GST_REGISTRY_REUSE_PLUGIN_SCANNER", "no");
}

#[cfg(not(target_os = "macos"))]
fn configure_macos_gst_env() {}

fn pop_gst_error(bus: &gst::Bus) -> Option<String> {
    for _ in 0..5 {
        match bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            Some(msg) => match msg.view() {
                gst::MessageView::Error(err) => {
                    let src = err.src().map(|s| s.path_string()).unwrap_or_default();
                    let err_msg = err.error();
                    let debug = err.debug().map(|d| d.to_string()).unwrap_or_default();
                    return Some(format!("{src}: {err_msg} ({debug})"));
                }
                _ => continue,
            },
            None => break,
        }
    }
    None
}

fn quote_path_for_gst(path: &PathBuf) -> String {
    let mut out = String::new();
    out.push('"');
    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}
