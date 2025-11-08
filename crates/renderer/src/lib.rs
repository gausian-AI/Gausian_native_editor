use anyhow::Result;
use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use thiserror::Error;
use wgpu::util::DeviceExt;

mod cpu;
mod preview;

pub use cpu::convert_yuv_to_rgba;
pub use preview::{
    CpuFrame, CpuPixelFormat, PreviewDownscale, PreviewFrameInput, PreviewGpuSync,
    PreviewReadbackResources, PreviewTextureSource,
};

#[derive(Debug, Error)]
pub enum RendererError {
    #[error("WGPU error: {0}")]
    Wgpu(#[from] wgpu::Error),
    #[error("Surface error: {0}")]
    Surface(#[from] wgpu::SurfaceError),
    #[error("Request device error: {0}")]
    RequestDevice(#[from] wgpu::RequestDeviceError),
    #[error("Buffer async error")]
    BufferAsync,
    #[error("Invalid format: {0}")]
    InvalidFormat(String),
    #[error("Shader compilation error: {0}")]
    ShaderCompilation(String),
}

/// GPU-accelerated renderer for video compositing
pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<wgpu::SurfaceConfiguration>,

    // Render pipelines
    yuv_to_rgb_pipeline: Option<wgpu::RenderPipeline>,
    scale_pipeline: Option<wgpu::RenderPipeline>,
    blend_pipeline: Option<wgpu::RenderPipeline>,
    transform_pipeline: Option<wgpu::RenderPipeline>,

    // Bind group layouts
    texture_bind_group_layout: wgpu::BindGroupLayout,
    uniform_bind_group_layout: wgpu::BindGroupLayout,

    // Vertex buffer for full-screen quad
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,

    preview_readback: PreviewReadbackResources,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    tex_coords: [f32; 2],
}

const VERTICES: &[Vertex] = &[
    Vertex {
        position: [-1.0, -1.0],
        tex_coords: [0.0, 1.0],
    },
    Vertex {
        position: [1.0, -1.0],
        tex_coords: [1.0, 1.0],
    },
    Vertex {
        position: [1.0, 1.0],
        tex_coords: [1.0, 0.0],
    },
    Vertex {
        position: [-1.0, 1.0],
        tex_coords: [0.0, 0.0],
    },
];

const INDICES: &[u16] = &[0, 1, 2, 2, 3, 0];

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct TransformUniforms {
    pub matrix: [[f32; 4]; 4],
    pub opacity: f32,
    pub _padding: [f32; 3],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct ColorUniforms {
    pub color_matrix: [[f32; 4]; 4],
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub hue: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderParams {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub color_space: ColorSpace,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba8,
    Bgra8,
    Rgb8,
    Yuv420p,
    Yuv422p,
    Yuv444p,
    Nv12,
    P010,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ColorSpace {
    Srgb,
    Rec709,
    Rec2020,
    DciP3,
}

impl Renderer {
    pub async fn new(surface: Option<wgpu::Surface<'static>>) -> Result<Self> {
        let instance = wgpu::Instance::default();

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: surface.as_ref(),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("No suitable adapter found"))?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("Renderer Device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await?;

        // Create bind group layouts
        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
                label: Some("texture_bind_group_layout"),
            });

        let uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
                label: Some("uniform_bind_group_layout"),
            });

        // Create vertex and index buffers
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: bytemuck::cast_slice(VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Index Buffer"),
            contents: bytemuck::cast_slice(INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });

        let preview_readback = preview::PreviewReadbackResources::new(&device)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        let renderer = Self {
            device,
            queue,
            surface,
            surface_config: None,
            yuv_to_rgb_pipeline: None,
            scale_pipeline: None,
            blend_pipeline: None,
            transform_pipeline: None,
            texture_bind_group_layout,
            uniform_bind_group_layout,
            vertex_buffer,
            index_buffer,
            preview_readback,
        };

        // We'll initialize pipelines later when surface is configured

        Ok(renderer)
    }

    pub fn render_preview_to_cpu_with_pump(
        &mut self,
        input: &PreviewFrameInput<'_>,
        pump: impl FnMut(&'static str),
    ) -> Result<CpuFrame, RendererError> {
        self.preview_readback
            .render_to_cpu(&self.device, &self.queue, input, pump)
    }

    pub fn configure_surface(
        &mut self,
        width: u32,
        height: u32,
        adapter: &wgpu::Adapter,
    ) -> Result<()> {
        if let Some(surface) = &self.surface {
            let capabilities = surface.get_capabilities(adapter);
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: capabilities.formats[0],
                width,
                height,
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };

            surface.configure(&self.device, &config);
            self.surface_config = Some(config.clone());
            self.init_pipelines(config.format)?;
        }
        Ok(())
    }

    fn init_pipelines(&mut self, surface_format: wgpu::TextureFormat) -> Result<()> {
        // YUV to RGB conversion shader
        let yuv_shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("YUV to RGB Shader"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!(
                    "shaders/yuv_to_rgb.wgsl"
                ))),
            });

        // Scale shader
        let scale_shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Scale Shader"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("shaders/scale.wgsl"))),
            });

        // Blend shader
        let blend_shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Blend Shader"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("shaders/blend.wgsl"))),
            });

        // Transform shader
        let transform_shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Transform Shader"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!(
                    "shaders/transform.wgsl"
                ))),
            });

        // Create render pipelines
        self.yuv_to_rgb_pipeline = Some(self.create_render_pipeline(
            "YUV to RGB Pipeline",
            &yuv_shader,
            surface_format,
        )?);

        self.scale_pipeline =
            Some(self.create_render_pipeline("Scale Pipeline", &scale_shader, surface_format)?);

        self.blend_pipeline =
            Some(self.create_render_pipeline("Blend Pipeline", &blend_shader, surface_format)?);

        self.transform_pipeline = Some(self.create_render_pipeline(
            "Transform Pipeline",
            &transform_shader,
            surface_format,
        )?);

        Ok(())
    }

    fn create_render_pipeline(
        &self,
        label: &str,
        shader: &wgpu::ShaderModule,
        surface_format: wgpu::TextureFormat,
    ) -> Result<wgpu::RenderPipeline> {
        let render_pipeline_layout =
            self.device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some(&format!("{} Layout", label)),
                    bind_group_layouts: &[
                        &self.texture_bind_group_layout,
                        &self.uniform_bind_group_layout,
                    ],
                    push_constant_ranges: &[],
                });

        let render_pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&render_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: "vs_main",
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[
                            wgpu::VertexAttribute {
                                offset: 0,
                                shader_location: 0,
                                format: wgpu::VertexFormat::Float32x2,
                            },
                            wgpu::VertexAttribute {
                                offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                                shader_location: 1,
                                format: wgpu::VertexFormat::Float32x2,
                            },
                        ],
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: "fs_main",
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: surface_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: Some(wgpu::Face::Back),
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: 1,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview: None,
                cache: None,
            });

        Ok(render_pipeline)
    }

    /// Convert YUV texture to RGB
    pub fn yuv_to_rgb(
        &self,
        y_texture: &wgpu::Texture,
        u_texture: &wgpu::Texture,
        v_texture: &wgpu::Texture,
        output: &wgpu::TextureView,
        color_space: ColorSpace,
    ) -> Result<()> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("YUV to RGB Encoder"),
            });

        // Create bind groups for YUV textures
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let y_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &y_texture.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
            label: Some("Y Texture Bind Group"),
        });

        // Create color conversion uniforms based on color space
        let color_uniforms = self.create_color_conversion_uniforms(color_space);
        let uniform_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Color Uniforms Buffer"),
                contents: bytemuck::cast_slice(&[color_uniforms]),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        let uniform_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
            label: Some("Uniform Bind Group"),
        });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("YUV to RGB Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output,
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

            if let Some(pipeline) = &self.yuv_to_rgb_pipeline {
                render_pass.set_pipeline(pipeline);
                render_pass.set_bind_group(0, &y_bind_group, &[]);
                render_pass.set_bind_group(1, &uniform_bind_group, &[]);
                render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                render_pass
                    .set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
                render_pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        Ok(())
    }

    /// Scale texture with bilinear or bicubic filtering
    pub fn scale_texture(
        &self,
        input: &wgpu::TextureView,
        output: &wgpu::TextureView,
        scale_factor: f32,
        filter_mode: FilterMode,
    ) -> Result<()> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Scale Encoder"),
            });

        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: match filter_mode {
                FilterMode::Linear => wgpu::FilterMode::Linear,
                FilterMode::Nearest => wgpu::FilterMode::Nearest,
            },
            min_filter: match filter_mode {
                FilterMode::Linear => wgpu::FilterMode::Linear,
                FilterMode::Nearest => wgpu::FilterMode::Nearest,
            },
            ..Default::default()
        });

        let texture_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(input),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
            label: Some("Scale Texture Bind Group"),
        });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Scale Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output,
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

            if let Some(pipeline) = &self.scale_pipeline {
                render_pass.set_pipeline(pipeline);
                render_pass.set_bind_group(0, &texture_bind_group, &[]);
                render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                render_pass
                    .set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
                render_pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        Ok(())
    }

    /// Blend two textures with specified blend mode and opacity
    pub fn blend_textures(
        &self,
        base: &wgpu::TextureView,
        overlay: &wgpu::TextureView,
        output: &wgpu::TextureView,
        blend_mode: BlendMode,
        opacity: f32,
    ) -> Result<()> {
        // Implementation would create appropriate bind groups and render pass
        // Similar to scale_texture but with blend-specific uniforms
        Ok(())
    }

    /// Apply transform matrix to texture
    pub fn transform_texture(
        &self,
        input: &wgpu::TextureView,
        output: &wgpu::TextureView,
        transform: &TransformUniforms,
    ) -> Result<()> {
        // Implementation similar to other operations
        Ok(())
    }

    fn create_color_conversion_uniforms(&self, color_space: ColorSpace) -> ColorUniforms {
        match color_space {
            ColorSpace::Srgb | ColorSpace::Rec709 => ColorUniforms {
                // BT.709 YUV to RGB conversion matrix
                color_matrix: [
                    [1.164, 0.000, 1.596, 0.0],
                    [1.164, -0.392, -0.813, 0.0],
                    [1.164, 2.017, 0.000, 0.0],
                    [-0.874, 0.531, -1.088, 1.0],
                ],
                brightness: 0.0,
                contrast: 1.0,
                saturation: 1.0,
                hue: 0.0,
            },
            ColorSpace::Rec2020 => ColorUniforms {
                // BT.2020 YUV to RGB conversion matrix
                color_matrix: [
                    [1.164, 0.000, 1.717, 0.0],
                    [1.164, -0.192, -0.650, 0.0],
                    [1.164, 2.190, 0.000, 0.0],
                    [-0.931, 0.394, -1.186, 1.0],
                ],
                brightness: 0.0,
                contrast: 1.0,
                saturation: 1.0,
                hue: 0.0,
            },
            ColorSpace::DciP3 => ColorUniforms {
                // DCI-P3 conversion matrix
                color_matrix: [
                    [1.164, 0.000, 1.596, 0.0],
                    [1.164, -0.392, -0.813, 0.0],
                    [1.164, 2.017, 0.000, 0.0],
                    [-0.874, 0.531, -1.088, 1.0],
                ],
                brightness: 0.0,
                contrast: 1.0,
                saturation: 1.0,
                hue: 0.0,
            },
        }
    }

    /// Create texture from raw RGBA data
    pub fn create_texture_from_rgba(
        &self,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<wgpu::Texture> {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("RGBA Texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        Ok(texture)
    }

    /// Render to texture and read back to CPU
    pub async fn render_to_cpu(
        &self,
        width: u32,
        height: u32,
        render_fn: impl FnOnce(&mut wgpu::RenderPass),
    ) -> Result<Vec<u8>> {
        self.render_to_cpu_with_pump(width, height, render_fn, || {
            self.device.poll(wgpu::Maintain::Poll);
        })
        .await
    }

    pub async fn render_to_cpu_with_pump(
        &self,
        width: u32,
        height: u32,
        render_fn: impl FnOnce(&mut wgpu::RenderPass),
        mut pump: impl FnMut(),
    ) -> Result<Vec<u8>> {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Render Target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render to CPU Encoder"),
            });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render to CPU Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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

            render_fn(&mut render_pass);
        }

        // Create buffer for readback
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Readback Buffer"),
            size: (width * height * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * width),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        self.queue.submit(std::iter::once(encoder.finish()));

        // Map buffer and read data
        let buffer_slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });

        use std::sync::mpsc::TryRecvError;
        use std::time::Duration;

        loop {
            match rx.try_recv() {
                Ok(Ok(())) => break,
                Ok(Err(_)) => return Err(anyhow::anyhow!("Buffer async error")),
                Err(TryRecvError::Empty) => {
                    pump();
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(TryRecvError::Disconnected) => {
                    return Err(anyhow::anyhow!("Readback channel disconnected"))
                }
            }
        }

        let data = buffer_slice.get_mapped_range().to_vec();
        buffer.unmap();

        Ok(data)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FilterMode {
    Nearest,
    Linear,
}

#[derive(Debug, Clone, Copy)]
pub enum BlendMode {
    Normal,
    Multiply,
    Screen,
    Overlay,
    SoftLight,
    HardLight,
    ColorDodge,
    ColorBurn,
    Darken,
    Lighten,
    Difference,
    Exclusion,
}

/// CPU fallback renderer for systems without adequate GPU support
pub struct CpuRenderer {
    width: u32,
    height: u32,
}

impl CpuRenderer {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Software YUV to RGB conversion
    pub fn yuv_to_rgb_cpu(
        &self,
        y_plane: &[u8],
        u_plane: &[u8],
        v_plane: &[u8],
        output: &mut [u8],
        color_space: ColorSpace,
    ) -> Result<()> {
        // Software implementation of YUV to RGB conversion
        // This would be much slower but provides fallback capability
        Ok(())
    }

    /// Software scaling with bilinear interpolation
    pub fn scale_cpu(
        &self,
        input: &[u8],
        input_width: u32,
        input_height: u32,
        output: &mut [u8],
        output_width: u32,
        output_height: u32,
    ) -> Result<()> {
        // Software bilinear scaling implementation
        Ok(())
    }

    /// Software alpha blending
    pub fn blend_cpu(
        &self,
        base: &[u8],
        overlay: &[u8],
        output: &mut [u8],
        blend_mode: BlendMode,
        opacity: f32,
    ) -> Result<()> {
        // Software blending implementation
        Ok(())
    }
}
