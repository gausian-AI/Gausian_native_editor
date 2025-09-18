use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crossbeam_channel as channel;
use eframe::egui::Context as EguiContext;
use native_decoder::{create_decoder, DecoderConfig, YuvPixFmt as NativeYuvPixFmt};

use media_io::YuvPixFmt;

pub(crate) const PREFETCH_BUDGET_PER_TICK: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlayState {
    Paused,
    Seeking,
    Playing,
    Scrubbing,
}

pub(crate) struct EngineState {
    pub(crate) state: PlayState,
    pub(crate) rate: f32, // 1.0 by default
    pub(crate) target_pts: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct VideoProps {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) fps: f64,
    pub(crate) fmt: YuvPixFmt,
}

#[derive(Clone)]
pub(crate) enum FramePayload {
    Cpu { y: Arc<[u8]>, uv: Arc<[u8]> },
}

#[derive(Clone)]
pub(crate) struct VideoFrameOut {
    pub(crate) pts: f64,
    pub(crate) props: VideoProps,
    pub(crate) payload: FramePayload,
    pub(crate) accurate: bool, // true if from ACCURATE seek/streaming; false if from KEY_UNIT fast stage
}

pub(crate) enum DecodeCmd {
    Play { start_pts: f64, rate: f32 },
    Seek { target_pts: f64 },
    Pause,
    Stop,
}

pub(crate) struct LatestFrameSlot(pub(crate) Arc<Mutex<Option<VideoFrameOut>>>);

pub(crate) struct DecodeWorkerRuntime {
    #[allow(dead_code)]
    pub(crate) handle: thread::JoinHandle<()>,
    pub(crate) cmd_tx: channel::Sender<DecodeCmd>,
    pub(crate) slot: LatestFrameSlot,
}

