use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use native_decoder::{
    create_decoder, is_native_decoding_available, DecoderConfig, NativeVideoDecoder, VideoFrame,
    YuvPixFmt as NativeYuvPixFmt,
};

use media_io::YuvPixFmt;

use super::worker::{
    spawn_worker, DecodeCmd, DecodeWorkerRuntime, LatestFrameSlot, VideoFrameOut, WorkerStats,
    PREFETCH_BUDGET_PER_TICK,
};
use eframe::egui::Context as EguiContext;
use tracing::info;

#[derive(Default)]
pub(crate) struct DecodeManager {
    decoders: HashMap<String, DecoderEntry>,
    workers: HashMap<String, DecodeWorkerRuntime>,
    interactive_states: HashMap<String, bool>,
    worker_clip_ids: HashMap<String, Option<String>>,
    worker_stats: HashMap<String, Arc<Mutex<WorkerStats>>>,
}

struct DecoderEntry {
    decoder: Box<dyn NativeVideoDecoder>,
    zc_decoder: Option<Box<dyn NativeVideoDecoder>>, // zero-copy VT session (IOSurface)
    last_pts: Option<f64>,
    last_fmt: Option<&'static str>,
    consecutive_misses: u32,
    attempts_this_tick: u32,
    fed_samples: usize,
    draws: u32,
}

pub(crate) struct WorkerStatsSnapshot {
    pub first_frame_ms: Option<u64>,
}

impl DecodeManager {
    fn normalize_path_key(path: &str) -> String {
        fs::canonicalize(path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string())
    }

    pub(crate) fn get_or_create(
        &mut self,
        path: &str,
        cfg: &DecoderConfig,
    ) -> Result<&mut DecoderEntry> {
        let key = Self::normalize_path_key(path);
        if !self.decoders.contains_key(&key) {
            let decoder = if is_native_decoding_available() {
                create_decoder(path, cfg.clone())?
            } else {
                // TODO: on non-macOS, swap for MF/VAAPI backends when available.
                create_decoder(path, cfg.clone())?
            };
            self.decoders.insert(
                key.clone(),
                DecoderEntry {
                    decoder,
                    zc_decoder: None,
                    last_pts: None,
                    last_fmt: None,
                    consecutive_misses: 0,
                    attempts_this_tick: 0,
                    fed_samples: 0,
                    draws: 0,
                },
            );
        }
        Ok(self.decoders.get_mut(&key).unwrap())
    }

    /// Try once; if None, feed the async pipeline a few steps without blocking UI.
    pub(crate) fn decode_and_prefetch(
        &mut self,
        path: &str,
        cfg: &DecoderConfig,
        target_ts: f64,
    ) -> Option<VideoFrame> {
        let entry = self.get_or_create(path, cfg).ok()?;
        entry.attempts_this_tick = 0;

        let mut frame = entry.decoder.decode_frame(target_ts).ok().flatten();
        entry.attempts_this_tick += 1;

        let mut tries = 0;
        while frame.is_none() && tries < PREFETCH_BUDGET_PER_TICK {
            let _ = entry.decoder.decode_frame(target_ts); // advance AVF/VT asynchronously
            entry.attempts_this_tick += 1;
            tries += 1;
            frame = entry.decoder.decode_frame(target_ts).ok().flatten();
        }

        if let Some(ref f) = frame {
            entry.last_pts = Some(f.timestamp);
            entry.last_fmt = Some(match f.format {
                NativeYuvPixFmt::Nv12 => "NV12",
                NativeYuvPixFmt::P010 => "P010",
                _ => "YUV",
            });
            entry.consecutive_misses = 0;
        } else {
            entry.consecutive_misses = entry.consecutive_misses.saturating_add(1);
        }
        frame
    }

    /// Decode exactly once without advancing/prefetching (used when paused).
    pub(crate) fn decode_exact_once(
        &mut self,
        path: &str,
        cfg: &DecoderConfig,
        target_ts: f64,
    ) -> Option<VideoFrame> {
        let entry = self.get_or_create(path, cfg).ok()?;
        entry.attempts_this_tick = 0;
        let frame = entry.decoder.decode_frame(target_ts).ok().flatten();
        entry.attempts_this_tick += 1;
        if let Some(ref f) = frame {
            entry.last_pts = Some(f.timestamp);
            entry.last_fmt = Some(match f.format {
                NativeYuvPixFmt::Nv12 => "NV12",
                NativeYuvPixFmt::P010 => "P010",
                _ => "YUV",
            });
            entry.consecutive_misses = 0;
        } else {
            entry.consecutive_misses = entry.consecutive_misses.saturating_add(1);
        }
        frame
    }

