use std::sync::mpsc::{channel, TryRecvError};
use std::time::Duration;

use tracing::{info_span, instrument};
use wgpu::util::DeviceExt;

use crate::{ColorSpace, PixelFormat, RendererError};

const COPY_ALIGNMENT: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Trait implemented by GPU-sync helpers (e.g. `GpuSyncController`) so the renderer can
/// notify callers whenever new work has been submitted to the device queue.
pub trait PreviewGpuSync {
    fn notify_work_submitted(&self);
}

/// Describes the GPU textures backing the preview frame that should be sampled.
#[derive(Debug, Clone, Copy)]
pub enum PreviewTextureSource<'a> {
    Nv12 {
        y_plane: &'a wgpu::TextureView,
        uv_plane: &'a wgpu::TextureView,
    },
    P010 {
        y_plane: &'a wgpu::TextureView,
        uv_plane: &'a wgpu::TextureView,
    },
    Rgba {
        texture: &'a wgpu::TextureView,
        format: wgpu::TextureFormat,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct PreviewDownscale {
    pub width: u32,
    pub height: u32,
}

/// Metadata describing a preview frame readback request.
#[derive(Clone, Copy)]
pub struct PreviewFrameInput<'a> {
    pub width: u32,
    pub height: u32,
    pub color_space: ColorSpace,
    pub pixel_format: PixelFormat,
    pub textures: PreviewTextureSource<'a>,
    pub downscale: Option<PreviewDownscale>,
    pub gpu_sync: Option<&'a dyn PreviewGpuSync>,
}

impl<'a> PreviewFrameInput<'a> {
    pub fn output_size(&self) -> (u32, u32) {
        self.downscale
            .map(|d| (d.width, d.height))
            .unwrap_or((self.width, self.height))
    }