pub(crate) fn spawn_worker(path: &str, ui_ctx: EguiContext) -> DecodeWorkerRuntime {
    use channel::{unbounded, Receiver, Sender};
    let (cmd_tx, cmd_rx) = unbounded::<DecodeCmd>();
    let slot = LatestFrameSlot(Arc::new(Mutex::new(None)));
    let slot_for_worker = LatestFrameSlot(slot.0.clone());
    let path = path.to_string();
    let handle = thread::spawn(move || {
        // Initialize decoders
        let cfg_cpu = DecoderConfig {
            hardware_acceleration: true,
            preferred_format: Some(NativeYuvPixFmt::Nv12),
            zero_copy: false,
        };
        let mut cpu_dec = match create_decoder(&path, cfg_cpu) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[worker] create_decoder CPU failed: {e}");
                return;
            }
        };
        // For now, worker outputs CPU NV12/P010 frames only (zero-copy can be added later)

        let props = cpu_dec.get_properties();
        let fps = if props.frame_rate > 0.0 {
            props.frame_rate
        } else {
            30.0
        };
        let frame_dur = if fps > 0.0 { 1.0 / fps } else { 1.0 / 30.0 };

        let mut mode = PlayState::Paused;
        let mut rate: f32 = 1.0;
        let mut anchor_pts: f64 = 0.0;
        let mut anchor_t = Instant::now();
        let mut running = true;
        let mut need_seek_decode = false; // decode until success
        let mut last_seek_target: Option<f64> = None;
        let mut seek_started_at: Option<Instant> = None;
        let mut approx_shown: bool = false;

        let mut pending: VecDeque<VideoFrameOut> = VecDeque::new();
        let mut last_repaint = Instant::now();
        while running {
            // Drain commands
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    DecodeCmd::Play { start_pts, rate: r } => {
                        // Only (re)anchor when transitioning into Playing; otherwise keep smooth progression
                        if mode != PlayState::Playing {
                            mode = PlayState::Playing;
                            anchor_pts = start_pts;
                            anchor_t = Instant::now();
                            // Resume streaming mode in decoder
                            cpu_dec.set_strict_paused(false);
                        }
                        rate = r;
                        need_seek_decode = false; // cancel any pending single-shot seek
                        last_seek_target = None;
                        seek_started_at = None;
                    }
                    DecodeCmd::Seek { target_pts } => {
                        // Enter seeking; decode exactly once at target_pts, then pause.
                        mode = PlayState::Seeking;
                        anchor_pts = target_pts;
                        need_seek_decode = true;
                        last_seek_target = None; // force a fresh seek_to on new target
                    }
                    DecodeCmd::Pause => {
                        // If a strict seek is in progress, defer pause until preroll arrives
                        if need_seek_decode {
                            // no-op; will switch to Paused after success
                        } else {
                            mode = PlayState::Paused;
                        }
                    }
                    DecodeCmd::Stop => {
                        running = false;
                    }
                }
            }

            match mode {
                PlayState::Playing => {
                    let dt = anchor_t.elapsed().as_secs_f64();
                    let target = anchor_pts + dt * (rate as f64);
                    // CPU path with a few coax attempts
                    let mut f = cpu_dec.decode_frame(target).ok().flatten();
                    let mut tries = 0;
                    while f.is_none() && tries < PREFETCH_BUDGET_PER_TICK {
                        let _ = cpu_dec.decode_frame(target);
                        tries += 1;
                        f = cpu_dec.decode_frame(target).ok().flatten();
                    }
                    if let Some(vf) = f {
                        let fmt = match vf.format {
                            NativeYuvPixFmt::Nv12 => YuvPixFmt::Nv12,
                            NativeYuvPixFmt::P010 => YuvPixFmt::P010,
                        };
                        let y: Arc<[u8]> = Arc::from(vf.y_plane.into_boxed_slice());
                        let uv: Arc<[u8]> = Arc::from(vf.uv_plane.into_boxed_slice());
                        let out = VideoFrameOut {
                            pts: vf.timestamp,
                            props: VideoProps { w: vf.width, h: vf.height, fps, fmt },
                            payload: FramePayload::Cpu { y, uv },
                            accurate: true,
                        };
                        eprintln!("[WORKER] out pts={:.3}", out.pts);
                        if let Ok(mut g) = slot_for_worker.0.lock() { *g = Some(out); }
                        // Push-driven repaint: coalesce to ~vsync (<= 60Hz)
                        if last_repaint.elapsed().as_millis() >= 8 {
                            ui_ctx.request_repaint();
                            last_repaint = Instant::now();
                        }
                    }
                    thread::sleep(std::time::Duration::from_millis(4));
                }
                PlayState::Seeking | PlayState::Scrubbing => {
                    if need_seek_decode {
                        let target = anchor_pts;
                        // Coalesce seeks: only re-issue if target changed
                        if last_seek_target.map(|t| (t - target).abs() > f64::EPSILON).unwrap_or(true) {
                            // Two-stage: first fast key-unit seek (approximate), then accurate
                            if !approx_shown {
                                let _ = cpu_dec.seek_to_keyframe(target);
                            } else {
                                let _ = cpu_dec.seek_to(target);
                            }
                            last_seek_target = Some(target);
                            seek_started_at = Some(Instant::now());
                        }
                        // Decode once (with small coax attempts)
                        let mut f = cpu_dec.decode_frame(target).ok().flatten();
                        let mut tries = 0;
                        while f.is_none() && tries < PREFETCH_BUDGET_PER_TICK {
                            let _ = cpu_dec.decode_frame(target);
                            tries += 1;
                            f = cpu_dec.decode_frame(target).ok().flatten();
                        }
                        if let Some(vf) = f {
                            let fmt = match vf.format {
                                NativeYuvPixFmt::Nv12 => YuvPixFmt::Nv12,
                                NativeYuvPixFmt::P010 => YuvPixFmt::P010,
                            };
                            let y: Arc<[u8]> = Arc::from(vf.y_plane.into_boxed_slice());
                            let uv: Arc<[u8]> = Arc::from(vf.uv_plane.into_boxed_slice());
                            let out = VideoFrameOut {
                                pts: vf.timestamp,
                                props: VideoProps { w: vf.width, h: vf.height, fps, fmt },
                                payload: FramePayload::Cpu { y, uv },
                                accurate: approx_shown, // first stage false, refine (second) true
                            };
                            eprintln!("[WORKER] out pts={:.3}", out.pts);
                            if let Ok(mut g) = slot_for_worker.0.lock() { *g = Some(out); }
                            ui_ctx.request_repaint();
                            if !approx_shown {
                                // Show approximate (key-unit) first; now refine accurately
                                approx_shown = true;
                                // Force re-seek accurately on next loop
                                last_seek_target = None;
                            } else {
                                // After accurate preroll, transition to paused and hold last frame
                                need_seek_decode = false;
                                mode = PlayState::Paused;
                                last_seek_target = None;
                                seek_started_at = None;
                                approx_shown = false;
                            }
                        } else {
                            // No frame yet: keep seeking and repaint periodically
                            ui_ctx.request_repaint();
                            // Optional timeout to avoid burning CPU; stay in Seeking (UI keeps showing 'Seeking…')
                            if let Some(t0) = seek_started_at {
                                if t0.elapsed().as_millis() > 800 {
                                    // Re-issue seek to nudge pipeline
                                    if !approx_shown {
                                        let _ = cpu_dec.seek_to_keyframe(target);
                                    } else {
                                        let _ = cpu_dec.seek_to(target);
                                    }
                                    last_seek_target = Some(target);
                                    seek_started_at = Some(Instant::now());
                                }
                            }
                        }
                    } else {
                        thread::sleep(std::time::Duration::from_millis(6));
                    }
                }
                PlayState::Paused => {
                    // Hold last frame; avoid decoding
                    thread::sleep(std::time::Duration::from_millis(8));
                }
            }
        }
    });

    DecodeWorkerRuntime {
        handle,
        cmd_tx,
        slot,
    }
}