    /// Attempt zero-copy decode via IOSurface. On macOS only.
    #[cfg(target_os = "macos")]
    pub(crate) fn decode_zero_copy(
        &mut self,
        path: &str,
        target_ts: f64,
    ) -> Option<native_decoder::IOSurfaceFrame> {
        use native_decoder::YuvPixFmt as Nyf;
        let key = Self::normalize_path_key(path);
        let entry = if let Some(e) = self.decoders.get_mut(&key) {
            e
        } else {
            // Initialize a CPU decoder entry first (so HUD works), then add zero-copy below.
            let cfg = DecoderConfig {
                hardware_acceleration: true,
                preferred_format: Some(Nyf::Nv12),
                zero_copy: false,
            };
            let _ = self.get_or_create(path, &cfg);
            self.decoders.get_mut(&key).unwrap()
        };
        if entry.zc_decoder.is_none() {
            let cfg_zc = DecoderConfig {
                hardware_acceleration: true,
                preferred_format: Some(Nyf::Nv12),
                zero_copy: true,
            };
            if let Ok(dec) = create_decoder(path, cfg_zc) {
                entry.zc_decoder = Some(dec);
            } else {
                return None;
            }
        }
        let dec = entry.zc_decoder.as_mut().unwrap();
        // Try a few feeds to coax out a frame without blocking long
        let mut f = dec.decode_frame_zero_copy(target_ts).ok().flatten();
        let mut tries = 0;
        while f.is_none() && tries < PREFETCH_BUDGET_PER_TICK {
            let _ = dec.decode_frame_zero_copy(target_ts);
            tries += 1;
            f = dec.decode_frame_zero_copy(target_ts).ok().flatten();
        }
        f
    }