    fn validate(&self) -> Result<(), RendererError> {
        if self.width == 0 || self.height == 0 {
            return Err(RendererError::InvalidFormat(
                "preview frame input width/height must be non-zero".into(),
            ));
        }

        if let Some(scale) = self.downscale {
            if scale.width == 0 || scale.height == 0 {
                return Err(RendererError::InvalidFormat(
                    "downscale width/height must be non-zero".into(),
                ));
            }
        }

        match (&self.textures, self.pixel_format) {
            (PreviewTextureSource::Nv12 { .. }, PixelFormat::Nv12) => {}
            (PreviewTextureSource::P010 { .. }, PixelFormat::P010) => {}
            (PreviewTextureSource::Rgba { format, .. }, PixelFormat::Rgba8) => match format {
                wgpu::TextureFormat::Rgba8Unorm
                | wgpu::TextureFormat::Rgba8UnormSrgb
                | wgpu::TextureFormat::Rgba8Uint
                | wgpu::TextureFormat::Rgba8Sint => {}
                other => {
                    return Err(RendererError::InvalidFormat(format!(
                        "unsupported RGBA texture format: {other:?}"
                    )));
                }
            },
            _ => {
                return Err(RendererError::InvalidFormat(format!(
                    "pixel format {:?} does not match provided textures",
                    self.pixel_format
                )));
            }
        }

        match self.color_space {
            ColorSpace::Rec709 | ColorSpace::Srgb => Ok(()),
            other => Err(RendererError::InvalidFormat(format!(
                "unsupported color space: {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CpuPixelFormat {
    Rgba8,
}

#[derive(Debug, Clone)]
pub struct CpuFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: u32,
    pub format: CpuPixelFormat,
    pub color_space: ColorSpace,
}

struct PreviewRenderCache {
    render_texture: wgpu::Texture,
    render_view: wgpu::TextureView,
    staging_buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    row_pitch: u32,
    buffer_size: u64,
}

pub struct PreviewReadbackResources {
    pipelines: PreviewReadbackPipelines,
    cache: Option<PreviewRenderCache>,
    supports_p010: bool,
}

impl PreviewReadbackResources {
    pub fn new(device: &wgpu::Device) -> Result<Self, RendererError> {
        Ok(Self {
            pipelines: PreviewReadbackPipelines::new(device)?,
            cache: None,
            supports_p010: device
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM),
        })
    }

    #[instrument(
        name = "preview_readback.render",
        skip_all,
        fields(width = input.width, height = input.height)
    )]
    pub fn render_to_cpu(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        input: &PreviewFrameInput<'_>,
        pump: impl FnMut(&'static str),
    ) -> Result<CpuFrame, RendererError> {
        input.validate()?;
        let (target_width, target_height) = input.output_size();

        if target_width == 0 || target_height == 0 {
            return Err(RendererError::InvalidFormat(
                "render target width/height must be non-zero".into(),
            ));
        }

        self.ensure_cache(device, target_width, target_height)?;
        let cache = self
            .cache
            .as_ref()
            .expect("preview cache populated after ensure_cache");

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("preview-readback.encoder"),
        });

        let (pipeline, bind_group) = match &input.textures {
            PreviewTextureSource::Nv12 { y_plane, uv_plane } => {
                let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("preview-readback.nv12.bind_group"),
                    layout: &self.pipelines.nv12_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(y_plane),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(uv_plane),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&self.pipelines.sampler),
                        },
                    ],
                });
                (&self.pipelines.nv12_pipeline, bind_group)
            }
            PreviewTextureSource::P010 { y_plane, uv_plane } => {
                if !self.supports_p010 {
                    return Err(RendererError::InvalidFormat(
                        "P010 preview readback requires device feature TEXTURE_FORMAT_16BIT_NORM"
                            .into(),
                    ));
                }
                let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("preview-readback.p010.bind_group"),
                    layout: &self.pipelines.nv12_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(y_plane),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(uv_plane),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&self.pipelines.sampler),
                        },
                    ],
                });
                (&self.pipelines.p010_pipeline, bind_group)
            }
            PreviewTextureSource::Rgba { texture, .. } => {
                let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("preview-readback.rgba.bind_group"),
                    layout: &self.pipelines.rgba_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(texture),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.pipelines.sampler),
                        },
                    ],
                });
                (&self.pipelines.rgba_pipeline, bind_group)
            }
        };

        {
            let span = info_span!(
                "preview_readback.draw",
                width = target_width,
                height = target_height,
                pixel_format = ?input.pixel_format
            );
            let _guard = span.enter();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("preview-readback.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &cache.render_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);

            pass.set_vertex_buffer(0, self.pipelines.vertex_buffer.slice(..));
            pass.set_index_buffer(
                self.pipelines.index_buffer.slice(..),
                wgpu::IndexFormat::Uint16,
            );
            pass.draw_indexed(0..6, 0, 0..1);
        }

        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &cache.render_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &cache.staging_buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(cache.row_pitch),
                    rows_per_image: Some(target_height),
                },
            },
            wgpu::Extent3d {
                width: target_width,
                height: target_height,
                depth_or_array_layers: 1,
            },
        );

        if let Some(sync) = input.gpu_sync {
            sync.notify_work_submitted();
        }

        queue.submit(std::iter::once(encoder.finish()));

        self.map_to_cpu(
            device,
            &cache.staging_buffer,
            cache.row_pitch,
            target_width,
            target_height,
            input.color_space,
            pump,
        )
    }

    fn ensure_cache(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Result<&PreviewRenderCache, RendererError> {
        let row_pitch = align_to(width * 4, COPY_ALIGNMENT);
        let buffer_size = row_pitch as u64 * height as u64;

        let recreate = match &self.cache {
            Some(cache) => {
                cache.width != width
                    || cache.height != height
                    || cache.row_pitch != row_pitch
                    || cache.buffer_size < buffer_size
            }
            None => true,
        };

        if recreate {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("preview-readback.render-target"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TARGET_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let staging_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("preview-readback.staging"),
                size: align_to_u64(buffer_size, 4),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });

            self.cache = Some(PreviewRenderCache {
                render_texture: texture,
                render_view: view,
                staging_buffer,
                width,
                height,
                row_pitch,
                buffer_size: align_to_u64(buffer_size, 4),
            });
        }

        Ok(self.cache.as_ref().expect("cache must be populated"))
    }

    fn map_to_cpu(
        &self,
        device: &wgpu::Device,
        staging: &wgpu::Buffer,
        row_pitch: u32,
        width: u32,
        height: u32,
        color_space: ColorSpace,
        mut pump: impl FnMut(&'static str),
    ) -> Result<CpuFrame, RendererError> {
        let slice = staging.slice(..row_pitch as u64 * height as u64);
        let (tx, rx) = channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });

        loop {
            match rx.try_recv() {
                Ok(Ok(())) => break,
                Ok(Err(_)) => return Err(RendererError::BufferAsync),
                Err(TryRecvError::Empty) => {
                    pump("preview.readback.map");
                    device.poll(wgpu::Maintain::Poll);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(TryRecvError::Disconnected) => {
                    return Err(RendererError::BufferAsync);
                }
            }
        }

        let mapped = slice.get_mapped_range();
        let row_stride = width as usize * 4;
        let mut pixels = vec![0u8; row_stride * height as usize];
        for row in 0..height as usize {
            let src_offset = row * row_pitch as usize;
            let dst_offset = row * row_stride;
            let row_slice = &mapped[src_offset..src_offset + row_stride];
            pixels[dst_offset..dst_offset + row_stride].copy_from_slice(row_slice);
        }
        drop(mapped);
        staging.unmap();

        Ok(CpuFrame {
            pixels,
            width,
            height,
            bytes_per_row: (width * 4),
            format: CpuPixelFormat::Rgba8,
            color_space,
        })
    }
}

