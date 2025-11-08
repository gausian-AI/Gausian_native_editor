use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{atomic::AtomicU8, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::wgpu;

use renderer::PreviewGpuSync;

use tracing::{error, info, trace};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PlaybackPhase {
    ScrubbingOrSeeking,
    PlayingRealtime,
    Paused,
}

impl PlaybackPhase {
    pub fn allows_blocking_gpu_sync(self) -> bool {
        matches!(
            self,
            PlaybackPhase::ScrubbingOrSeeking | PlaybackPhase::Paused
        )
    }

    pub fn is_realtime_playing(self) -> bool {
        matches!(self, PlaybackPhase::PlayingRealtime)
    }
}

impl PreviewGpuSync for GpuSyncController {
    fn notify_work_submitted(&self) {
        GpuSyncController::notify_work_submitted(self);
    }
}

#[macro_export]
macro_rules! gpu_pump {
    ($ctx:expr, $reason:literal) => {{
        $ctx.gpu_sync().poll_nonblocking();
        tracing::trace!(target = "gpu_sync", reason = $reason, "gpu_pump");
    }};
}

#[macro_export]
macro_rules! gpu_service {
    ($ctx:expr, $reason:literal) => {{
        let sync = $ctx.gpu_sync();
        sync.debug_forbid_blocking_in_realtime($reason);
        sync.service_gpu($reason);
    }};
}

#[inline]
fn encode_phase(phase: PlaybackPhase) -> u8 {
    match phase {
        PlaybackPhase::ScrubbingOrSeeking => 0,
        PlaybackPhase::PlayingRealtime => 1,
        PlaybackPhase::Paused => 2,
    }
}

#[inline]
fn decode_phase(value: u8) -> PlaybackPhase {
    match value {
        0 => PlaybackPhase::ScrubbingOrSeeking,
        1 => PlaybackPhase::PlayingRealtime,
        _ => PlaybackPhase::Paused,
    }
}

pub struct GpuSyncController {
    device: Arc<wgpu::Device>,
    phase: AtomicU8,
    suppressed: Mutex<HashSet<&'static str>>,
    needs_poll: AtomicBool,
    last_poll: Mutex<Instant>,
}

#[allow(clippy::disallowed_methods)]
impl GpuSyncController {
    pub fn new(device: Arc<wgpu::Device>, phase: PlaybackPhase) -> Self {
        Self {
            device,
            phase: AtomicU8::new(encode_phase(phase)),
            suppressed: Mutex::new(HashSet::new()),
            needs_poll: AtomicBool::new(false),
            last_poll: Mutex::new(Instant::now()),
        }
    }

    pub fn set_phase(&self, phase: PlaybackPhase) {
        self.phase
            .store(encode_phase(phase), AtomicOrdering::SeqCst);
        if phase.is_realtime_playing() {
            if let Ok(mut guard) = self.suppressed.lock() {
                guard.clear();
            }
        }
    }

    pub fn phase(&self) -> PlaybackPhase {
        decode_phase(self.phase.load(AtomicOrdering::SeqCst))
    }

    pub fn is_realtime_playing(&self) -> bool {
        self.phase().is_realtime_playing()
    }

    pub fn poll_nonblocking(&self) {
        const MIN_INTERVAL: Duration = Duration::from_millis(2);
        let now = Instant::now();
        let mut last_poll = match self.last_poll.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };

        let should_poll = if self.needs_poll.load(AtomicOrdering::Acquire) {
            true
        } else {
            now.duration_since(*last_poll) >= MIN_INTERVAL
        };

        if !should_poll {
            return;
        }

        let _ = self.device.poll(wgpu::Maintain::Poll);
        *last_poll = now;
        self.needs_poll.store(false, AtomicOrdering::Release);
    }

    pub fn service_gpu(&self, reason: &'static str) -> bool {
        let phase = self.phase();
        if phase.is_realtime_playing() {
            if let Ok(mut guard) = self.suppressed.lock() {
                if guard.insert(reason) {
                    info!(
                        "[gpu_sync] suppressed blocking wait during realtime playback reason={}",
                        reason
                    );
                }
            }
            self.poll_nonblocking();
            return false;
        }

        info!(
            "[gpu_sync] blocking wait allowed phase={:?} reason={}",
            phase, reason
        );
        let _ = self.device.poll(wgpu::Maintain::Wait);
        self.needs_poll.store(false, AtomicOrdering::Release);
        true
    }

    #[track_caller]
    pub fn debug_forbid_blocking_in_realtime(&self, reason: &str) {
        if self.phase().is_realtime_playing() {
            error!(
                "BLOCKING_WAIT_LEAK during realtime playback at {:?} reason={}",
                std::panic::Location::caller(),
                reason
            );
        }
    }

    pub fn notify_work_submitted(&self) {
        self.needs_poll.store(true, AtomicOrdering::Release);
    }
}
