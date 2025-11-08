use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::TextureHandle;
use eframe::egui_wgpu;
use eframe::{egui, wgpu};
use media_io::YuvPixFmt;
use native_decoder::{
    self, create_decoder, is_native_decoding_available, DecoderConfig, NativeVideoDecoder,
    VideoFrame, YuvPixFmt as NativeYuvPixFmt,
};
use renderer::{
    ColorSpace as RendererColorSpace, PixelFormat as RendererPixelFormat, PreviewDownscale,
    PreviewFrameInput, PreviewReadbackResources, PreviewTextureSource,
};

use crate::decode::PlayState;
use crate::gpu::context::GpuContext;
use crate::gpu::readback::{ReadbackManager, ReadbackRequest, ReadbackResult, ReadbackTag};
use crate::gpu::sync::{GpuSyncController, PlaybackPhase};
use crate::preview::visual_source_at;
use crate::VisualSource;
use crate::PRESENT_SIZE_MISMATCH_LOGGED;
use tracing::{debug, error, info, trace, warn};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreviewShaderMode {
    Solid,
    ShowY,
    UvDebug,
    Nv12,
}

impl Default for PreviewShaderMode {
    fn default() -> Self {
        PreviewShaderMode::Solid
    }
}

pub(crate) struct StreamSlot {
    pub(crate) stream_id: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fmt: YuvPixFmt,
    pub(crate) clear_color: egui::Color32,
    pub(crate) y_tex: Option<Arc<eframe::wgpu::Texture>>,
    pub(crate) uv_tex: Option<Arc<eframe::wgpu::Texture>>,
    pub(crate) out_tex: Option<Arc<eframe::wgpu::Texture>>,
    pub(crate) out_view: Option<eframe::wgpu::TextureView>,
    pub(crate) egui_tex_id: Option<egui::TextureId>,
}

pub(crate) struct StreamMetadata {
    pub(crate) stream_id: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fmt: YuvPixFmt,
    pub(crate) clear_color: egui::Color32,
}

struct ScheduledReadback {
    tag: ReadbackTag,
    auto: bool,
}

struct ReadbackSource {
    rgba_texture: Arc<wgpu::Texture>,
    y_plane: Option<Arc<wgpu::Texture>>,
    uv_plane: Option<Arc<wgpu::Texture>>,
    format: Option<YuvPixFmt>,
    width: u32,
    height: u32,
}

pub(crate) struct InteractivePolicy {
    clip_id: Option<String>,
    tier: Option<String>,
    playback_started_at: Option<Instant>,
    interactive_active: bool,
    first_frame_ms: Option<u64>,
    last_forced_wait: Option<Instant>,
    last_lag: Option<Instant>,
    hold_logged: bool,
    last_play_state: Option<PlayState>,
}

impl InteractivePolicy {
    fn new() -> Self {
        Self {
            clip_id: None,
            tier: None,
            playback_started_at: None,
            interactive_active: true,
            first_frame_ms: None,
            last_forced_wait: None,
            last_lag: None,
            hold_logged: false,
            last_play_state: None,
        }
    }

    fn reset_for_clip(&mut self, clip_id: &str, tier: &str) {
        self.clip_id = Some(clip_id.to_string());
        self.tier = Some(tier.to_string());
        self.playback_started_at = None;
        self.interactive_active = true;
        self.first_frame_ms = None;
        self.last_forced_wait = None;
        self.last_lag = None;
        self.hold_logged = false;
        self.last_play_state = None;
    }

    pub(crate) fn note_first_frame_ms(&mut self, value: Option<u64>) {
        if let Some(ms) = value {
            if self.first_frame_ms.is_none() {
                self.first_frame_ms = Some(ms);
            }
        }
    }

    pub(crate) fn note_forced_wait(&mut self, now: Instant) {
        self.last_forced_wait = Some(now);
        if matches!(self.last_play_state, Some(PlayState::Playing)) {
            self.playback_started_at = Some(now);
            self.hold_logged = false;
        }
    }

    pub(crate) fn note_lag(&mut self, lagging: bool, play_state: PlayState, now: Instant) {
        if lagging && matches!(play_state, PlayState::Playing) {
            self.last_lag = Some(now);
            self.playback_started_at = Some(now);
            self.hold_logged = false;
        }
    }

    pub(crate) fn evaluate(
        &mut self,
        clip_id: &str,
        tier: &str,
        play_state: PlayState,
        now: Instant,
    ) -> bool {
        if self.clip_id.as_deref() != Some(clip_id) || self.tier.as_deref() != Some(tier) {
            self.reset_for_clip(clip_id, tier);
        }

        if !matches!(self.last_play_state, Some(PlayState::Playing))
            && matches!(play_state, PlayState::Playing)
        {
            self.playback_started_at = Some(now);
            self.last_forced_wait = None;
            self.last_lag = None;
            self.hold_logged = false;
        }

        if matches!(play_state, PlayState::Scrubbing | PlayState::Seeking) {
            self.playback_started_at = Some(now);
            self.interactive_active = true;
            self.hold_logged = false;
            self.last_play_state = Some(play_state);
            return true;
        }

        if matches!(play_state, PlayState::Paused) {
            if self.interactive_active {
                info!(
                    "[interactive] upscale clip={} mode=fullres trigger=paused",
                    clip_id
                );
            }
            self.interactive_active = false;
            self.hold_logged = false;
            self.playback_started_at = None;
            self.last_play_state = Some(play_state);
            return false;
        }

        // Remaining cases are Playing (or fallback default)
        if tier != "original" {
            if self.interactive_active {
                info!(
                    "[interactive] upscale clip={} mode=fullres trigger=proxy",
                    clip_id
                );
            }
            self.interactive_active = false;
            self.hold_logged = false;
            self.last_play_state = Some(play_state);
            return false;
        }

        if matches!(play_state, PlayState::Playing) && self.playback_started_at.is_none() {
            self.playback_started_at = Some(now);
        }

        let mut desired = self.interactive_active;
        let mut trigger: Option<&'static str> = None;

        if self.interactive_active {
            if let Some(start) = self.playback_started_at {
                let window = Duration::from_millis(750);
                let first_ok = self.first_frame_ms.map(|ms| ms <= 33).unwrap_or(false);
                let waited_enough = now.duration_since(start) >= window;
                let lag_clear = self
                    .last_lag
                    .map(|t| now.duration_since(t) >= window)
                    .unwrap_or(true);
                let wait_clear = self
                    .last_forced_wait
                    .map(|t| now.duration_since(t) >= window)
                    .unwrap_or(true);
                if first_ok && waited_enough && lag_clear && wait_clear {
                    desired = false;
                    trigger = Some("realtime");
                }
            }
        }

        if desired != self.interactive_active {
            if let Some(reason) = trigger {
                info!(
                    "[interactive] upscale clip={} mode=fullres trigger={}",
                    clip_id, reason
                );
            }
            self.interactive_active = desired;
            if !desired {
                self.hold_logged = false;
                self.last_forced_wait = None;
                self.last_lag = None;
            }
        } else if self.interactive_active
            && !matches!(play_state, PlayState::Playing)
            && !self.hold_logged
        {
            info!(
                "[interactive] hold_lowres clip={} reason=no_realtime_or_proxy tier={}",
                clip_id, tier
            );
            self.hold_logged = true;
        }

        self.last_play_state = Some(play_state);
        self.interactive_active
    }
}

pub(crate) struct PreviewState {
    pub(crate) texture: Option<TextureHandle>,
    pub(crate) stream: Option<StreamSlot>,
    pub(crate) last_pts: Option<f64>,
    pub(crate) frame_cache: Arc<Mutex<HashMap<FrameCacheKey, CachedFrame>>>,
    pub(crate) cache_worker: Option<JoinHandle<()>>,
    pub(crate) cache_stop: Option<Arc<AtomicBool>>,
    pub(crate) current_source: Option<VisualSource>,
    pub(crate) last_frame_time: f64,
    pub(crate) last_size: (u32, u32),
    pub(crate) gpu_tex_a: Option<Arc<eframe::wgpu::Texture>>,
    pub(crate) gpu_view_a: Option<eframe::wgpu::TextureView>,
    pub(crate) gpu_tex_b: Option<Arc<eframe::wgpu::Texture>>,
    pub(crate) gpu_view_b: Option<eframe::wgpu::TextureView>,
    pub(crate) gpu_use_b: bool,
    pub(crate) gpu_tex_id: Option<egui::TextureId>,
    pub(crate) gpu_size: (u32, u32),
    pub(crate) y_tex: [Option<Arc<eframe::wgpu::Texture>>; 3],
    pub(crate) uv_tex: [Option<Arc<eframe::wgpu::Texture>>; 3],
    pub(crate) y_size: (u32, u32),
    pub(crate) uv_size: (u32, u32),
    pub(crate) ring_write: usize,
    pub(crate) ring_present: usize,
    nv12_cache: HashMap<FrameCacheKey, Nv12Frame>,
    nv12_keys: VecDeque<FrameCacheKey>,
    pub(crate) cache_hits: u64,
    pub(crate) cache_misses: u64,
    pub(crate) decode_time_ms: f64,
    pub(crate) last_fmt: Option<YuvPixFmt>,
    pub(crate) last_cpu_tick: u64,
    pub(crate) last_present_tick: u64,
    pub(crate) shader_mode: PreviewShaderMode,
    #[cfg(target_os = "macos")]
    pub(crate) gpu_yuv: Option<native_decoder::GpuYuv>,
    #[cfg(target_os = "macos")]
    pub(crate) last_zc: Option<(
        YuvPixFmt,
        Arc<eframe::wgpu::Texture>,
        Arc<eframe::wgpu::Texture>,
        (u32, u32),
    )>,
    #[cfg(target_os = "macos")]
    pub(crate) last_zc_tick: u64,
    #[cfg(target_os = "macos")]
    pub(crate) zc_logged: bool,
    readback_backend: Box<dyn PreviewReadback + Send>,
    readback_pending: VecDeque<ScheduledReadback>,
    readback_results: VecDeque<ReadbackResult>,
    readback_inflight: HashMap<ReadbackTag, Instant>,
    readback_last_submit: HashMap<ReadbackTag, Instant>,
    readback_ring: usize,
    readback_scale: f32,
    readback_auto_interval: Option<Duration>,
    readback_last_auto: Option<Instant>,
    readback_last_scrub: Option<Instant>,
    readback_fallback_reason: Option<String>,
    renderer_realtime_min_interval: Duration,
    gpu_sync: Option<Arc<GpuSyncController>>,
    gpu_phase: Option<PlaybackPhase>,
    pub(crate) interactive_policy: InteractivePolicy,
    pub(crate) last_logged_playback: Option<String>,
    pub(crate) last_interactive_request: Option<bool>,
    pub(crate) last_play_state_for_readback: Option<PlayState>,
}