struct PreviewReadbackPipelines {
    sampler: wgpu::Sampler,
    nv12_pipeline: wgpu::RenderPipeline,
    p010_pipeline: wgpu::RenderPipeline,
    rgba_pipeline: wgpu::RenderPipeline,
    nv12_layout: wgpu::BindGroupLayout,
    rgba_layout: wgpu::BindGroupLayout,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
}

impl PreviewReadbackPipelines {
    fn new(device: &wgpu::Device) -> Result<Self, RendererError> {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("preview-readback.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let nv12_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("preview-readback.nv12.shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "shaders/preview_nv12.wgsl"
            ))),
        });

        let p010_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("preview-readback.p010.shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "shaders/preview_p010.wgsl"
            ))),
        });

        let rgba_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("preview-readback.rgba.shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "shaders/preview_rgba.wgsl"
            ))),
        });

        let nv12_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("preview-readback.nv12.layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let rgba_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("preview-readback.rgba.layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
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

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("preview-readback.vertex-buffer"),
            contents: bytemuck::cast_slice(&FULLSCREEN_VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("preview-readback.index-buffer"),
            contents: bytemuck::cast_slice(&FULLSCREEN_INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });

        let nv12_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("preview-readback.nv12.pipeline-layout"),
            bind_group_layouts: &[&nv12_layout],
            push_constant_ranges: &[],
        });

        let nv12_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("preview-readback.nv12.pipeline"),
            layout: Some(&nv12_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &nv12_shader,
                entry_point: "vs_fullscreen",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[preview_vertex_layout()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &nv12_shader,
                entry_point: "fs_nv12",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TARGET_FORMAT,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let p010_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("preview-readback.p010.pipeline-layout"),
            bind_group_layouts: &[&nv12_layout],
            push_constant_ranges: &[],
        });

        let p010_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("preview-readback.p010.pipeline"),
            layout: Some(&p010_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &p010_shader,
                entry_point: "vs_fullscreen",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[preview_vertex_layout()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &p010_shader,
                entry_point: "fs_p010",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TARGET_FORMAT,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let rgba_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("preview-readback.rgba.pipeline-layout"),
            bind_group_layouts: &[&rgba_layout],
            push_constant_ranges: &[],
        });

        let rgba_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("preview-readback.rgba.pipeline"),
            layout: Some(&rgba_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &rgba_shader,
                entry_point: "vs_fullscreen",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[preview_vertex_layout()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &rgba_shader,
                entry_point: "fs_rgba",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TARGET_FORMAT,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            sampler,
            nv12_pipeline,
            p010_pipeline,
            rgba_pipeline,
            nv12_layout,
            rgba_layout,
            vertex_buffer,
            index_buffer,
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VertexPacked {
    position: [f32; 2],
    tex_coords: [f32; 2],
}

const FULLSCREEN_VERTICES: [VertexPacked; 4] = [
    VertexPacked {
        position: [-1.0, -1.0],
        tex_coords: [0.0, 1.0],
    },
    VertexPacked {
        position: [1.0, -1.0],
        tex_coords: [1.0, 1.0],
    },
    VertexPacked {
        position: [1.0, 1.0],
        tex_coords: [1.0, 0.0],
    },
    VertexPacked {
        position: [-1.0, 1.0],
        tex_coords: [0.0, 0.0],
    },
];

const FULLSCREEN_INDICES: [u16; 6] = [0, 1, 2, 2, 3, 0];

const PREVIEW_VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute {
        offset: 0,
        shader_location: 0,
        format: wgpu::VertexFormat::Float32x2,
    },
    wgpu::VertexAttribute {
        offset: std::mem::size_of::<[f32; 2]>() as u64,
        shader_location: 1,
        format: wgpu::VertexFormat::Float32x2,
    },
];

fn preview_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<VertexPacked>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &PREVIEW_VERTEX_ATTRIBUTES,
    }
}

fn align_to(value: u32, alignment: u32) -> u32 {
    if value == 0 {
        return alignment;
    }
    ((value + alignment - 1) / alignment) * alignment
}

fn align_to_u64(value: u64, alignment: u64) -> u64 {
    if alignment == 0 {
        return value;
    }
    ((value + alignment - 1) / alignment) * alignment
}

#[cfg(test)]
mod tests {
    use super::*;
    use pollster::FutureExt;

    fn init_device() -> (wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .block_on()
            .expect("adapter");

        let available_features = adapter.features();
        let requested_features = wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
        let features = available_features & requested_features;

        adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("preview-readback.test-device"),
                    required_features: features,
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .block_on()
            .expect("device")
    }

