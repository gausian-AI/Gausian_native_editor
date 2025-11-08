use super::sync::{GpuSyncController, PlaybackPhase};
use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::wgpu;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};

fn env_sync_mode() -> bool {
    std::env::var("GAUS_READBACK_SYNC")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_fps() -> Option<u32> {
    match std::env::var("GAUS_READBACK_FPS") {
        Ok(s) => s
            .parse::<i32>()
            .ok()
            .map(|v| if v <= 0 { 0 } else { v as u32 }),
        Err(_) => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ReadbackTag {
    Thumbnail,
    Scrub,
    Capture,
    Other(String),
}

pub struct ReadbackRequest<'a> {
    pub extent: wgpu::Extent3d,
    pub bytes_per_row: u32,
    pub src: wgpu::ImageCopyTexture<'a>,
    pub tag: ReadbackTag,
}

#[derive(Clone, Debug)]
pub struct ReadbackResult {
    pub tag: ReadbackTag,
    pub pixels: Vec<u8>,
    pub extent: wgpu::Extent3d,
    pub bytes_per_row: u32,
    pub timestamp_ns: u64,
}

struct ReadbackSlot {
    buffer: wgpu::Buffer,
    capacity: u64,
    state: SlotState,
}

enum SlotState {
    Free,
    CopyPending {
        tag: ReadbackTag,
        extent: wgpu::Extent3d,
        bytes_per_row: u32,
        size: u64,
    },
    Mapping {
        tag: ReadbackTag,
        extent: wgpu::Extent3d,
        bytes_per_row: u32,
        size: u64,
        mapped: bool,
    },
}

enum MapCompletion {
    Finished {
        slot: usize,
        status: Result<(), wgpu::BufferAsyncError>,
    },
    Shutdown,
}

impl ReadbackSlot {
    fn new(device: &wgpu::Device, size: u64) -> Self {
        let size_aligned = align_u64(size.max(4), 4);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback-slot"),
            size: size_aligned,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            buffer,
            capacity: size_aligned,
            state: SlotState::Free,
        }
    }

    fn ensure_capacity(&mut self, device: &wgpu::Device, size: u64) {
        if self.capacity >= size {
            return;
        }
        let size_aligned = align_u64(size.max(4), 4);
        self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback-slot"),
            size: size_aligned,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.capacity = size_aligned;
    }
}

pub struct ReadbackManager {
    ring: usize,
    slots: Arc<Mutex<Vec<ReadbackSlot>>>,
    map_tx: Sender<MapCompletion>,
    result_rx: Receiver<ReadbackResult>,
    _worker: Option<thread::JoinHandle<()>>,
    sync_mode: bool,
    sync_warned: bool,
    _auto_fps: Option<u32>,
    last_phase: PlaybackPhase,
    did_snapshot_this_pause: bool,
    awaiting_blocking_completion: bool,
}

impl ReadbackManager {
    pub fn new(_device: &wgpu::Device, ring: usize) -> Self {
        let ring = ring.max(1);
        let sync_mode = env_sync_mode();
        let auto_fps = env_fps();

        if sync_mode {
            warn!("readback: GAUS_READBACK_SYNC=1 forcing synchronous GPU readback (debug only)");
        }

        let slots = Arc::new(Mutex::new(Vec::<ReadbackSlot>::with_capacity(ring)));
        let (map_tx, map_rx) = unbounded::<MapCompletion>();
        let (result_tx, result_rx) = unbounded::<ReadbackResult>();
        let slots_for_worker = Arc::clone(&slots);

        let worker = thread::Builder::new()
            .name("gaus-readback".to_string())
            .spawn(move || {
                while let Ok(message) = map_rx.recv() {
                    match message {
                        MapCompletion::Shutdown => break,
                        MapCompletion::Finished { slot, status } => {
                            let mut slots = match slots_for_worker.lock() {
                                Ok(lock) => lock,
                                Err(poison) => {
                                    error!("readback: slot mutex poisoned");
                                    poison.into_inner()
                                }
                            };
                            if let Some(slot_ref) = slots.get_mut(slot) {
                                if status.is_ok() {
                                    if let SlotState::Mapping { mapped, .. } = &mut slot_ref.state {
                                        *mapped = true;
                                    }
                                }
                                let state = std::mem::replace(&mut slot_ref.state, SlotState::Free);
                                match state {
                                    SlotState::Mapping {
                                        tag,
                                        extent,
                                        bytes_per_row,
                                        size,
                                        mapped,
                                    } => {
                                        match status {
                                            Ok(()) => {
                                                debug_assert!(mapped, "get_mapped_range() before map completion");
                                                let slice = slot_ref.buffer.slice(..size);
                                                let range = slice.get_mapped_range();
                                                let mut pixels =
                                                    vec![0u8; range.len()];
                                                pixels.copy_from_slice(&range);
                                                drop(range);
                                                slot_ref.buffer.unmap();
                                                let timestamp_ns =
                                                    SystemTime::now()
                                                        .duration_since(
                                                            UNIX_EPOCH,
                                                        )
                                                        .map(|d| d.as_nanos() as u64)
                                                        .unwrap_or_default();
                                                let result = ReadbackResult {
                                                    tag,
                                                    pixels,
                                                    extent,
                                                    bytes_per_row,
                                                    timestamp_ns,
                                                };
                                                let tag_for_log = result.tag.clone();
                                                let byte_count = size;
                                                if result_tx.send(result).is_err() {
                                                    warn!("readback: dropping result, receiver gone");
                                                } else {
                                                    info!(
                                                        "readback: done tag={:?} slot={} bytes={}",
                                                        tag_for_log,
                                                        slot,
                                                        byte_count
                                                    );
                                                }
                                            }
                                            Err(err) => {
                                                slot_ref.buffer.unmap();
                                                error!(
                                                    "readback: map_async failed for slot {}: {:?}",
                                                    slot, err
                                                );
                                            }
                                        }
                                    }
                                    other => {
                                        slot_ref.state = other;
                                        error!(
                                            "readback: completion for slot {} without mapping state",
                                            slot
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            })
            .ok();

        Self {
            ring,
            slots,
            map_tx,
            result_rx,
            _worker: worker,
            sync_mode,
            sync_warned: false,
            _auto_fps: auto_fps,
            last_phase: PlaybackPhase::PlayingRealtime,
            did_snapshot_this_pause: false,
            awaiting_blocking_completion: false,
        }
    }

    pub fn phase_policy_allows(&mut self, phase: PlaybackPhase) -> bool {
        let previous = self.last_phase;
        let allow = match phase {
            PlaybackPhase::ScrubbingOrSeeking => {
                self.did_snapshot_this_pause = false;
                self.awaiting_blocking_completion = false;
                true
            }
            PlaybackPhase::Paused => {
                let permitted = !self.did_snapshot_this_pause || previous != PlaybackPhase::Paused;
                if permitted {
                    self.did_snapshot_this_pause = true;
                }
                self.awaiting_blocking_completion = false;
                permitted
            }
            PlaybackPhase::PlayingRealtime => {
                self.did_snapshot_this_pause = false;
                self.awaiting_blocking_completion = false;
                if previous != PlaybackPhase::PlayingRealtime {
                    info!("[readback] suppressed during realtime playback (no GPU stall)");
                }
                false
            }
        };

        self.last_phase = phase;
        allow
    }

    pub fn mark_enqueued(&mut self, phase: PlaybackPhase) {
        if matches!(
            phase,
            PlaybackPhase::ScrubbingOrSeeking | PlaybackPhase::Paused
        ) {
            self.awaiting_blocking_completion = true;
        }
    }

    pub fn request_copy(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        req: ReadbackRequest<'_>,
    ) -> Option<usize> {
        let required_size = (req.bytes_per_row as u64)
            * (req.extent.height as u64).max(1)
            * (req.extent.depth_or_array_layers as u64).max(1);
        if required_size == 0 {
            warn!("readback: rejecting zero-sized request tag={:?}", req.tag);
            return None;
        }

        let mut slots = match self.slots.lock() {
            Ok(lock) => lock,
            Err(poison) => {
                error!("readback: slot mutex poisoned");
                poison.into_inner()
            }
        };

        let mut slot_idx = None;
        for (idx, slot) in slots.iter().enumerate() {
            if matches!(slot.state, SlotState::Free) {
                slot_idx = Some(idx);
                break;
            }
        }

        let idx = if let Some(idx) = slot_idx {
            idx
        } else if slots.len() < self.ring {
            slots.push(ReadbackSlot::new(device, required_size));
            slots.len() - 1
        } else {
            drop(slots);
            info!("readback: ring full, dropping request tag={:?}", req.tag);
            return None;
        };

        let slot = &mut slots[idx];
        slot.ensure_capacity(device, required_size);
        slot.state = SlotState::CopyPending {
            tag: req.tag.clone(),
            extent: req.extent,
            bytes_per_row: req.bytes_per_row,
            size: required_size,
        };
        encoder.copy_texture_to_buffer(
            req.src,
            wgpu::ImageCopyBuffer {
                buffer: &slot.buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(req.bytes_per_row),
                    rows_per_image: Some(req.extent.height),
                },
            },
            req.extent,
        );

        Some(idx)
    }

    pub fn schedule_map(&mut self, slot_index: usize) {
        let mut slots = match self.slots.lock() {
            Ok(lock) => lock,
            Err(poison) => {
                error!("readback: slot mutex poisoned");
                poison.into_inner()
            }
        };

        let Some(slot_ref) = slots.get_mut(slot_index) else {
            warn!("readback: schedule_map invalid slot index {}", slot_index);
            return;
        };

        let (tag, extent, bytes_per_row, size) =
            match std::mem::replace(&mut slot_ref.state, SlotState::Free) {
                SlotState::CopyPending {
                    tag,
                    extent,
                    bytes_per_row,
                    size,
                } => (tag, extent, bytes_per_row, size),
                other => {
                    slot_ref.state = other;
                    warn!(
                        "readback: schedule_map called for slot {} without pending copy",
                        slot_index
                    );
                    return;
                }
            };

        slot_ref.state = SlotState::Mapping {
            tag,
            extent,
            bytes_per_row,
            size,
            mapped: false,
        };

        let slice = slot_ref.buffer.slice(..size);
        let map_tx = self.map_tx.clone();
        slice.map_async(wgpu::MapMode::Read, move |status| {
            let _ = map_tx.send(MapCompletion::Finished {
                slot: slot_index,
                status,
            });
        });
    }

    pub fn try_recv(&mut self) -> Option<ReadbackResult> {
        self.result_rx.try_recv().ok()
    }

    pub fn has_pending(&self) -> bool {
        let (slots, results) = self.pending_counts();
        slots > 0 || results > 0
    }

    pub fn poll(&mut self, controller: &GpuSyncController) -> bool {
        let phase = controller.phase();
        let (pending_slots, pending_results) = self.pending_counts();

        if self.sync_mode && !self.sync_warned {
            warn!("readback: GAUS_READBACK_SYNC=1 enabling blocking waits");
            self.sync_warned = true;
        }

        let allow_blocking = phase.allows_blocking_gpu_sync();
        let mut use_wait = false;
        if allow_blocking {
            if self.sync_mode {
                use_wait = pending_slots > 0;
            } else {
                use_wait = self.awaiting_blocking_completion && pending_slots > 0;
            }
        } else {
            self.awaiting_blocking_completion = false;
        }

        let mut forced_wait = false;
        if use_wait {
            controller.debug_forbid_blocking_in_realtime("readback::poll");
            let waited = controller.service_gpu("readback::poll");
            if !waited {
                self.awaiting_blocking_completion = false;
            } else {
                forced_wait = true;
            }
        } else {
            controller.poll_nonblocking();
            if pending_slots == 0 && pending_results == 0 {
                std::thread::yield_now();
            }
        }

        let mode_str = if forced_wait { "Wait" } else { "Poll" };
        info!(
            "[readback] poll mode={} phase={:?} pending_slots={} pending_results={}",
            mode_str, phase, pending_slots, pending_results
        );

        let (slots_after, results_after) = self.pending_counts();
        if slots_after == 0 && results_after == 0 {
            self.awaiting_blocking_completion = false;
        }

        forced_wait
    }

    fn pending_counts(&self) -> (usize, usize) {
        let slots_in_flight = self
            .slots
            .lock()
            .map(|slots| {
                slots
                    .iter()
                    .filter(|slot| {
                        matches!(
                            slot.state,
                            SlotState::CopyPending { .. } | SlotState::Mapping { .. }
                        )
                    })
                    .count()
            })
            .unwrap_or(0);
        let results_ready = self.result_rx.len();
        (slots_in_flight, results_ready)
    }
}

impl Drop for ReadbackManager {
    fn drop(&mut self) {
        let _ = self.map_tx.send(MapCompletion::Shutdown);
        if let Some(handle) = self._worker.take() {
            let _ = handle.join();
        }
    }
}

fn align_u64(value: u64, align: u64) -> u64 {
    ((value + align - 1) / align) * align
}

#[cfg(all(test, feature = "readback-test"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn throttle_interval_limits_rate() {
        let mut last = None;
        let interval = Duration::from_millis(100);
        let now = Instant::now();

        let should_fire = |last: &mut Option<Instant>, instant: Instant| -> bool {
            if let Some(prev) = last {
                if instant.duration_since(*prev) < interval {
                    return false;
                }
            }
            *last = Some(instant);
            true
        };

        assert!(should_fire(&mut last, now));
        assert!(!should_fire(&mut last, now + Duration::from_millis(10)));
        assert!(should_fire(&mut last, now + Duration::from_millis(120)));
    }

    #[test]
    fn default_mode_is_non_blocking() {
        std::env::remove_var("GAUS_READBACK_SYNC");
        let (_instance, _adapter, device, _queue) = super::readback_test_helpers::test_bootstrap();
        let mgr = ReadbackManager::new(&device, 2);
        assert!(!mgr.sync_mode);
    }
}

#[cfg(all(test, feature = "readback-test"))]
mod readback_test_helpers {
    use super::*;
    use pollster::block_on;

    pub fn test_bootstrap() -> (wgpu::Instance, wgpu::Adapter, wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::default();
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("adapter");
        let (device, queue) =
            block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .expect("device");
        (instance, adapter, device, queue)
    }
}