    /// Single attempt zero-copy decode without prefetching (paused mode)
    #[cfg(target_os = "macos")]
    pub(crate) fn decode_zero_copy_once(
        &mut self,
        path: &str,
        target_ts: f64,
    ) -> Option<native_decoder::IOSurfaceFrame> {
        use native_decoder::YuvPixFmt as Nyf;
        let key = Self::normalize_path_key(path);
        let entry = if let Some(e) = self.decoders.get_mut(&key) {
            e
        } else {
            let cfg = DecoderConfig {
                hardware_acceleration: true,
                preferred_format: Some(Nyf::Nv12),
                zero_copy: false,
            };
            let _ = self.get_or_create(path, &cfg);
            self.decoders.get_mut(&key).unwrap()
        };
        if entry.zc_decoder.is_none() {
            let cfg_zc = DecoderConfig {
                hardware_acceleration: true,
                preferred_format: Some(Nyf::Nv12),
                zero_copy: true,
            };
            if let Ok(dec) = create_decoder(path, cfg_zc) {
                entry.zc_decoder = Some(dec);
            } else {
                return None;
            }
        }
        let dec = entry.zc_decoder.as_mut().unwrap();
        dec.decode_frame_zero_copy(target_ts).ok().flatten()
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn decode_zero_copy(
        &mut self,
        _path: &str,
        _target_ts: f64,
    ) -> Option<native_decoder::IOSurfaceFrame> {
        None
    }

    pub(crate) fn hud(&self, path: &str, target_ts: f64) -> String {
        let key = Self::normalize_path_key(path);
        if let Some(e) = self.decoders.get(&key) {
            let last = e.last_pts.unwrap_or(f64::NAN);
            let fmt = e.last_fmt.unwrap_or("?");
            let ring = e.decoder.ring_len();
            let cb = e.decoder.cb_frames();
            let last_cb = e.decoder.last_cb_pts();
            let fed = e.decoder.fed_samples();

            format!(
                "decode: attempts {}  misses {}  last_pts {:.3}  target {:.3}  fmt {}\nring {}  cb {}  last_cb {:.3}  fed {}  draws {}",
                e.attempts_this_tick, e.consecutive_misses, last, target_ts, fmt,
                ring, cb, last_cb, fed, e.draws
            )
        } else {
            format!("decode: initializing…  target {:.3}", target_ts)
        }
    }

    pub(crate) fn increment_draws(&mut self, path: &str) {
        let key = Self::normalize_path_key(path);
        if let Some(e) = self.decoders.get_mut(&key) {
            e.draws = e.draws.saturating_add(1);
        }
    }

    // Worker management for decoupled decode → render
    pub(crate) fn ensure_worker(
        &mut self,
        path: &str,
        clip_id: Option<&str>,
        ui_ctx: &EguiContext,
    ) {
        let key = Self::normalize_path_key(path);
        if self.workers.contains_key(&key) {
            let clip_str = clip_id
                .map(|s| s.to_string())
                .or_else(|| self.worker_clip_ids.get(&key).cloned().flatten())
                .unwrap_or_else(|| "<unknown>".to_string());
            info!("[decode] reuse_worker clip={} state=hot", clip_str);
            if let Some(id) = clip_id {
                self.worker_clip_ids
                    .insert(key.clone(), Some(id.to_string()));
            }
            if !self.worker_stats.contains_key(&key) {
                if let Some(rt) = self.workers.get(&key) {
                    self.worker_stats.insert(key.clone(), Arc::clone(&rt.stats));
                }
            }
            return;
        }
        let clip_owned = clip_id.map(|s| s.to_string());
        let stats_arc = Arc::new(Mutex::new(WorkerStats::default()));
        let rt = spawn_worker(
            &key,
            clip_owned.clone(),
            ui_ctx.clone(),
            Arc::clone(&stats_arc),
        );
        info!(
            "[decode] spawn_worker clip={} reason=playback_start (cold)",
            clip_owned.as_deref().unwrap_or("<unknown>")
        );
        self.worker_clip_ids.insert(key.clone(), clip_owned);
        self.worker_stats.insert(key.clone(), stats_arc);
        self.interactive_states.insert(key.clone(), false);
        self.workers.insert(key, rt);
    }

    pub(crate) fn send_cmd(&mut self, path: &str, cmd: DecodeCmd) {
        let key = Self::normalize_path_key(path);
        if let Some(w) = self.workers.get(&key) {
            let _ = w.cmd_tx.send(cmd);
        }
    }

    pub(crate) fn set_interactive(&mut self, path: &str, clip_id: Option<&str>, interactive: bool) {
        let key = Self::normalize_path_key(path);
        let current = self.interactive_states.get(&key).copied().unwrap_or(false);
        if current == interactive {
            return;
        }
        if let Some(w) = self.workers.get(&key) {
            let _ = w.cmd_tx.send(DecodeCmd::SetInteractive {
                active: interactive,
            });
            self.interactive_states.insert(key.clone(), interactive);
            if let Some(id) = clip_id {
                self.worker_clip_ids
                    .insert(key.clone(), Some(id.to_string()));
            }
        }
    }

    pub(crate) fn worker_stats_snapshot(&self, path: &str) -> Option<WorkerStatsSnapshot> {
        let key = Self::normalize_path_key(path);
        self.worker_stats.get(&key).and_then(|arc| {
            arc.lock().ok().map(|stats| WorkerStatsSnapshot {
                first_frame_ms: stats.first_frame_ms,
            })
        })
    }

    pub(crate) fn take_latest(&mut self, path: &str) -> Option<VideoFrameOut> {
        let key = Self::normalize_path_key(path);
        if let Some(w) = self.workers.get(&key) {
            if let Ok(g) = w.slot.0.lock() {
                // Peek instead of take to avoid UI flicker between frames.
                return g.clone();
            }
        }
        None
    }

    /// Clear the latest-frame slot to eliminate stale frames after a seek/re-anchor.
    pub(crate) fn clear_latest(&mut self, path: &str) {
        let key = Self::normalize_path_key(path);
        if let Some(w) = self.workers.get(&key) {
            if let Ok(mut g) = w.slot.0.lock() {
                *g = None;
            }
        }
    }
}