impl PreviewState {
    pub(crate) fn new() -> Self {
        let settings = PreviewReadbackSettings::from_env();
        if renderer_backend_default_enabled() {
            Self::new_with_renderer(RendererBackendOptions::from_env(settings))
        } else {
            Self::new_without_renderer_with_settings(settings)
        }
    }

    pub(crate) fn new_with_renderer(options: RendererBackendOptions) -> Self {
        let settings = options.settings;
        Self::with_backend(
            Box::new(RendererReadbackBackend::new(options)),
            settings,
            options.realtime_min_interval,
        )
    }

    pub(crate) fn new_without_renderer() -> Self {
        let settings = PreviewReadbackSettings::from_env();
        Self::new_without_renderer_with_settings(settings)
    }

    fn new_without_renderer_with_settings(settings: PreviewReadbackSettings) -> Self {
        let backend = Box::new(ReadbackManagerBackend::new(settings.ring));
        Self::with_backend(backend, settings, Duration::from_millis(0))
    }

    fn with_backend(
        backend: Box<dyn PreviewReadback + Send>,
        settings: PreviewReadbackSettings,
        renderer_realtime_min_interval: Duration,
    ) -> Self {
        let backend_name = backend.name();

        let state = Self {
            texture: None,
            stream: None,
            last_pts: None,
            frame_cache: Arc::new(Mutex::new(HashMap::new())),
            cache_worker: None,
            cache_stop: None,
            current_source: None,
            last_frame_time: -1.0,
            last_size: (0, 0),
            gpu_tex_a: None,
            gpu_view_a: None,
            gpu_tex_b: None,
            gpu_view_b: None,
            gpu_use_b: false,
            gpu_tex_id: None,
            gpu_size: (0, 0),
            y_tex: [None, None, None],
            uv_tex: [None, None, None],
            y_size: (0, 0),
            uv_size: (0, 0),
            ring_write: 0,
            ring_present: 0,
            nv12_cache: HashMap::new(),
            nv12_keys: VecDeque::new(),
            cache_hits: 0,
            cache_misses: 0,
            decode_time_ms: 0.0,
            last_fmt: None,
            last_cpu_tick: 0,
            last_present_tick: 0,
            shader_mode: PreviewShaderMode::Nv12,
            #[cfg(target_os = "macos")]
            gpu_yuv: None,
            #[cfg(target_os = "macos")]
            last_zc: None,
            #[cfg(target_os = "macos")]
            last_zc_tick: 0,
            #[cfg(target_os = "macos")]
            zc_logged: false,
            readback_backend: backend,
            readback_pending: VecDeque::new(),
            readback_results: VecDeque::new(),
            readback_inflight: HashMap::new(),
            readback_last_submit: HashMap::new(),
            readback_ring: settings.ring,
            readback_scale: settings.scale,
            readback_auto_interval: settings.auto_interval,
            readback_last_auto: None,
            readback_last_scrub: None,
            readback_fallback_reason: None,
            renderer_realtime_min_interval,
            gpu_sync: None,
            gpu_phase: None,
            interactive_policy: InteractivePolicy::new(),
            last_logged_playback: None,
            last_interactive_request: None,
            last_play_state_for_readback: None,
        };

        info!(
            target = "preview_readback",
            backend = backend_name,
            scale = settings.scale,
            ring = settings.ring,
            "preview readback backend initialized"
        );

        state
    }

    pub(crate) fn update_gpu_phase(
        &mut self,
        rs: &eframe::egui_wgpu::RenderState,
        phase: PlaybackPhase,
    ) {
        let controller = match self.gpu_sync.as_ref() {
            Some(existing) => existing.clone(),
            None => {
                let ctrl = Arc::new(GpuSyncController::new(rs.device.clone(), phase));
                self.gpu_sync = Some(ctrl.clone());
                ctrl
            }
        };
        controller.set_phase(phase);

        if self.gpu_phase != Some(phase) {
            if matches!(phase, PlaybackPhase::PlayingRealtime) {
                self.readback_backend.clear_for_realtime();
                self.readback_pending.clear();
                self.readback_inflight.clear();
                self.readback_results.clear();
                self.readback_last_auto = None;
                self.readback_last_scrub = None;
            }
            self.gpu_phase = Some(phase);
        }
    }