    #[test]
    fn nv12_to_cpu_outputs_rgba() {
        let (device, queue) = init_device();
        let mut resources = PreviewReadbackResources::new(&device).expect("resources");

        let width = 4;
        let height = 2;
        let y_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test.y"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let uv_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test.uv"),
            size: wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Solid gray: Y=128, U=128, V=128.
        let y_plane = vec![128u8; (width * height) as usize];
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &y_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &y_plane,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let uv_plane = vec![128u8; (width * height / 2) as usize];
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &uv_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &uv_plane,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height / 2),
            },
            wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
        );

        let input = PreviewFrameInput {
            width,
            height,
            color_space: ColorSpace::Rec709,
            pixel_format: PixelFormat::Nv12,
            textures: PreviewTextureSource::Nv12 {
                y_plane: &y_texture.create_view(&wgpu::TextureViewDescriptor::default()),
                uv_plane: &uv_texture.create_view(&wgpu::TextureViewDescriptor::default()),
            },
            downscale: None,
            gpu_sync: None,
        };

        let frame = resources
            .render_to_cpu(&device, &queue, &input, |_| {})
            .expect("cpu frame");

        assert_eq!(frame.width, width);
        assert_eq!(frame.height, height);
        assert_eq!(frame.pixels.len(), (width * height * 4) as usize);

        // Solid gray ~ 128 within tolerance.
        for chunk in frame.pixels.chunks_exact(4) {
            let r = chunk[0];
            let g = chunk[1];
            let b = chunk[2];
            assert!(
                (r as i32 - 128).abs() <= 2,
                "expected r near 128, got {}",
                r
            );
            assert!(
                (g as i32 - 128).abs() <= 2,
                "expected g near 128, got {}",
                g
            );
            assert!(
                (b as i32 - 128).abs() <= 2,
                "expected b near 128, got {}",
                b
            );
            assert_eq!(chunk[3], 255);
        }
    }

    #[test]
    fn rgba_texture_pass_through() {
        let (device, queue) = init_device();
        let mut resources = PreviewReadbackResources::new(&device).expect("resources");

        let width = 2;
        let height = 2;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test.rgba"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let pixels: [u8; 16] = [
            255, 0, 0, 255, // Red
            0, 255, 0, 255, // Green
            0, 0, 255, 255, // Blue
            255, 255, 255, 255, // White
        ];

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let input = PreviewFrameInput {
            width,
            height,
            color_space: ColorSpace::Srgb,
            pixel_format: PixelFormat::Rgba8,
            textures: PreviewTextureSource::Rgba {
                texture: &texture.create_view(&wgpu::TextureViewDescriptor::default()),
                format: wgpu::TextureFormat::Rgba8Unorm,
            },
            downscale: None,
            gpu_sync: None,
        };

        let frame = resources
            .render_to_cpu(&device, &queue, &input, |_| {})
            .expect("cpu frame");

        assert_eq!(frame.pixels, pixels);
    }

    #[test]
    fn p010_to_cpu_outputs_rgba() {
        let (device, queue) = init_device();
        if !device
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
        {
            eprintln!("skipping p010_to_cpu_outputs_rgba: device lacks 16-bit norm support");
            return;
        }
        let mut resources = PreviewReadbackResources::new(&device).expect("resources");

        let width = 2;
        let height = 2;

        let y_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test.p010.y"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R16Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let uv_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test.p010.uv"),
            size: wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg16Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let y_value = (512u16) << 6;
        let mut y_plane = Vec::with_capacity((width * height * 2) as usize);
        for _ in 0..(width * height) {
            y_plane.extend_from_slice(&y_value.to_le_bytes());
        }

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &y_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &y_plane,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(width * 2),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let uv_value = (512u16) << 6;
        let mut uv_plane = Vec::with_capacity((width * height) as usize);
        uv_plane.extend_from_slice(&uv_value.to_le_bytes());
        uv_plane.extend_from_slice(&uv_value.to_le_bytes());

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &uv_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &uv_plane,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(height / 2),
            },
            wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
        );

        let input = PreviewFrameInput {
            width,
            height,
            color_space: ColorSpace::Rec709,
            pixel_format: PixelFormat::P010,
            textures: PreviewTextureSource::P010 {
                y_plane: &y_texture.create_view(&wgpu::TextureViewDescriptor::default()),
                uv_plane: &uv_texture.create_view(&wgpu::TextureViewDescriptor::default()),
            },
            downscale: None,
            gpu_sync: None,
        };

        let frame = resources
            .render_to_cpu(&device, &queue, &input, |_| {})
            .expect("cpu frame");

        assert_eq!(frame.width, width);
        assert_eq!(frame.height, height);

        for chunk in frame.pixels.chunks_exact(4) {
            let [r, g, b, a] = [chunk[0], chunk[1], chunk[2], chunk[3]];
            assert!(r.abs_diff(128) <= 4);
            assert!(g.abs_diff(128) <= 4);
            assert!(b.abs_diff(128) <= 4);
            assert_eq!(a, 255);
        }
    }
}
