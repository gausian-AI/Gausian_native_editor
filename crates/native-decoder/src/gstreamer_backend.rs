//! GStreamer-backed decoder implementing the NativeVideoDecoder trait.
//!
//! This backend targets NV12 output via appsink and maps frames into the
//! existing `VideoFrame { y_plane, uv_plane, .. }` structure. It keeps
//! the API surface identical to the existing VideoToolbox decoder so the
//! desktop app can switch via the feature flag with no UI changes.

#![cfg(feature = "gstreamer")]

use crate::{DecoderConfig, NativeVideoDecoder, VideoFrame, VideoProperties, YuvPixFmt};
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tracing::{debug, warn};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::VideoFrameExt; // plane_stride/plane_data access
use gstreamer_pbutils as gst_pbutils;

// Initialize GStreamer once per process.
static GST_INIT_ONCE: AtomicBool = AtomicBool::new(false);

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
        }
        GST_INIT_ONCE.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// Create a simple pipeline: filesrc ! decodebin ! videoconvert ! NV12 ! appsink
fn build_pipeline(path: &Path) -> Result<(gst::Pipeline, gst_app::AppSink)> {
    let pipeline = gst::Pipeline::new();

    // Elements
    let src = gst::ElementFactory::make("filesrc")
        .property("location", &path.to_string_lossy().to_string())
        .build()
        .context("make filesrc with location")?;

    let decodebin = gst::ElementFactory::make("decodebin")
        .build()
        .context("make decodebin")?;

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

    // Create appsink with limited buffering.
    let appsink = gst_app::AppSink::builder()
        .caps(&caps)
        .max_buffers(8)
        .drop(true)
        .build();

    pipeline
        .add_many(&[
            &src,
            &decodebin,
            &convert,
            &capsfilter,
            appsink.upcast_ref(),
        ])
        .context("pipeline add")?;

    // Static links (except decodebin's dynamic src pads)
    gst::Element::link_many(&[&convert, &capsfilter, appsink.upcast_ref()])
        .context("link convert->capsfilter->appsink")?;
    src.link(&decodebin).context("link filesrc->decodebin")?;

    // Link decodebin's newly created video pad to videoconvert.
    let convert_weak = convert.downgrade();
    decodebin.connect_pad_added(move |_dbin, src_pad| {
        let Some(convert) = convert_weak.upgrade() else { return; };
        let Some(sink_pad) = convert.static_pad("sink") else { return; };
        if sink_pad.is_linked() {
            return;
        }
        // Attempt to link; if this is not a video pad, the link will fail and be ignored.
        let _ = src_pad.link(&sink_pad);
    });

    Ok((pipeline, appsink))
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
}

impl GstDecoder {
    pub fn new<P: AsRef<Path>>(path: P, config: DecoderConfig) -> Result<Self> {
        ensure_gst_init()?;
        let path = path.as_ref();
        let (pipeline, sink) = build_pipeline(path)?;
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
            // Give the pipeline more time to complete the accurate seek and preroll
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
            while std::time::Instant::now() < deadline {
                if let Some(msg) = self.bus.timed_pop_filtered(
                    gst::ClockTime::from_mseconds(33),
                    &[gst::MessageType::AsyncDone, gst::MessageType::Error, gst::MessageType::Eos],
                ) {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::AsyncDone(_) => break,
                        MessageView::Error(e) => {
                            debug!("GStreamer seek error from {}: {} ({:?})", e.src().map(|s| s.path_string()).unwrap_or_default(), e.error(), e.debug());
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
                    warn!("GStreamer error from {}: {} ({:?})", e.src().map(|s| s.path_string()).unwrap_or_default(), e.error(), e.debug());
                }
                _ => {}
            }
        }
    }

    fn fill_ring_from_sink(&mut self) -> Result<()> {
        // Pull frames from appsink. In strict paused mode the pipeline is PAUSED and
        // produces a preroll buffer, which must be retrieved via try_pull_preroll.
        // In streaming mode (PLAYING), use try_pull_sample.
        let attempts = if self.strict_paused { 60 } else { 6 };
        let slice_ms = if self.strict_paused { 12 } else { 5 };
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

                let (w, h) = {
                    let p = self.props.lock().unwrap();
                    (p.width.max(1), p.height.max(1))
                };

                // Prefer mapping via gst_video to handle stride/padding.
                if let Some(caps) = sample.caps() {
                    if let Ok(info) = gst_video::VideoInfo::from_caps(&caps) {
                        if let Ok(vf) = gst_video::VideoFrameRef::from_buffer_ref_readable(
                            &buffer,
                            &info,
                        ) {
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
                            if self.strict_paused { break; }
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
                        if self.strict_paused { break; }
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
            if let Ok(mut last) = self.last_seek.lock() { *last = timestamp; }
            self.ring.clear();
        }
        // Drain bus and pull a few samples into the ring
        self.drain_bus();
        self.fill_ring_from_sink()?;
        // Choose nearest at or before target
        let out = self.ring.pop_nearest_at_or_before(timestamp);
        if let Some(ref f) = out {
            if let Ok(mut last_out) = self.last_out_pts.lock() { *last_out = f.timestamp; }
        }
        Ok(out)
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
        if let Ok(mut last) = self.last_seek.lock() { *last = timestamp; }
        Ok(())
    }
}

/// Public constructor used by lib::create_decoder when the feature is enabled.
pub fn create_gst_decoder<P: AsRef<Path>>(path: P, config: DecoderConfig) -> Result<Box<dyn NativeVideoDecoder>> {
    let dec = GstDecoder::new(path, config)?;
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
    fn new(cap: usize) -> Self { Self { frames: Vec::with_capacity(cap), cap } }
    fn clear(&mut self) { self.frames.clear(); }
    fn len(&self) -> usize { self.frames.len() }
    fn push(&mut self, f: VideoFrame) {
        if self.frames.len() >= self.cap { self.frames.remove(0); }
        self.frames.push(f);
    }
    fn pop_nearest_at_or_before(&mut self, target: f64) -> Option<VideoFrame> {
        if self.frames.is_empty() { return None; }
        // Find candidate with pts <= target and maximum pts
        let mut best_idx: Option<usize> = None;
        let mut best_dt = f64::INFINITY;
        for (i, f) in self.frames.iter().enumerate() {
            let pts = f.timestamp;
            if pts.is_finite() && pts <= target {
                let dt = target - pts;
                if dt < best_dt { best_dt = dt; best_idx = Some(i); }
            }
        }
        if let Some(i) = best_idx {
            return Some(self.frames.remove(i));
        }
        // else: return the oldest
        Some(self.frames.remove(0))
    }
}