    pub(crate) fn gpu_context<'a>(
        &self,
        rs: &'a eframe::egui_wgpu::RenderState,
    ) -> Option<GpuContext<'a>> {
        self.gpu_sync
            .as_ref()
            .cloned()
            .map(|sync| GpuContext::new(rs, sync))
    }

    pub(crate) fn gpu_sync_controller(&self) -> Option<Arc<GpuSyncController>> {
        self.gpu_sync.as_ref().cloned()
    }

    pub(crate) fn ensure_stream_slot<'a>(
        &'a mut self,
        gpu: &GpuContext<'_>,
        renderer: &mut eframe::egui_wgpu::Renderer,
        meta: StreamMetadata,
    ) -> &'a mut StreamSlot {
        let StreamMetadata {
            stream_id,
            width,
            height,
            fmt,
            clear_color,
        } = meta;

        let ready = matches!(
            self.stream.as_ref(),
            Some(slot)
                if slot.stream_id == stream_id
                    && slot.width == width
                    && slot.height == height
                    && slot.fmt == fmt
                    && slot.y_tex.is_some()
                    && slot.uv_tex.is_some()
                    && slot.out_tex.is_some()
                    && slot.out_view.is_some()
                    && slot.egui_tex_id.is_some()
        );

        if ready {
            return self.stream.as_mut().unwrap();
        }

        if let Some(slot) = self.stream.take() {
            if let Some(id) = slot.egui_tex_id {
                renderer.free_texture(&id);
            }
        }

        let (y_format, uv_format) = match fmt {
            YuvPixFmt::Nv12 => (
                eframe::wgpu::TextureFormat::R8Unorm,
                eframe::wgpu::TextureFormat::Rg8Unorm,
            ),
            YuvPixFmt::P010 => (
                eframe::wgpu::TextureFormat::R16Unorm,
                eframe::wgpu::TextureFormat::Rg16Unorm,
            ),
        };

        let (y_tex, uv_tex, out_tex, out_view, tex_id) = gpu.with_device(|device| {
            let y_tex = Arc::new(device.create_texture(&eframe::wgpu::TextureDescriptor {
                label: Some("preview_stream_y"),
                size: eframe::wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: eframe::wgpu::TextureDimension::D2,
                format: y_format,
                usage: eframe::wgpu::TextureUsages::COPY_DST
                    | eframe::wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            }));

            let uv_tex = Arc::new(device.create_texture(&eframe::wgpu::TextureDescriptor {
                label: Some("preview_stream_uv"),
                size: eframe::wgpu::Extent3d {
                    width: (width + 1) / 2,
                    height: (height + 1) / 2,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: eframe::wgpu::TextureDimension::D2,
                format: uv_format,
                usage: eframe::wgpu::TextureUsages::COPY_DST
                    | eframe::wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            }));

            let out_tex = Arc::new(device.create_texture(&eframe::wgpu::TextureDescriptor {
                label: Some("preview_stream_out"),
                size: eframe::wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: eframe::wgpu::TextureDimension::D2,
                format: eframe::wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: eframe::wgpu::TextureUsages::COPY_DST
                    | eframe::wgpu::TextureUsages::TEXTURE_BINDING
                    | eframe::wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            }));

            let out_view = out_tex.create_view(&eframe::wgpu::TextureViewDescriptor::default());
            let tex_id = renderer.register_native_texture(
                device,
                &out_view,
                eframe::wgpu::FilterMode::Linear,
            );
            (y_tex, uv_tex, out_tex, out_view, tex_id)
        });

        self.stream = Some(StreamSlot {
            stream_id,
            width,
            height,
            fmt,
            clear_color,
            y_tex: Some(y_tex),
            uv_tex: Some(uv_tex),
            out_tex: Some(out_tex),
            out_view: Some(out_view),
            egui_tex_id: Some(tex_id),
        });

        self.stream.as_mut().unwrap()
    }

    // Ensure triple-buffer NV12 plane textures at native size
    pub(crate) fn ensure_yuv_textures(
        &mut self,
        gpu: &GpuContext<'_>,
        w: u32,
        h: u32,
        fmt: YuvPixFmt,
    ) {
        let y_sz = (w, h);
        let uv_sz = ((w + 1) / 2, (h + 1) / 2);
        if self.y_size == y_sz
            && self.uv_size == uv_sz
            && self.y_tex[0].is_some()
            && self.uv_tex[0].is_some()
        {
            return;
        }
        let supports16 = device_supports_16bit_norm(gpu);
        let (y_format, uv_format) = match fmt {
            YuvPixFmt::Nv12 => (
                eframe::wgpu::TextureFormat::R8Unorm,
                eframe::wgpu::TextureFormat::Rg8Unorm,
            ),
            YuvPixFmt::P010 => {
                if supports16 {
                    (
                        eframe::wgpu::TextureFormat::R16Unorm,
                        eframe::wgpu::TextureFormat::Rg16Unorm,
                    )
                } else {
                    (
                        eframe::wgpu::TextureFormat::R16Uint,
                        eframe::wgpu::TextureFormat::Rg16Uint,
                    )
                }
            }
        };

        let create_y = |device: &eframe::wgpu::Device| {
            device.create_texture(&eframe::wgpu::TextureDescriptor {
                label: Some("preview_nv12_y"),
                size: eframe::wgpu::Extent3d {
                    width: y_sz.0,
                    height: y_sz.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: eframe::wgpu::TextureDimension::D2,
                format: y_format,
                usage: eframe::wgpu::TextureUsages::COPY_DST
                    | eframe::wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let create_uv = |device: &eframe::wgpu::Device| {
            device.create_texture(&eframe::wgpu::TextureDescriptor {
                label: Some("preview_nv12_uv"),
                size: eframe::wgpu::Extent3d {
                    width: uv_sz.0,
                    height: uv_sz.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: eframe::wgpu::TextureDimension::D2,
                format: uv_format,
                usage: eframe::wgpu::TextureUsages::COPY_DST
                    | eframe::wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };

        gpu.with_device(|device| {
            for i in 0..3 {
                self.y_tex[i] = Some(std::sync::Arc::new(create_y(device)));
                self.uv_tex[i] = Some(std::sync::Arc::new(create_uv(device)));
            }
        });

        self.ring_write = 0;
        self.ring_present = 0;
        self.y_size = y_sz;
        self.uv_size = uv_sz;
    }

    pub(crate) fn upload_yuv_planes(
        &mut self,
        gpu: &GpuContext<'_>,
        fmt: YuvPixFmt,
        y: &[u8],
        uv: &[u8],
        w: u32,
        h: u32,
    ) {
        self.ensure_yuv_textures(gpu, w, h, fmt);
        let next_idx = (self.ring_write + 1) % 3;
        if next_idx == self.ring_present {
            eprintln!(
                "[RING DROP] write={} present={} (dropping frame to avoid stall)",
                self.ring_write, self.ring_present
            );
            return;
        }
        let idx = self.ring_write % 3;
        let y_tex = self.y_tex[idx].as_ref().map(|a| &**a).unwrap();
        let uv_tex = self.uv_tex[idx].as_ref().map(|a| &**a).unwrap();

        let uv_w = (w + 1) / 2;
        let uv_h = (h + 1) / 2;
        let (y_bpp, uv_bpp_per_texel) = match fmt {
            YuvPixFmt::Nv12 => (1usize, 2usize),
            YuvPixFmt::P010 => (2usize, 4usize),
        };

        gpu.with_queue(|queue| {
            upload_plane(queue, y_tex, y, w, h, (w as usize) * y_bpp, y_bpp);
            upload_plane(
                queue,
                uv_tex,
                uv,
                uv_w,
                uv_h,
                (uv_w as usize) * uv_bpp_per_texel,
                uv_bpp_per_texel,
            );
        });

        self.ring_present = idx;
        self.ring_write = next_idx;
        self.last_fmt = Some(fmt);
        crate::gpu_pump!(gpu, "upload_yuv_planes");
    }

    pub(crate) fn current_plane_textures(
        &self,
    ) -> Option<(
        YuvPixFmt,
        std::sync::Arc<eframe::wgpu::Texture>,
        std::sync::Arc<eframe::wgpu::Texture>,
    )> {
        let mut best: Option<(
            u64,
            YuvPixFmt,
            std::sync::Arc<eframe::wgpu::Texture>,
            std::sync::Arc<eframe::wgpu::Texture>,
        )> = None;
        if let Some(fmt) = self.last_fmt {
            let idx = self.ring_present % 3;
            if let (Some(y), Some(uv)) = (self.y_tex[idx].as_ref(), self.uv_tex[idx].as_ref()) {
                best = Some((self.last_cpu_tick, fmt, y.clone(), uv.clone()));
            }
        }
        #[cfg(target_os = "macos")]
        if let Some((fmt, y, uv, _sz)) = self.last_zc.as_ref() {
            match best {
                Some((tick, ..)) if self.last_zc_tick <= tick => {}
                _ => {
                    best = Some((self.last_zc_tick, *fmt, y.clone(), uv.clone()));
                }
            }
        }
        best.map(|(_, fmt, y, uv)| (fmt, y, uv))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn ensure_zero_copy_nv12_textures(&mut self, gpu: &GpuContext<'_>, w: u32, h: u32) {
        let target_y = (w, h);
        let target_uv = ((w + 1) / 2, (h + 1) / 2);
        let needs_new = match &self.gpu_yuv {
            Some(_) if self.y_size == target_y && self.uv_size == target_uv => false,
            _ => true,
        };
        if !needs_new {
            return;
        }

        let (y_tex, uv_tex) = gpu.with_device(|device| {
            let make_tex = |label: &str, size: (u32, u32), format: eframe::wgpu::TextureFormat| {
                Arc::new(device.create_texture(&eframe::wgpu::TextureDescriptor {
                    label: Some(label),
                    size: eframe::wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: eframe::wgpu::TextureDimension::D2,
                    format,
                    usage: eframe::wgpu::TextureUsages::COPY_DST
                        | eframe::wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                }))
            };

            let y_tex = make_tex(
                "preview_zc_nv12_y",
                target_y,
                eframe::wgpu::TextureFormat::R8Unorm,
            );
            let uv_tex = make_tex(
                "preview_zc_nv12_uv",
                target_uv,
                eframe::wgpu::TextureFormat::Rg8Unorm,
            );
            (y_tex, uv_tex)
        });

        self.gpu_yuv = Some(native_decoder::GpuYuv {
            y_tex: y_tex.clone(),
            uv_tex: uv_tex.clone(),
        });
        self.y_size = target_y;
        self.uv_size = target_uv;
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn set_last_zc_present(
        &mut self,
        fmt: YuvPixFmt,
        y_tex: std::sync::Arc<eframe::wgpu::Texture>,
        uv_tex: std::sync::Arc<eframe::wgpu::Texture>,
        w: u32,
        h: u32,
    ) {
        self.last_zc = Some((fmt, y_tex, uv_tex, (w, h)));
        self.last_fmt = Some(fmt);
        self.y_size = (w, h);
        self.uv_size = ((w + 1) / 2, (h + 1) / 2);
        self.last_present_tick = self.last_present_tick.wrapping_add(1);
        self.last_zc_tick = self.last_present_tick;
    }

    pub(crate) fn present_yuv(
        &mut self,
        gpu: &GpuContext<'_>,
        path: &str,
        t_sec: f64,
    ) -> Option<(
        YuvPixFmt,
        Arc<eframe::wgpu::Texture>,
        Arc<eframe::wgpu::Texture>,
    )> {
        let key = FrameCacheKey::new(path, t_sec, 0, 0);
        let mut fmt;
        let mut y;
        let mut uv;
        let mut w;
        let mut h;
        if let Some(hit) = self.nv12_cache.get(&key) {
            fmt = hit.fmt;
            y = hit.y.clone();
            uv = hit.uv.clone();
            w = hit.w;
            h = hit.h;
            if let Some(pos) = self.nv12_keys.iter().position(|k| k == &key) {
                self.nv12_keys.remove(pos);
            }
            self.nv12_keys.push_back(key.clone());
        } else {
            if let Ok(frame) = media_io::decode_yuv_at(std::path::Path::new(path), t_sec) {
                fmt = frame.fmt;
                y = frame.y;
                uv = frame.uv;
                w = frame.width;
                h = frame.height;
                if fmt == YuvPixFmt::P010 && !device_supports_16bit_norm(gpu) {
                    if let Some((_f, ny, nuv, nw, nh)) = decode_video_frame_nv12_only(path, t_sec) {
                        fmt = YuvPixFmt::Nv12;
                        y = ny;
                        uv = nuv;
                        w = nw;
                        h = nh;
                    }
                }
                self.nv12_cache.insert(
                    key.clone(),
                    Nv12Frame {
                        fmt,
                        y: y.clone(),
                        uv: uv.clone(),
                        w,
                        h,
                    },
                );
                self.nv12_keys.push_back(key.clone());
                if self.nv12_keys.len() > 64 {
                    if let Some(old) = self.nv12_keys.pop_front() {
                        self.nv12_cache.remove(&old);
                    }
                }
            } else {
                return None;
            }
        }
        self.upload_yuv_planes(gpu, fmt, &y, &uv, w, h);
        let idx = self.ring_present;
        Some((
            fmt,
            self.y_tex[idx].as_ref().unwrap().clone(),
            self.uv_tex[idx].as_ref().unwrap().clone(),
        ))
    }

    // Ensure double-buffered GPU textures and a registered TextureId
    pub(crate) fn ensure_gpu_textures(
        &mut self,
        gpu: &GpuContext<'_>,
        rs: &eframe::egui_wgpu::RenderState,
        w: u32,
        h: u32,
    ) {
        if self.gpu_size == (w, h)
            && self.gpu_tex_id.is_some()
            && (self.gpu_view_a.is_some() || self.gpu_view_b.is_some())
        {
            return;
        }

        let (tex_a, view_a, tex_b, view_b) = gpu.with_device(|device| {
            let make_tex = || {
                device.create_texture(&eframe::wgpu::TextureDescriptor {
                    label: Some("preview_native_tex"),
                    size: eframe::wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: eframe::wgpu::TextureDimension::D2,
                    format: eframe::wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: eframe::wgpu::TextureUsages::COPY_DST
                        | eframe::wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
            };
            let tex_a = std::sync::Arc::new(make_tex());
            let view_a = tex_a.create_view(&eframe::wgpu::TextureViewDescriptor::default());
            let tex_b = std::sync::Arc::new(make_tex());
            let view_b = tex_b.create_view(&eframe::wgpu::TextureViewDescriptor::default());
            (tex_a, view_a, tex_b, view_b)
        });

        // Register a TextureId if needed, otherwise update it to A initially
        let mut renderer = rs.renderer.write();
        gpu.with_device(|device| {
            if let Some(id) = self.gpu_tex_id {
                renderer.update_egui_texture_from_wgpu_texture(
                    device,
                    &view_a,
                    eframe::wgpu::FilterMode::Linear,
                    id,
                );
            } else {
                let id = renderer.register_native_texture(
                    device,
                    &view_a,
                    eframe::wgpu::FilterMode::Linear,
                );
                self.gpu_tex_id = Some(id);
            }
        });

        self.gpu_tex_a = Some(tex_a);
        self.gpu_view_a = Some(view_a);
        self.gpu_tex_b = Some(tex_b);
        self.gpu_view_b = Some(view_b);
        self.gpu_use_b = false;
        self.gpu_size = (w, h);
    }

    // Upload RGBA bytes into the next back buffer and retarget the TextureId to it
    pub(crate) fn upload_gpu_frame(
        &mut self,
        gpu: &GpuContext<'_>,
        rs: &eframe::egui_wgpu::RenderState,
        rgba: &[u8],
    ) {
        let (w, h) = self.gpu_size;
        // swap buffer
        self.gpu_use_b = !self.gpu_use_b;
        let (tex, view) = if self.gpu_use_b {
            (
                self.gpu_tex_b.as_ref().map(|a| &**a),
                self.gpu_view_b.as_ref(),
            )
        } else {
            (
                self.gpu_tex_a.as_ref().map(|a| &**a),
                self.gpu_view_a.as_ref(),
            )
        };
        if let (Some(tex), Some(view)) = (tex, view) {
            let bytes_per_row = (w * 4) as usize;
            let align = eframe::wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize; // 256
            let padded_bpr = ((bytes_per_row + align - 1) / align) * align;
            gpu.with_queue(|queue| {
                if padded_bpr == bytes_per_row {
                    queue.write_texture(
                        eframe::wgpu::ImageCopyTexture {
                            texture: tex,
                            mip_level: 0,
                            origin: eframe::wgpu::Origin3d::ZERO,
                            aspect: eframe::wgpu::TextureAspect::All,
                        },
                        rgba,
                        eframe::wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: Some(bytes_per_row as u32),
                            rows_per_image: Some(h),
                        },
                        eframe::wgpu::Extent3d {
                            width: w,
                            height: h,
                            depth_or_array_layers: 1,
                        },
                    );
                } else {
                    // build a padded buffer per row to satisfy alignment
                    let mut padded = vec![0u8; padded_bpr * (h as usize)];
                    for row in 0..(h as usize) {
                        let src_off = row * bytes_per_row;
                        let dst_off = row * padded_bpr;
                        padded[dst_off..dst_off + bytes_per_row]
                            .copy_from_slice(&rgba[src_off..src_off + bytes_per_row]);
                    }
                    queue.write_texture(
                        eframe::wgpu::ImageCopyTexture {
                            texture: tex,
                            mip_level: 0,
                            origin: eframe::wgpu::Origin3d::ZERO,
                            aspect: eframe::wgpu::TextureAspect::All,
                        },
                        &padded,
                        eframe::wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: Some(padded_bpr as u32),
                            rows_per_image: Some(h),
                        },
                        eframe::wgpu::Extent3d {
                            width: w,
                            height: h,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            });
            if let Some(id) = self.gpu_tex_id {
                let mut renderer = rs.renderer.write();
                gpu.with_device(|device| {
                    renderer.update_egui_texture_from_wgpu_texture(
                        device,
                        view,
                        eframe::wgpu::FilterMode::Linear,
                        id,
                    );
                });
            }
            crate::gpu_pump!(gpu, "upload_gpu_frame");
        }
    }

    // Present a GPU-cached frame for a source/time. If absent, decode one and upload.
    pub(crate) fn present_gpu_cached(
        &mut self,
        gpu: &GpuContext<'_>,
        rs: &eframe::egui_wgpu::RenderState,
        path: &str,
        t_sec: f64,
        desired: (u32, u32),
    ) -> Option<egui::TextureId> {
        self.ensure_gpu_textures(gpu, rs, desired.0, desired.1);
        // Try cache first
        let key = FrameCacheKey::new(path, t_sec, desired.0, desired.1);
        if let Some(cached) = self.get_cached_frame(&key) {
            let mut bytes = Vec::with_capacity(cached.image.pixels.len() * 4);
            for p in &cached.image.pixels {
                bytes.extend_from_slice(&p.to_array());
            }
            self.upload_gpu_frame(gpu, rs, &bytes);
            return self.gpu_tex_id; // ignored in wgpu path; retained for compatibility
        }
        // Decode one frame on demand
        let decoded = if path.to_lowercase().ends_with(".png")
            || path.to_lowercase().ends_with(".jpg")
            || path.to_lowercase().ends_with(".jpeg")
        {
            decode_image_optimized(path, desired.0, desired.1)
        } else {
            decode_video_frame_optimized(path, t_sec, desired.0, desired.1)
        };
        if let Some(img) = decoded {
            let mut bytes = Vec::with_capacity(img.pixels.len() * 4);
            for p in &img.pixels {
                bytes.extend_from_slice(&p.to_array());
            }
            self.upload_gpu_frame(gpu, rs, &bytes);
            return self.gpu_tex_id; // ignored in wgpu path; retained for compatibility
        }
        None
    }

    pub(crate) fn update(
        &mut self,
        ctx: &egui::Context,
        size: (u32, u32),
        source: Option<&VisualSource>,
        _playing: bool,
        t_sec: f64,
    ) {
        // Check if we need to update the frame
        let need_update = match source {
            Some(src) => {
                self.current_source.as_ref().map_or(true, |current| {
                    current.path != src.path ||
                    (t_sec - self.last_frame_time).abs() > 0.05 || // Update every 50ms for smooth scrubbing
                    self.last_size != size
                })
            }
            None => self.current_source.is_some(),
        };

        if need_update {
            self.current_source = source.cloned();
            self.last_frame_time = t_sec;
            self.last_size = size;

            if let Some(src) = source {
                // Try to get frame from cache first
                let cache_key = FrameCacheKey::new(&src.path, t_sec, size.0, size.1);

                if let Some(_cached_frame) = self.get_cached_frame(&cache_key) {
                    // Cache hit - let present_gpu_cached upload to native WGPU on paint
                    self.cache_hits += 1;
                    ctx.request_repaint();
                } else {
                    // Cache miss - decode frame asynchronously
                    self.cache_misses += 1;
                    self.decode_frame_async(ctx, src.clone(), cache_key, t_sec);
                }
            } else {
                // no source
            }
        }
    }

    pub(crate) fn get_cached_frame(&self, key: &FrameCacheKey) -> Option<CachedFrame> {
        if let Ok(cache) = self.frame_cache.lock() {
            if let Some(mut frame) = cache.get(key).cloned() {
                frame.access_count += 1;
                frame.last_access = std::time::Instant::now();
                return Some(frame);
            }
        }
        None
    }

    pub(crate) fn decode_frame_async(
        &mut self,
        ctx: &egui::Context,
        source: VisualSource,
        cache_key: FrameCacheKey,
        t_sec: f64,
    ) {
        // If native decoding is available and this is a video, do not spawn RGBA decoding.
        // The persistent native decoder will feed frames via the ring buffer.
        if !source.is_image && is_native_decoding_available() {
            return;
        }
        let cache = self.frame_cache.clone();
        let ctx = ctx.clone();

        // Stop any existing cache worker
        if let Some(stop) = &self.cache_stop {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(worker) = self.cache_worker.take() {
            let _ = worker.join();
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        self.cache_stop = Some(stop_flag.clone());

        let worker = thread::spawn(move || {
            if stop_flag.load(Ordering::Relaxed) {
                return;
            }

            let start_time = std::time::Instant::now();

            // Decode frame efficiently
            let frame_result = if source.is_image {
                decode_image_optimized(&source.path, cache_key.width, cache_key.height)
            } else {
                // Use native decoder if available, fallback to FFmpeg
                if is_native_decoding_available() {
                    decode_video_frame_native(
                        &source.path,
                        t_sec,
                        cache_key.width,
                        cache_key.height,
                    )
                } else {
                    decode_video_frame_optimized(
                        &source.path,
                        t_sec,
                        cache_key.width,
                        cache_key.height,
                    )
                }
            };

            if stop_flag.load(Ordering::Relaxed) {
                return;
            }

            if let Some(image) = frame_result {
                let _decode_time = start_time.elapsed();

                // Cache the frame
                let cached_frame = CachedFrame {
                    image: image.clone(),
                    decoded_at: std::time::Instant::now(),
                    access_count: 1,
                    last_access: std::time::Instant::now(),
                };

                if let Ok(mut cache) = cache.lock() {
                    // Implement LRU eviction if cache is too large
                    if cache.len() > 50 {
                        // Max 50 cached frames
                        evict_lru_frames(&mut cache, 10); // Remove oldest 10 frames
                    }

                    cache.insert(cache_key, cached_frame);
                }

                // Update texture on main thread
                ctx.request_repaint();
            }
        });

        self.cache_worker = Some(worker);
    }

    pub(crate) fn stop_cache_worker(&mut self) {
        if let Some(stop) = &self.cache_stop {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(worker) = self.cache_worker.take() {
            let _ = worker.join();
        }
        self.cache_stop = None;
    }

    pub(crate) fn print_cache_stats(&self) {
        let total_requests = self.cache_hits + self.cache_misses;
        if total_requests > 0 {
            let hit_rate = (self.cache_hits as f64 / total_requests as f64) * 100.0;
            println!(
                "Preview Cache Stats: {:.1}% hit rate ({}/{} requests), avg decode: {:.1}ms",
                hit_rate, self.cache_hits, total_requests, self.decode_time_ms
            );
        }
    }

    pub(crate) fn preload_nearby_frames(
        &self,
        source: &VisualSource,
        current_time: f64,
        size: (u32, u32),
    ) {
        if source.is_image {
            return;
        } // No need to preload for images

        let cache = self.frame_cache.clone();
        let source = source.clone();
        let (w, h) = size;

        // Preload frames around current time (±2 seconds)
        thread::spawn(move || {
            let _preload_range = 2.0; // seconds
            let _step = 0.2; // every 200ms

            for offset in [0.2, 0.4, 0.6, 0.8, 1.0, -0.2, -0.4, -0.6, -0.8, -1.0] {
                let preload_time = current_time + offset;
                if preload_time < 0.0 {
                    continue;
                }

                let cache_key = FrameCacheKey::new(&source.path, preload_time, w, h);

                // Check if frame is already cached
                if let Ok(cache) = cache.lock() {
                    if cache.contains_key(&cache_key) {
                        continue; // Already cached
                    }
                }

                // Decode frame in background
                if let Some(image) = decode_video_frame_optimized(&source.path, preload_time, w, h)
                {
                    let cached_frame = CachedFrame {
                        image,
                        decoded_at: std::time::Instant::now(),
                        access_count: 0,
                        last_access: std::time::Instant::now(),
                    };

                    if let Ok(mut cache) = cache.lock() {
                        // Only cache if we're not over the limit
                        if cache.len() < 50 {
                            cache.insert(cache_key, cached_frame);
                        }
                    }
                }

                // Small delay to avoid overwhelming the system
                thread::sleep(Duration::from_millis(10));
            }
        });
    }

    pub(crate) fn present_yuv_with_frame(
        &mut self,
        gpu: &GpuContext<'_>,
        path: &str,
        t_sec: f64,
        vf_opt: Option<&native_decoder::VideoFrame>,
    ) -> Option<(
        YuvPixFmt,
        Arc<eframe::wgpu::Texture>,
        Arc<eframe::wgpu::Texture>,
    )> {
        if let Some(vf) = vf_opt {
            // Map NativeYuvPixFmt to local YuvPixFmt and handle P010->NV12 fallback
            let mut fmt = match vf.format {
                native_decoder::YuvPixFmt::Nv12 => YuvPixFmt::Nv12,
                native_decoder::YuvPixFmt::P010 => YuvPixFmt::P010,
            };
            let mut y: Vec<u8> = vf.y_plane.clone();
            let mut uv: Vec<u8> = vf.uv_plane.clone();
            let w = vf.width;
            let h = vf.height;
            if fmt == YuvPixFmt::P010 && !device_supports_16bit_norm(gpu) {
                if let Some((_f, ny, nuv, nw, nh)) = decode_video_frame_nv12_only(path, t_sec) {
                    fmt = YuvPixFmt::Nv12;
                    y = ny;
                    uv = nuv;
                    let _ = (nw, nh);
                }
            }
            let key = FrameCacheKey::new(path, t_sec, 0, 0);
            self.nv12_cache.insert(
                key.clone(),
                Nv12Frame {
                    fmt,
                    y: y.clone(),
                    uv: uv.clone(),
                    w,
                    h,
                },
            );
            self.nv12_keys.push_back(key);
            while self.nv12_keys.len() > 64 {
                if let Some(old) = self.nv12_keys.pop_front() {
                    self.nv12_cache.remove(&old);
                }
            }
            self.upload_yuv_planes(gpu, fmt, &y, &uv, w, h);
            let idx = self.ring_present;
            return Some((
                fmt,
                self.y_tex[idx].as_ref().unwrap().clone(),
                self.uv_tex[idx].as_ref().unwrap().clone(),
            ));
        }
        // Fallback to old path
        self.present_yuv(gpu, path, t_sec)
    }

    pub(crate) fn present_yuv_from_bytes(
        &mut self,
        gpu: &GpuContext<'_>,
        fmt: YuvPixFmt,
        y_bytes: &[u8],
        uv_bytes: &[u8],
        w: u32,
        h: u32,
    ) -> Option<(
        YuvPixFmt,
        Arc<eframe::wgpu::Texture>,
        Arc<eframe::wgpu::Texture>,
    )> {
        // Ensure textures/buffers exist at this decoded size/format
        self.ensure_yuv_textures(gpu, w, h, fmt);

        // Write into current ring slot
        let wi = self.ring_write % 3;

        let (y_bpp, uv_bpp_per_texel) = match fmt {
            YuvPixFmt::Nv12 => (1usize, 2usize),
            YuvPixFmt::P010 => (2usize, 4usize),
        };
        let y_w = w as usize;
        let y_h = h as usize;
        let uv_w = ((w + 1) / 2) as usize;
        let uv_h = ((h + 1) / 2) as usize;

        // Guard: verify plane lengths once; early out if mismatched
        let expected_y = y_w * y_bpp * y_h;
        let expected_uv = uv_w * uv_bpp_per_texel * uv_h;
        debug_assert_eq!(y_bytes.len(), expected_y, "Y plane size mismatch");
        debug_assert_eq!(uv_bytes.len(), expected_uv, "UV plane size mismatch");
        if y_bytes.len() != expected_y || uv_bytes.len() != expected_uv {
            let flag = PRESENT_SIZE_MISMATCH_LOGGED.get_or_init(|| AtomicBool::new(false));
            if !flag.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[present] size mismatch: got Y={} UV={}, expected Y={} UV={}",
                    y_bytes.len(),
                    uv_bytes.len(),
                    expected_y,
                    expected_uv
                );
            }
            return None;
        }

        gpu.with_queue(|queue| {
            if let Some(y_tex) = self.y_tex[wi].as_ref() {
                upload_plane(queue, &**y_tex, y_bytes, w, h, y_w * y_bpp, y_bpp);
            }

            if let Some(uv_tex) = self.uv_tex[wi].as_ref() {
                upload_plane(
                    queue,
                    &**uv_tex,
                    uv_bytes,
                    (w + 1) / 2,
                    (h + 1) / 2,
                    uv_w * uv_bpp_per_texel,
                    uv_bpp_per_texel,
                );
            }
        });
        crate::gpu_pump!(gpu, "present_yuv_from_bytes");

        // Persist last-good so fallback can reuse
        self.last_fmt = Some(fmt);
        self.y_size = (w, h);
        self.uv_size = ((w + 1) / 2, (h + 1) / 2);
        self.ring_present = wi;
        self.ring_write = (wi + 1) % 3;
        self.last_present_tick = self.last_present_tick.wrapping_add(1);
        self.last_cpu_tick = self.last_present_tick;

        let y_tex = self.y_tex[wi].as_ref()?.clone();
        let uv_tex = self.uv_tex[wi].as_ref()?.clone();
        Some((fmt, y_tex, uv_tex))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn present_nv12_zero_copy(
        &mut self,
        gpu: &GpuContext<'_>,
        zc: &native_decoder::IOSurfaceFrame,
    ) -> Option<(
        YuvPixFmt,
        Arc<eframe::wgpu::Texture>,
        Arc<eframe::wgpu::Texture>,
    )> {
        self.ensure_zero_copy_nv12_textures(gpu, zc.width, zc.height);
        if let Some((y_arc, uv_arc)) = self
            .gpu_yuv
            .as_ref()
            .map(|g| (g.y_tex.clone(), g.uv_tex.clone()))
        {
            let import_result = gpu.with_queue(|queue| {
                self.gpu_yuv
                    .as_ref()
                    .unwrap()
                    .import_from_iosurface(queue, zc)
            });
            if let Err(e) = import_result {
                eprintln!("[zc] import_from_iosurface error: {}", e);
                return None;
            }
            #[cfg(target_os = "macos")]
            if !self.zc_logged {
                tracing::info!(
                    "[preview] imported NV12 planes: Y={}x{}  UV={}x{}",
                    zc.width,
                    zc.height,
                    (zc.width + 1) / 2,
                    (zc.height + 1) / 2
                );
                self.zc_logged = true;
            }
            // Persist last ZC for reuse
            self.set_last_zc_present(
                YuvPixFmt::Nv12,
                y_arc.clone(),
                uv_arc.clone(),
                zc.width,
                zc.height,
            );
            crate::gpu_pump!(gpu, "present_nv12_zero_copy");
            return Some((YuvPixFmt::Nv12, y_arc, uv_arc));
        }
        None
    }

    pub(crate) fn request_scrub_readback(&mut self) {
        self.enqueue_readback(ReadbackTag::Scrub, false);
    }

    pub(crate) fn request_capture_readback(&mut self) {
        self.enqueue_readback(ReadbackTag::Capture, false);
    }

    pub(crate) fn process_readback(&mut self, gpu: &GpuContext<'_>, play_state: PlayState) {
        if let Some(interval) = self.readback_auto_interval {
            if matches!(play_state, PlayState::Scrubbing | PlayState::Seeking) {
                let should_emit = self
                    .readback_last_auto
                    .map(|last| last.elapsed() >= interval)
                    .unwrap_or(true);
                if should_emit {
                    self.enqueue_readback(ReadbackTag::Scrub, true);
                }
            }
        }

        let Some(controller) = self.gpu_sync_controller() else {
            return;
        };
        let phase = controller.phase();

        let backend_has_pending = self.readback_backend.has_pending();
        let has_work = backend_has_pending || !self.readback_pending.is_empty();
        if !has_work {
            return;
        }

        let poll = self.readback_backend.poll(controller.as_ref(), phase);
        if poll.forced_wait {
            self.interactive_policy.note_forced_wait(Instant::now());
        }

        while let Some(result) = self.readback_backend.try_recv() {
            self.readback_inflight.remove(&result.tag);
            self.readback_last_submit
                .insert(result.tag.clone(), Instant::now());
            self.push_readback_result(result);
        }

        let Some(source) = self.build_readback_source() else {
            return;
        };

        let mut deferred: Vec<ScheduledReadback> = Vec::new();

        while let Some(job) = self.readback_pending.pop_front() {
            if self.should_defer_request(&job, phase) {
                deferred.push(job);
                continue;
            }

            let (target_width, target_height) =
                self.calculate_target_dimensions(source.width.max(1), source.height.max(1));
            let bytes_per_row = align_bytes_per_row(target_width);
            let allow_blocking = !phase.is_realtime_playing();

            let request = PreviewReadbackRequest {
                tag: &job.tag,
                auto: job.auto,
                phase,
                source: &source,
                original_width: source.width.max(1),
                original_height: source.height.max(1),
                target_width,
                target_height,
                bytes_per_row,
                allow_blocking,
            };

            match self.readback_backend.readback(gpu, &request) {
                Ok(ReadbackSubmission::Completed(result)) => {
                    self.readback_last_submit
                        .insert(job.tag.clone(), Instant::now());
                    self.note_readback_success(&job);
                    self.readback_inflight.remove(&job.tag);
                    self.push_readback_result(result);
                }
                Ok(ReadbackSubmission::Submitted) => {
                    self.record_inflight(&job);
                    self.note_readback_success(&job);
                }
                Ok(ReadbackSubmission::Pending) => {
                    if phase.is_realtime_playing()
                        && matches!(self.readback_backend.kind(), PreviewReadbackKind::Legacy)
                    {
                        // Legacy manager cannot service realtime requests without blocking; drop request.
                    } else {
                        deferred.push(job);
                    }
                }
                Err(err) => {
                    self.handle_readback_error(err, &job);
                    deferred.push(job);
                }
            }
        }

        if !deferred.is_empty() {
            self.readback_pending.extend(deferred);
        }
    }

    pub(crate) fn take_readback_results(&mut self) -> Vec<ReadbackResult> {
        self.readback_results.drain(..).collect()
    }

    fn enqueue_readback(&mut self, tag: ReadbackTag, auto: bool) {
        if self
            .readback_pending
            .iter()
            .any(|pending| pending.tag == tag && pending.auto == auto)
        {
            return;
        }

        if let Some(start) = self.readback_inflight.get(&tag) {
            if start.elapsed() < readback_inflight_timeout(&tag) {
                return;
            }
            self.readback_inflight.remove(&tag);
        }

        if let Some(last) = self.readback_last_submit.get(&tag) {
            let min_gap = readback_min_interval(&tag, auto);
            if last.elapsed() < min_gap {
                return;
            }
        }

        self.readback_pending
            .push_back(ScheduledReadback { tag, auto });
    }

    fn build_readback_source(&self) -> Option<ReadbackSource> {
        let slot = self.stream.as_ref()?;
        let width = slot.width;
        let height = slot.height;
        if width == 0 || height == 0 {
            return None;
        }
        let rgba_texture = Arc::clone(slot.out_tex.as_ref()?);
        Some(ReadbackSource {
            rgba_texture,
            y_plane: slot.y_tex.as_ref().map(Arc::clone),
            uv_plane: slot.uv_tex.as_ref().map(Arc::clone),
            format: Some(slot.fmt),
            width,
            height,
        })
    }

    fn calculate_target_dimensions(&self, original_width: u32, original_height: u32) -> (u32, u32) {
        let scale = self.readback_scale.clamp(0.1, 1.0);
        if (scale - 1.0).abs() < f32::EPSILON {
            (original_width, original_height)
        } else {
            (
                (original_width as f32 * scale).round().max(1.0) as u32,
                (original_height as f32 * scale).round().max(1.0) as u32,
            )
        }
    }

    fn should_defer_request(&self, job: &ScheduledReadback, phase: PlaybackPhase) -> bool {
        if job.auto && matches!(job.tag, ReadbackTag::Scrub) {
            if let Some(interval) = self.readback_auto_interval {
                if let Some(last) = self.readback_last_scrub {
                    if last.elapsed() < interval {
                        return true;
                    }
                }
            }
        }

        if phase.is_realtime_playing()
            && matches!(self.readback_backend.kind(), PreviewReadbackKind::Renderer)
            && !matches!(job.tag, ReadbackTag::Capture)
        {
            if self.renderer_realtime_min_interval > Duration::from_millis(0) {
                if let Some(last) = self.readback_last_submit.get(&job.tag) {
                    if last.elapsed() < self.renderer_realtime_min_interval {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn record_inflight(&mut self, job: &ScheduledReadback) {
        let now = Instant::now();
        self.readback_inflight.insert(job.tag.clone(), now);
        self.readback_last_submit.insert(job.tag.clone(), now);
    }

    fn note_readback_success(&mut self, job: &ScheduledReadback) {
        if matches!(job.tag, ReadbackTag::Scrub) {
            let now = Instant::now();
            self.readback_last_scrub = Some(now);
            if job.auto {
                self.readback_last_auto = Some(now);
            }
        }
    }

    fn push_readback_result(&mut self, result: ReadbackResult) {
        if self.readback_results.len() >= self.readback_ring {
            self.readback_results.pop_front();
        }
        self.readback_results.push_back(result);
    }

    fn handle_readback_error(&mut self, error: PreviewReadbackError, job: &ScheduledReadback) {
        match error {
            PreviewReadbackError::Unsupported { reason } => {
                warn!(
                    target = "preview_readback",
                    tag = ?job.tag,
                    reason,
                    "preview readback unsupported; switching to legacy backend"
                );
                self.fallback_to_legacy(&reason);
            }
            PreviewReadbackError::SubmissionFailed { reason } => {
                warn!(
                    target = "preview_readback",
                    tag = ?job.tag,
                    reason,
                    "preview readback submission failed"
                );
            }
        }
    }

    fn fallback_to_legacy(&mut self, reason: &str) {
        if matches!(self.readback_backend.kind(), PreviewReadbackKind::Legacy) {
            return;
        }
        warn!(
            target = "preview_readback",
            reason, "falling back to legacy preview readback backend"
        );
        self.readback_backend = Box::new(ReadbackManagerBackend::new(self.readback_ring));
        self.readback_backend.clear_for_realtime();
        self.readback_inflight.clear();
        self.readback_fallback_reason = Some(reason.to_string());
    }
}

fn find_jpeg_frame(buf: &[u8]) -> Option<(usize, usize)> {
    // SOI 0xFFD8, EOI 0xFFD9
    let mut start = None;
    for i in 0..buf.len().saturating_sub(1) {
        if start.is_none() && buf[i] == 0xFF && buf[i + 1] == 0xD8 {
            start = Some(i);
        }
        if let Some(s) = start {
            if buf[i] == 0xFF && buf[i + 1] == 0xD9 {
                return Some((s, i + 2));
            }
        }
    }
    None
}

fn decode_to_color_image(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let data = img.into_raw();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        &data,
    ))
}

struct ReadbackDownscale {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

struct ReadbackResolve {
    texture: Arc<wgpu::Texture>,
    width: u32,
    height: u32,
}

impl ReadbackDownscale {
    fn new(device: &wgpu::Device) -> Self {
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("readback-downscale-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("readback-downscale-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("readback-downscale-shader"),
            source: wgpu::ShaderSource::Wgsl(READBACK_DOWNSCALE_SHADER.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("readback-downscale-pipeline-layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("readback-downscale-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Self {
            pipeline,
            bind_layout,
            sampler,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PreviewReadbackSettings {
    ring: usize,
    scale: f32,
    auto_interval: Option<Duration>,
}

impl PreviewReadbackSettings {
    fn from_env() -> Self {
        Self {
            ring: readback_env_ring(),
            scale: readback_env_scale(),
            auto_interval: readback_env_interval(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RendererBackendOptions {
    pub(crate) settings: PreviewReadbackSettings,
    pub(crate) realtime_min_interval: Duration,
}

impl RendererBackendOptions {
    fn new(settings: PreviewReadbackSettings, realtime_min_interval: Duration) -> Self {
        Self {
            settings,
            realtime_min_interval,
        }
    }

    fn from_env(settings: PreviewReadbackSettings) -> Self {
        let realtime_min_interval = std::env::var("GAUS_PREVIEW_RENDERER_REALTIME_FPS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .and_then(|fps| {
                if fps > 0 {
                    Some(Duration::from_millis((1000 / fps.max(1)) as u64))
                } else {
                    None
                }
            })
            .unwrap_or(Duration::from_millis(100));
        Self::new(settings, realtime_min_interval)
    }
}

fn renderer_backend_default_enabled() -> bool {
    std::env::var("GAUS_PREVIEW_RENDERER")
        .map(|value| {
            let lower = value.trim().to_ascii_lowercase();
            matches!(lower.as_str(), "1" | "true" | "on" | "yes")
        })
        .unwrap_or(false)
}

fn readback_env_ring() -> usize {
    std::env::var("GAUS_READBACK_RING")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|ring| ring.max(1))
        .unwrap_or(4)
}

fn readback_min_interval(tag: &ReadbackTag, auto: bool) -> Duration {
    match tag {
        ReadbackTag::Scrub => {
            if auto {
                Duration::from_millis(160)
            } else {
                Duration::from_millis(60)
            }
        }
        ReadbackTag::Capture => Duration::from_millis(0),
        ReadbackTag::Thumbnail => Duration::from_millis(200),
        ReadbackTag::Other(_) => Duration::from_millis(120),
    }
}

fn readback_inflight_timeout(tag: &ReadbackTag) -> Duration {
    match tag {
        ReadbackTag::Capture => Duration::from_millis(800),
        _ => Duration::from_millis(300),
    }
}

fn readback_env_scale() -> f32 {
    std::env::var("GAUS_READBACK_SCALE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .map(|scale| scale.clamp(0.1, 1.0))
        .unwrap_or(0.5)
}

fn readback_env_interval() -> Option<Duration> {
    std::env::var("GAUS_READBACK_FPS")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .and_then(|fps| {
            if fps > 0.0 {
                let ms = (1000.0 / fps).round().max(1.0) as u64;
                Some(Duration::from_millis(ms))
            } else {
                None
            }
        })
}

fn align_bytes_per_row(width: u32) -> u32 {
    let base = width.max(1) * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    ((base + align - 1) / align) * align
}

const READBACK_DOWNSCALE_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0)
    );

    var output: VertexOutput;
    let pos = positions[idx];
    output.position = vec4<f32>(pos, 0.0, 1.0);
    output.uv = vec2<f32>((pos.x + 1.0) * 0.5, 1.0 - ((pos.y + 1.0) * 0.5));
    return output;
}

@group(0) @binding(0)
var input_tex: texture_2d<f32>;
@group(0) @binding(1)
var input_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(input_tex, input_sampler, clamp(in.uv, vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0)));
}
"#;

// Optimized video frame decode at native size (no scaling; GPU handles fit)
fn decode_video_frame_optimized(
    path: &str,
    t_sec: f64,
    w: u32,
    h: u32,
) -> Option<egui::ColorImage> {
    // Decode one frame at requested size to match GPU upload
    let frame_bytes = (w as usize) * (h as usize) * 4;
    let out = std::process::Command::new("ffmpeg")
        .arg("-ss")
        .arg(format!("{:.3}", t_sec.max(0.0)))
        .arg("-i")
        .arg(path)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(format!("scale={}x{}:flags=fast_bilinear", w, h))
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-threads")
        .arg("1")
        .arg("-")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }
    if out.stdout.len() < frame_bytes {
        return None;
    }
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        &out.stdout[..frame_bytes],
    ))
}

// Decode video frame using native decoder
fn decode_video_frame_native(path: &str, t_sec: f64, w: u32, h: u32) -> Option<egui::ColorImage> {
    let config = DecoderConfig {
        hardware_acceleration: true,
        preferred_format: Some(native_decoder::YuvPixFmt::Nv12),
        zero_copy: false, // Phase 1 only
    };

    match create_decoder(path, config) {
        Ok(mut decoder) => {
            match decoder.decode_frame(t_sec) {
                Ok(Some(video_frame)) => {
                    // Convert YUV to RGBA for egui::ColorImage
                    let rgba = yuv_to_rgba(
                        &video_frame.y_plane,
                        &video_frame.uv_plane,
                        video_frame.width,
                        video_frame.height,
                        video_frame.format,
                    );

                    // Scale to requested size if needed
                    if video_frame.width == w && video_frame.height == h {
                        Some(egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &rgba,
                        ))
                    } else {
                        // Simple nearest-neighbor scaling for now
                        let scaled =
                            scale_rgba_nearest(&rgba, video_frame.width, video_frame.height, w, h);
                        Some(egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &scaled,
                        ))
                    }
                }
                Ok(None) => {
                    eprintln!("Native decoder: No frame at timestamp {:.3}s", t_sec);
                    None
                }
                Err(e) => {
                    eprintln!("Native decoder error: {}", e);
                    None
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to create native decoder: {}", e);
            None
        }
    }
}

// Convert YUV to RGBA (simple implementation)
fn yuv_to_rgba(
    y_plane: &[u8],
    uv_plane: &[u8],
    width: u32,
    height: u32,
    format: native_decoder::YuvPixFmt,
) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    match format {
        native_decoder::YuvPixFmt::Nv12 => {
            // NV12: Y plane + interleaved UV plane
            for y in 0..height as usize {
                for x in 0..width as usize {
                    let y_idx = y * width as usize + x;
                    let uv_idx = (y / 2) * width as usize + (x / 2) * 2;

                    let y_val = y_plane[y_idx] as f32;
                    let u_val = uv_plane[uv_idx] as f32 - 128.0;
                    let v_val = uv_plane[uv_idx + 1] as f32 - 128.0;

                    // YUV to RGB conversion (ITU-R BT.601)
                    let r = (y_val + 1.402 * v_val).clamp(0.0, 255.0) as u8;
                    let g = (y_val - 0.344136 * u_val - 0.714136 * v_val).clamp(0.0, 255.0) as u8;
                    let b = (y_val + 1.772 * u_val).clamp(0.0, 255.0) as u8;

                    let rgba_idx = (y * width as usize + x) * 4;
                    rgba[rgba_idx] = r;
                    rgba[rgba_idx + 1] = g;
                    rgba[rgba_idx + 2] = b;
                    rgba[rgba_idx + 3] = 255; // Alpha
                }
            }
        }
        native_decoder::YuvPixFmt::P010 => {
            // P010: 10-bit YUV (simplified to 8-bit for now)
            for y in 0..height as usize {
                for x in 0..width as usize {
                    let y_idx = y * width as usize + x;
                    let uv_idx = (y / 2) * width as usize + (x / 2) * 2;

                    // Convert 10-bit to 8-bit (shift right by 2)
                    let y_val = (y_plane[y_idx] as f32) * 4.0;
                    let u_val = (uv_plane[uv_idx] as f32) * 4.0 - 128.0;
                    let v_val = (uv_plane[uv_idx + 1] as f32) * 4.0 - 128.0;

                    // YUV to RGB conversion
                    let r = (y_val + 1.402 * v_val).clamp(0.0, 255.0) as u8;
                    let g = (y_val - 0.344136 * u_val - 0.714136 * v_val).clamp(0.0, 255.0) as u8;
                    let b = (y_val + 1.772 * u_val).clamp(0.0, 255.0) as u8;

                    let rgba_idx = (y * width as usize + x) * 4;
                    rgba[rgba_idx] = r;
                    rgba[rgba_idx + 1] = g;
                    rgba[rgba_idx + 2] = b;
                    rgba[rgba_idx + 3] = 255; // Alpha
                }
            }
        }
    }

    rgba
}

// Simple nearest-neighbor scaling
fn scale_rgba_nearest(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w * dst_h * 4) as usize];

    for y in 0..dst_h as usize {
        for x in 0..dst_w as usize {
            let src_x = (x as f32 * src_w as f32 / dst_w as f32) as usize;
            let src_y = (y as f32 * src_h as f32 / dst_h as f32) as usize;

            let src_idx = (src_y * src_w as usize + src_x) * 4;
            let dst_idx = (y * dst_w as usize + x) * 4;

            if src_idx + 3 < src.len() && dst_idx + 3 < dst.len() {
                dst[dst_idx] = src[src_idx];
                dst[dst_idx + 1] = src[src_idx + 1];
                dst[dst_idx + 2] = src[src_idx + 2];
                dst[dst_idx + 3] = src[src_idx + 3];
            }
        }
    }

    dst
}

// Decode a single frame to NV12 or P010 at native size.
fn decode_video_frame_yuv(
    path: &str,
    t_sec: f64,
) -> Option<(YuvPixFmt, Vec<u8>, Vec<u8>, u32, u32)> {
    let info = media_io::probe_media(std::path::Path::new(path)).ok()?;
    let w = info.width?;
    let h = info.height?;
    // Try P010 first
    let out10 = std::process::Command::new("ffmpeg")
        .arg("-ss")
        .arg(format!("{:.3}", t_sec.max(0.0)))
        .arg("-i")
        .arg(path)
        .arg("-frames:v")
        .arg("1")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("p010le")
        .arg("-threads")
        .arg("1")
        .arg("-")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if out10.status.success() {
        let exp10 = (w as usize) * (h as usize) * 3; // Y:2 bytes * w*h ; UV: w*h bytes (2x16-bit at half res)
        if out10.stdout.len() >= exp10 {
            let y_bytes = (w as usize) * (h as usize) * 2;
            let y = out10.stdout[..y_bytes].to_vec();
            let uv = out10.stdout[y_bytes..y_bytes + (exp10 - y_bytes)].to_vec();
            return Some((YuvPixFmt::P010, y, uv, w, h));
        }
    }
    // Fallback NV12
    let expected = (w as usize) * (h as usize) + (w as usize) * (h as usize) / 2;
    let out = std::process::Command::new("ffmpeg")
        .arg("-ss")
        .arg(format!("{:.3}", t_sec.max(0.0)))
        .arg("-i")
        .arg(path)
        .arg("-frames:v")
        .arg("1")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("nv12")
        .arg("-threads")
        .arg("1")
        .arg("-")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() < expected {
        return None;
    }
    let y_size = (w as usize) * (h as usize);
    let y = out.stdout[..y_size].to_vec();
    let uv = out.stdout[y_size..y_size + (expected - y_size)].to_vec();
    Some((YuvPixFmt::Nv12, y, uv, w, h))
}

fn decode_video_frame_nv12_only(
    path: &str,
    t_sec: f64,
) -> Option<(YuvPixFmt, Vec<u8>, Vec<u8>, u32, u32)> {
    let info = media_io::probe_media(std::path::Path::new(path)).ok()?;
    let w = info.width?;
    let h = info.height?;
    let expected = (w as usize) * (h as usize) + (w as usize) * (h as usize) / 2;
    let out = std::process::Command::new("ffmpeg")
        .arg("-ss")
        .arg(format!("{:.3}", t_sec.max(0.0)))
        .arg("-i")
        .arg(path)
        .arg("-frames:v")
        .arg("1")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("nv12")
        .arg("-threads")
        .arg("1")
        .arg("-")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() < expected {
        return None;
    }
    let y_size = (w as usize) * (h as usize);
    let y = out.stdout[..y_size].to_vec();
    let uv = out.stdout[y_size..y_size + (expected - y_size)].to_vec();
    Some((YuvPixFmt::Nv12, y, uv, w, h))
}

pub(crate) fn device_supports_16bit_norm(gpu: &GpuContext<'_>) -> bool {
    gpu.with_device(|device| {
        device
            .features()
            .contains(eframe::wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
    })
}

pub(crate) fn upload_plane(
    queue: &eframe::wgpu::Queue,
    texture: &eframe::wgpu::Texture,
    src: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    bytes_per_pixel: usize,
) {
    if width == 0 || height == 0 {
        return;
    }
    let required = (width as usize) * bytes_per_pixel;
    assert!(stride >= required, "stride too small for upload");
    assert!(
        src.len() >= stride * height as usize,
        "plane buffer too small"
    );

    let align = eframe::wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
    let padded = ((required + align - 1) / align) * align;

    if stride == padded {
        queue.write_texture(
            eframe::wgpu::ImageCopyTexture {
                texture,
                mip_level: 0,
                origin: eframe::wgpu::Origin3d::ZERO,
                aspect: eframe::wgpu::TextureAspect::All,
            },
            src,
            eframe::wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded as u32),
                rows_per_image: Some(height),
            },
            eframe::wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    } else {
        let mut repacked = vec![0u8; padded * height as usize];
        for row in 0..height as usize {
            let src_off = row * stride;
            let dst_off = row * padded;
            repacked[dst_off..dst_off + required]
                .copy_from_slice(&src[src_off..src_off + required]);
        }
        queue.write_texture(
            eframe::wgpu::ImageCopyTexture {
                texture,
                mip_level: 0,
                origin: eframe::wgpu::Origin3d::ZERO,
                aspect: eframe::wgpu::TextureAspect::All,
            },
            &repacked,
            eframe::wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded as u32),
                rows_per_image: Some(height),
            },
            eframe::wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }
}

#[derive(Clone)]
struct Nv12Frame {
    fmt: YuvPixFmt,
    y: Vec<u8>,
    uv: Vec<u8>,
    w: u32,
    h: u32,
}

// Using media_io::YuvPixFmt

// Optimized image decoding
fn decode_image_optimized(path: &str, w: u32, h: u32) -> Option<egui::ColorImage> {
    // For images, use the image crate directly for better performance
    let img = image::open(path).ok()?;
    let resized = img.resize(w, h, image::imageops::FilterType::Lanczos3);
    let rgba = resized.to_rgba8();
    let (width, height) = rgba.dimensions();

    Some(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &rgba.into_raw(),
    ))
}

// LRU eviction for frame cache
fn evict_lru_frames(cache: &mut HashMap<FrameCacheKey, CachedFrame>, count: usize) {
    if cache.len() <= count {
        return;
    }

    // Collect frames with their last access times
    let mut frames_with_time: Vec<(FrameCacheKey, std::time::Instant)> = cache
        .iter()
        .map(|(key, frame)| (key.clone(), frame.last_access))
        .collect();

    // Sort by last access time (oldest first)
    frames_with_time.sort_by_key(|(_, time)| *time);

    // Remove the oldest frames
    for (key, _) in frames_with_time.into_iter().take(count) {
        cache.remove(&key);
    }
}

fn grab_frame_at(path: &str, size: (u32, u32), t_sec: f64) -> Option<egui::ColorImage> {
    let (w, h) = size;
    decode_video_frame_optimized(path, t_sec, w, h)
}

// Efficient frame cache key
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FrameCacheKey {
    pub(crate) path: String,
    pub(crate) time_sec: u32, // Rounded to nearest 0.1 second for cache efficiency
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl FrameCacheKey {
    pub(crate) fn new(path: &str, time_sec: f64, width: u32, height: u32) -> Self {
        Self {
            path: path.to_string(),
            time_sec: (time_sec * 10.0).round() as u32, // 0.1 second precision
            width,
            height,
        }
    }
}

// Cached frame with metadata
#[derive(Clone)]
struct CachedFrame {
    pub(crate) image: egui::ColorImage,
    pub(crate) decoded_at: std::time::Instant,
    pub(crate) access_count: u32,
    pub(crate) last_access: std::time::Instant,
}

// Frame buffer used by the preview scheduler (kept for compatibility)
struct FrameBuffer {
    pub(crate) pts: f64,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) bytes: Vec<u8>,
}

// (removed legacy standalone WGPU context to avoid mixed versions)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewReadbackKind {
    Renderer,
    Legacy,
}

struct PreviewReadbackRequest<'a> {
    tag: &'a ReadbackTag,
    auto: bool,
    phase: PlaybackPhase,
    source: &'a ReadbackSource,
    original_width: u32,
    original_height: u32,
    target_width: u32,
    target_height: u32,
    bytes_per_row: u32,
    allow_blocking: bool,
}

#[derive(Debug)]
enum ReadbackSubmission {
    Submitted,
    Completed(ReadbackResult),
    Pending,
}

#[derive(Debug, Default)]
struct ReadbackPoll {
    forced_wait: bool,
    had_pending: bool,
}

#[derive(Debug)]
enum PreviewReadbackError {
    Unsupported { reason: String },
    SubmissionFailed { reason: String },
}

impl PreviewReadbackError {
    fn unsupported(reason: impl Into<String>) -> Self {
        PreviewReadbackError::Unsupported {
            reason: reason.into(),
        }
    }

    fn failed(reason: impl Into<String>) -> Self {
        PreviewReadbackError::SubmissionFailed {
            reason: reason.into(),
        }
    }

    fn is_permanent(&self) -> bool {
        matches!(self, PreviewReadbackError::Unsupported { .. })
    }
}

trait PreviewReadback: Send {
    fn kind(&self) -> PreviewReadbackKind;
    fn name(&self) -> &'static str;
    fn has_pending(&self) -> bool;
    fn readback(
        &mut self,
        gpu: &GpuContext<'_>,
        request: &PreviewReadbackRequest<'_>,
    ) -> Result<ReadbackSubmission, PreviewReadbackError>;
    fn poll(&mut self, gpu_sync: &GpuSyncController, phase: PlaybackPhase) -> ReadbackPoll;
    fn try_recv(&mut self) -> Option<ReadbackResult>;
    fn clear_for_realtime(&mut self);
}

struct ReadbackManagerBackend {
    ring: usize,
    manager: Option<ReadbackManager>,
    downscale: Option<ReadbackDownscale>,
    resolve: Option<ReadbackResolve>,
}

impl ReadbackManagerBackend {
    fn new(ring: usize) -> Self {
        Self {
            ring,
            manager: None,
            downscale: None,
            resolve: None,
        }
    }

    fn ensure_manager(&mut self, gpu: &GpuContext<'_>) -> &mut ReadbackManager {
        if self.manager.is_none() {
            let ring = self.ring;
            let created = gpu.with_device(|device| ReadbackManager::new(device, ring));
            self.manager = Some(created);
        }
        self.manager.as_mut().expect("manager present")
    }

    fn ensure_downscale_pipeline(&mut self, gpu: &GpuContext<'_>) -> &ReadbackDownscale {
        if self.downscale.is_none() {
            let pipeline = gpu.with_device(|device| ReadbackDownscale::new(device));
            self.downscale = Some(pipeline);
        }
        self.downscale.as_ref().expect("downscale pipeline")
    }

    fn ensure_resolve_texture(
        &mut self,
        gpu: &GpuContext<'_>,
        width: u32,
        height: u32,
    ) -> &ReadbackResolve {
        let recreate = self
            .resolve
            .as_ref()
            .map(|existing| existing.width != width || existing.height != height)
            .unwrap_or(true);
        if recreate {
            let texture = gpu.with_device(|device| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("readback-resolve"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                })
            });
            self.resolve = Some(ReadbackResolve {
                texture: Arc::new(texture),
                width,
                height,
            });
        }
        self.resolve.as_ref().expect("resolve texture")
    }
}

impl PreviewReadback for ReadbackManagerBackend {
    fn kind(&self) -> PreviewReadbackKind {
        PreviewReadbackKind::Legacy
    }

    fn name(&self) -> &'static str {
        "legacy_readback"
    }

    fn has_pending(&self) -> bool {
        self.manager
            .as_ref()
            .map(|manager| manager.has_pending())
            .unwrap_or(false)
    }

    fn readback(
        &mut self,
        gpu: &GpuContext<'_>,
        request: &PreviewReadbackRequest<'_>,
    ) -> Result<ReadbackSubmission, PreviewReadbackError> {
        self.ensure_manager(gpu);

        if !{
            let manager = self.manager.as_mut().expect("manager present after ensure");
            manager.phase_policy_allows(request.phase)
        } {
            return Ok(ReadbackSubmission::Pending);
        }

        let mut encoder = gpu.with_device(|device| {
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("readback-encoder"),
            })
        });

        let mut src_texture: Arc<wgpu::Texture> = Arc::clone(&request.source.rgba_texture);
        if request.target_width != request.original_width
            || request.target_height != request.original_height
        {
            let resolve_texture = {
                let resolve =
                    self.ensure_resolve_texture(gpu, request.target_width, request.target_height);
                Arc::clone(&resolve.texture)
            };
            let downscale = self.ensure_downscale_pipeline(gpu);

            let resolve_view = resolve_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let src_view = request
                .source
                .rgba_texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            let bind_group = gpu.with_device(|device| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("readback-downscale-bg"),
                    layout: &downscale.bind_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&src_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&downscale.sampler),
                        },
                    ],
                })
            });

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("readback-downscale"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &resolve_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pass.set_pipeline(&downscale.pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.draw(0..3, 0..1);
            }

            src_texture = resolve_texture;
        }

        let extent = wgpu::Extent3d {
            width: request.target_width,
            height: request.target_height,
            depth_or_array_layers: 1,
        };

        let image_src = wgpu::ImageCopyTexture {
            texture: &*src_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        };

        let readback_request = ReadbackRequest {
            extent,
            bytes_per_row: request.bytes_per_row,
            src: image_src,
            tag: request.tag.clone(),
        };

        let Some(slot_idx) = ({
            let manager_ref = self.manager.as_mut().expect("manager present after ensure");
            gpu.with_device(|device| {
                manager_ref.request_copy(device, &mut encoder, readback_request)
            })
        }) else {
            return Ok(ReadbackSubmission::Pending);
        };

        info!(
            target = "preview_readback",
            backend = "legacy",
            tag = ?request.tag,
            width = request.target_width,
            height = request.target_height,
            slot = slot_idx,
            "queued legacy preview readback"
        );

        gpu.gpu_sync().notify_work_submitted();
        gpu.with_queue(|queue| queue.submit(std::iter::once(encoder.finish())));
        crate::gpu_pump!(gpu, "readback_issue_submit");
        if let Some(manager) = self.manager.as_mut() {
            manager.schedule_map(slot_idx);
            manager.mark_enqueued(request.phase);
        }

        Ok(ReadbackSubmission::Submitted)
    }

    fn poll(&mut self, gpu_sync: &GpuSyncController, _phase: PlaybackPhase) -> ReadbackPoll {
        let mut poll = ReadbackPoll {
            forced_wait: false,
            had_pending: self.has_pending(),
        };

        if let Some(manager) = self.manager.as_mut() {
            let wait = manager.poll(gpu_sync);
            if wait {
                poll.forced_wait = true;
            }
        }

        poll
    }

    fn try_recv(&mut self) -> Option<ReadbackResult> {
        self.manager.as_mut()?.try_recv()
    }

    fn clear_for_realtime(&mut self) {
        self.manager = None;
        self.downscale = None;
        self.resolve = None;
    }
}

struct RendererReadbackBackend {
    resources: Option<PreviewReadbackResources>,
    failure_count: u32,
    disabled: bool,
}

impl RendererReadbackBackend {
    fn new(_options: RendererBackendOptions) -> Self {
        Self {
            resources: None,
            failure_count: 0,
            disabled: false,
        }
    }

    fn ensure_resources(
        &mut self,
        gpu: &GpuContext<'_>,
    ) -> Result<&mut PreviewReadbackResources, renderer::RendererError> {
        if self.resources.is_none() {
            let created = gpu.with_device(|device| PreviewReadbackResources::new(device));
            self.resources = Some(created?);
        }
        self.resources.as_mut().ok_or_else(|| {
            renderer::RendererError::InvalidFormat("failed to create renderer resources".into())
        })
    }

    fn handle_error(&mut self, err: renderer::RendererError) -> PreviewReadbackError {
        match err {
            renderer::RendererError::InvalidFormat(reason) => {
                self.disabled = true;
                warn!(target = "preview_readback", reason = %reason, "renderer readback disabled due to invalid format");
                PreviewReadbackError::unsupported(reason)
            }
            other => {
                self.failure_count += 1;
                error!(target = "preview_readback", error = %other, failures = self.failure_count, "renderer readback submission failed");
                if self.failure_count >= 3 {
                    self.disabled = true;
                    PreviewReadbackError::unsupported(format!(
                        "renderer failed after {} attempts",
                        self.failure_count
                    ))
                } else {
                    PreviewReadbackError::failed(other.to_string())
                }
            }
        }
    }
}

impl PreviewReadback for RendererReadbackBackend {
    fn kind(&self) -> PreviewReadbackKind {
        PreviewReadbackKind::Renderer
    }

    fn name(&self) -> &'static str {
        "renderer_readback"
    }

    fn has_pending(&self) -> bool {
        false
    }

    fn readback(
        &mut self,
        gpu: &GpuContext<'_>,
        request: &PreviewReadbackRequest<'_>,
    ) -> Result<ReadbackSubmission, PreviewReadbackError> {
        if self.disabled {
            return Err(PreviewReadbackError::unsupported(
                "renderer backend disabled",
            ));
        }

        let resources = match self.ensure_resources(gpu) {
            Ok(res) => res,
            Err(err) => return Err(self.handle_error(err)),
        };

        let rgba_view = request
            .source
            .rgba_texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let y_view = request
            .source
            .y_plane
            .as_ref()
            .map(|tex| tex.create_view(&wgpu::TextureViewDescriptor::default()));
        let uv_view = request
            .source
            .uv_plane
            .as_ref()
            .map(|tex| tex.create_view(&wgpu::TextureViewDescriptor::default()));

        let (pixel_format, textures) = if let (Some(fmt), Some(y), Some(uv)) =
            (request.source.format, y_view.as_ref(), uv_view.as_ref())
        {
            match fmt {
                YuvPixFmt::Nv12 => (
                    RendererPixelFormat::Nv12,
                    PreviewTextureSource::Nv12 {
                        y_plane: y,
                        uv_plane: uv,
                    },
                ),
                YuvPixFmt::P010 => (
                    RendererPixelFormat::P010,
                    PreviewTextureSource::P010 {
                        y_plane: y,
                        uv_plane: uv,
                    },
                ),
            }
        } else {
            (
                RendererPixelFormat::Rgba8,
                PreviewTextureSource::Rgba {
                    texture: &rgba_view,
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                },
            )
        };

        let downscale = if request.target_width != request.original_width
            || request.target_height != request.original_height
        {
            Some(PreviewDownscale {
                width: request.target_width,
                height: request.target_height,
            })
        } else {
            None
        };

        let color_space = match pixel_format {
            RendererPixelFormat::Rgba8 => RendererColorSpace::Srgb,
            _ => RendererColorSpace::Rec709,
        };

        let input = PreviewFrameInput {
            width: request.original_width,
            height: request.original_height,
            color_space,
            pixel_format,
            textures,
            downscale,
            gpu_sync: Some(gpu.gpu_sync()),
        };

        let frame = gpu
            .with_device(|device| {
                gpu.with_queue(|queue| {
                    resources.render_to_cpu(device, queue, &input, |reason| {
                        trace!(target = "preview_renderer_readback", reason);
                        if request.allow_blocking {
                            gpu.gpu_sync().service_gpu("renderer_readback_wait");
                        } else {
                            gpu.gpu_sync().poll_nonblocking();
                        }
                    })
                })
            })
            .map_err(|err| self.handle_error(err))?;

        let aligned_bytes = align_bytes_per_row(request.target_width) as usize;
        let row_bytes = frame.bytes_per_row as usize;
        let pixels = if aligned_bytes == row_bytes {
            frame.pixels
        } else {
            let mut padded = vec![0u8; aligned_bytes * (request.target_height as usize)];
            for row in 0..(request.target_height as usize) {
                let src_off = row * row_bytes;
                let dst_off = row * aligned_bytes;
                padded[dst_off..dst_off + row_bytes]
                    .copy_from_slice(&frame.pixels[src_off..src_off + row_bytes]);
            }
            padded
        };

        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default();

        let result = ReadbackResult {
            tag: request.tag.clone(),
            pixels,
            extent: wgpu::Extent3d {
                width: request.target_width,
                height: request.target_height,
                depth_or_array_layers: 1,
            },
            bytes_per_row: align_bytes_per_row(request.target_width),
            timestamp_ns,
        };

        self.failure_count = 0;
        Ok(ReadbackSubmission::Completed(result))
    }

    fn poll(&mut self, _gpu_sync: &GpuSyncController, _phase: PlaybackPhase) -> ReadbackPoll {
        ReadbackPoll {
            forced_wait: false,
            had_pending: false,
        }
    }

    fn try_recv(&mut self) -> Option<ReadbackResult> {
        None
    }

    fn clear_for_realtime(&mut self) {
        self.failure_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_renderer_options() -> RendererBackendOptions {
        let settings = PreviewReadbackSettings {
            ring: 4,
            scale: 1.0,
            auto_interval: None,
        };
        RendererBackendOptions::new(settings, Duration::from_millis(100))
    }

    #[test]
    fn renderer_backend_disables_on_invalid_format() {
        let mut backend = RendererReadbackBackend::new(test_renderer_options());
        let result =
            backend.handle_error(renderer::RendererError::InvalidFormat("bad format".into()));
        assert!(backend.disabled);
        assert!(matches!(result, PreviewReadbackError::Unsupported { .. }));
    }

    #[test]
    fn renderer_backend_requires_multiple_failures_before_disable() {
        let mut backend = RendererReadbackBackend::new(test_renderer_options());
        for _ in 0..2 {
            let result = backend.handle_error(renderer::RendererError::ShaderCompilation(
                "shaders failed".into(),
            ));
            assert!(matches!(
                result,
                PreviewReadbackError::SubmissionFailed { .. }
            ));
            assert!(!backend.disabled);
        }

        let result = backend.handle_error(renderer::RendererError::ShaderCompilation(
            "shaders failed again".into(),
        ));
        assert!(backend.disabled);
        assert!(matches!(result, PreviewReadbackError::Unsupported { .. }));
    }
}
