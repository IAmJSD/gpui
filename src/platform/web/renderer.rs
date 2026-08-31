use super::{WebGpuAtlas, atlas::WebGpuTextureInfo};
use crate::{
    Background, Bounds, DevicePixels, GpuSpecs, Path, Point, PrimitiveBatch, ScaledPixels, Scene,
    Size,
};
use anyhow::{Context as _, Result, anyhow};
use std::{iter, num::NonZeroU64, sync::Arc};

/// WebGPU guarantees 4x multisampling for every renderable color format, so
/// unlike blade there is no capability probe (nor a `ZED_PATH_SAMPLE_COUNT`
/// escape hatch; the browser has no environment variables).
const PATH_SAMPLE_COUNT: u32 = 4;

const INITIAL_INSTANCE_BUFFER_SIZE: u64 = 4 * 1024;

// Uploaded verbatim as the shader's `GlobalParams` uniform.
#[repr(C)]
#[derive(Clone, Copy)]
struct GlobalParams {
    viewport_size: [f32; 2],
    premultiplied_alpha: u32,
    pad: u32,
}

// Matches the shader's `PathSprite` struct.
#[derive(Clone)]
#[repr(C)]
struct PathSprite {
    bounds: Bounds<ScaledPixels>,
}

// Matches the shader's `PathRasterizationVertex` struct.
#[derive(Clone)]
#[repr(C)]
struct PathRasterizationVertex {
    xy_position: Point<ScaledPixels>,
    st_position: Point<f32>,
    color: Background,
    bounds: Bounds<ScaledPixels>,
}

/// Reinterprets a slice as raw bytes for a GPU upload.
///
/// SAFETY: `T` must be `#[repr(C)]` and match the WGSL-side layout of the
/// corresponding shader struct (the scene primitives, `GlobalParams`,
/// `PathSprite`, and `PathRasterizationVertex` all are). Padding bytes -- e.g.
/// the three after `PolychromeSprite.grayscale`, which the shader masks off
/// with `& 0xFFu` -- are read as-is; this is the same raw copy the native
/// renderer performs via blade_util's `BufferBelt::alloc_typed`.
unsafe fn as_bytes<T>(data: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), std::mem::size_of_val(data)) }
}

/// The GPU device and queue shared by every window's renderer.
///
/// Acquired asynchronously (WebGPU adapter/device requests return promises)
/// before the application's launch callback runs, so that window creation and
/// drawing can stay synchronous afterwards.
pub(crate) struct WebGpuContext {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    adapter_info: wgpu::AdapterInfo,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl WebGpuContext {
    pub(crate) async fn new() -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .map_err(|error| anyhow!("failed to request a WebGPU adapter: {error}"))?;
        let adapter_info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .map_err(|error| anyhow!("failed to request a WebGPU device: {error}"))?;
        Ok(Self {
            instance,
            adapter,
            adapter_info,
            device,
            queue,
        })
    }
}

struct Pipeline {
    pipeline: wgpu::RenderPipeline,
    // Group 0 of the pipeline's default layout: exactly the bindings the
    // pipeline's entry points statically use.
    bind_group_layout: wgpu::BindGroupLayout,
}

struct Pipelines {
    quads: Pipeline,
    shadows: Pipeline,
    path_rasterization: Pipeline,
    paths: Pipeline,
    underlines: Pipeline,
    mono_sprites: Pipeline,
    poly_sprites: Pipeline,
}

impl Pipelines {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        alpha_mode: wgpu::CompositeAlphaMode,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()),
        });

        // Mirrors blade: PreMultiplied surfaces get premultiplied blending,
        // everything else (Opaque here plays blade's `Ignored`) gets straight
        // alpha blending.
        let blend_mode = match alpha_mode {
            wgpu::CompositeAlphaMode::PreMultiplied => {
                wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING
            }
            _ => wgpu::BlendState::ALPHA_BLENDING,
        };

        let create = |label: &str,
                      vs: &str,
                      fs: &str,
                      topology: wgpu::PrimitiveTopology,
                      blend: wgpu::BlendState,
                      sample_count: u32| {
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: sample_count,
                    ..Default::default()
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            let bind_group_layout = pipeline.get_bind_group_layout(0);
            Pipeline {
                pipeline,
                bind_group_layout,
            }
        };

        use wgpu::PrimitiveTopology::{TriangleList, TriangleStrip};
        Self {
            quads: create("quads", "vs_quad", "fs_quad", TriangleStrip, blend_mode, 1),
            shadows: create(
                "shadows",
                "vs_shadow",
                "fs_shadow",
                TriangleStrip,
                blend_mode,
                1,
            ),
            path_rasterization: create(
                "path_rasterization",
                "vs_path_rasterization",
                "fs_path_rasterization",
                TriangleList,
                wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
                PATH_SAMPLE_COUNT,
            ),
            paths: create(
                "paths",
                "vs_path",
                "fs_path",
                TriangleStrip,
                // Same as blade: OVER for color, additive for alpha.
                wgpu::BlendState {
                    color: wgpu::BlendComponent::OVER,
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::One,
                        operation: wgpu::BlendOperation::Add,
                    },
                },
                1,
            ),
            underlines: create(
                "underlines",
                "vs_underline",
                "fs_underline",
                TriangleStrip,
                blend_mode,
                1,
            ),
            mono_sprites: create(
                "mono-sprites",
                "vs_mono_sprite",
                "fs_mono_sprite",
                TriangleStrip,
                blend_mode,
                1,
            ),
            poly_sprites: create(
                "poly-sprites",
                "vs_poly_sprite",
                "fs_poly_sprite",
                TriangleStrip,
                blend_mode,
                1,
            ),
        }
    }
}

/// A growable instance buffer for one primitive kind; the wgpu analogue of
/// blade_util's `BufferBelt`. Batches are packed at aligned offsets within a
/// frame and the cursor is reset each frame; `Queue::write_buffer` stages the
/// data, which lands before the frame's submit executes.
struct InstanceBuffer {
    label: &'static str,
    buffer: wgpu::Buffer,
    capacity: u64,
    offset: u64,
}

impl InstanceBuffer {
    fn new(device: &wgpu::Device, label: &'static str) -> Self {
        Self {
            label,
            buffer: Self::create_buffer(device, label, INITIAL_INSTANCE_BUFFER_SIZE),
            capacity: INITIAL_INSTANCE_BUFFER_SIZE,
            offset: 0,
        }
    }

    fn create_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Uploads `bytes` (non-empty) and returns their offset and size within
    /// `self.buffer`. Growing replaces the buffer; earlier batches' bind
    /// groups keep the previous buffer (and its already-staged data) alive.
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        alignment: u64,
        bytes: &[u8],
    ) -> (u64, NonZeroU64) {
        let size = bytes.len() as u64;
        let mut offset = self.offset.next_multiple_of(alignment.max(1));
        if offset + size > self.capacity {
            // Double on every reallocation so a frame's worth of batches
            // settles into one buffer after a few frames.
            let mut capacity = (self.capacity * 2).max(INITIAL_INSTANCE_BUFFER_SIZE);
            while capacity < size {
                capacity *= 2;
            }
            self.buffer = Self::create_buffer(device, self.label, capacity);
            self.capacity = capacity;
            offset = 0;
        }
        queue.write_buffer(&self.buffer, offset, bytes);
        self.offset = offset + size;
        (offset, NonZeroU64::new(size).unwrap())
    }
}

struct InstanceBuffers {
    quads: InstanceBuffer,
    shadows: InstanceBuffer,
    path_vertices: InstanceBuffer,
    path_sprites: InstanceBuffer,
    underlines: InstanceBuffer,
    mono_sprites: InstanceBuffer,
    poly_sprites: InstanceBuffer,
}

impl InstanceBuffers {
    fn new(device: &wgpu::Device) -> Self {
        Self {
            quads: InstanceBuffer::new(device, "quads"),
            shadows: InstanceBuffer::new(device, "shadows"),
            path_vertices: InstanceBuffer::new(device, "path vertices"),
            path_sprites: InstanceBuffer::new(device, "path sprites"),
            underlines: InstanceBuffer::new(device, "underlines"),
            mono_sprites: InstanceBuffer::new(device, "mono sprites"),
            poly_sprites: InstanceBuffer::new(device, "poly sprites"),
        }
    }

    fn reset(&mut self) {
        self.quads.offset = 0;
        self.shadows.offset = 0;
        self.path_vertices.offset = 0;
        self.path_sprites.offset = 0;
        self.underlines.offset = 0;
        self.mono_sprites.offset = 0;
        self.poly_sprites.offset = 0;
    }
}

/// Renders gpui scenes into an `HtmlCanvasElement` via WebGPU; a port of
/// `BladeRenderer` to wgpu.
pub(crate) struct WebGpuRenderer {
    context: Arc<WebGpuContext>,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    pipelines: Pipelines,
    instances: InstanceBuffers,
    storage_alignment: u64,
    atlas: Arc<WebGpuAtlas>,
    atlas_sampler: wgpu::Sampler,
    globals_buffer: wgpu::Buffer,
    // The path pre-pass wants its own globals (premultiplied_alpha forced to
    // 0, like blade); all `write_buffer`s in a frame land before its submit,
    // so one buffer cannot hold two values within a frame.
    path_rasterization_globals_buffer: wgpu::Buffer,
    gamma_ratios_buffer: wgpu::Buffer,
    grayscale_enhanced_contrast_buffer: wgpu::Buffer,
    path_intermediate_texture: wgpu::Texture,
    path_intermediate_texture_view: wgpu::TextureView,
    path_intermediate_msaa_texture_view: Option<wgpu::TextureView>,
}

impl WebGpuRenderer {
    pub(crate) fn new(
        context: &Arc<WebGpuContext>,
        canvas: &web_sys::HtmlCanvasElement,
        size: Size<DevicePixels>,
    ) -> Result<Self> {
        let device = &context.device;
        let surface = context
            .instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|error| anyhow!("failed to create a WebGPU canvas surface: {error}"))?;

        let width = (size.width.0.max(1)) as u32;
        let height = (size.height.0.max(1)) as u32;
        let mut surface_config = surface
            .get_default_config(&context.adapter, width, height)
            .context("the WebGPU adapter does not support rendering to a canvas surface")?;
        let capabilities = surface.get_capabilities(&context.adapter);
        surface_config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        surface_config.present_mode = wgpu::PresentMode::Fifo;
        surface_config.alpha_mode = if capabilities
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::Opaque)
        {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            capabilities
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto)
        };
        // Like blade (Bgra8Unorm + an sRGB-nonlinear color space), render into
        // a non-sRGB format: the shaders output sRGB-encoded values directly.
        let non_srgb = surface_config.format.remove_srgb_suffix();
        if capabilities.formats.contains(&non_srgb) {
            surface_config.format = non_srgb;
        }
        surface.configure(device, &surface_config);

        let pipelines = Pipelines::new(device, surface_config.format, surface_config.alpha_mode);
        let instances = InstanceBuffers::new(device);
        let storage_alignment = device.limits().min_storage_buffer_offset_alignment as u64;

        let atlas = Arc::new(WebGpuAtlas::new(device.clone(), context.queue.clone()));
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform = |label: &str| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let globals_buffer = uniform("globals");
        let path_rasterization_globals_buffer = uniform("path rasterization globals");
        let gamma_ratios_buffer = uniform("gamma ratios");
        let grayscale_enhanced_contrast_buffer = uniform("grayscale enhanced contrast");

        // Blade reads these from ZED_FONTS_GAMMA / _GRAYSCALE_ENHANCED_CONTRAST;
        // there is no environment in the browser, so the defaults (1.8, 1.0)
        // are fixed here. They never change, so they are written once.
        let gamma_ratios = get_gamma_ratios(1.8);
        let grayscale_enhanced_contrast = [1.0f32, 0.0, 0.0, 0.0];
        // SAFETY: `[f32; 4]` is plain data with no padding.
        context
            .queue
            .write_buffer(&gamma_ratios_buffer, 0, unsafe { as_bytes(&gamma_ratios) });
        context
            .queue
            .write_buffer(&grayscale_enhanced_contrast_buffer, 0, unsafe {
                as_bytes(&grayscale_enhanced_contrast)
            });

        let (path_intermediate_texture, path_intermediate_texture_view, msaa_view) =
            create_path_intermediate_textures(device, surface_config.format, width, height);

        Ok(Self {
            context: context.clone(),
            surface,
            surface_config,
            pipelines,
            instances,
            storage_alignment,
            atlas,
            atlas_sampler,
            globals_buffer,
            path_rasterization_globals_buffer,
            gamma_ratios_buffer,
            grayscale_enhanced_contrast_buffer,
            path_intermediate_texture,
            path_intermediate_texture_view,
            path_intermediate_msaa_texture_view: msaa_view,
        })
    }

    pub(crate) fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        let width = (size.width.0.max(1)) as u32;
        let height = (size.height.0.max(1)) as u32;
        if width == self.surface_config.width && height == self.surface_config.height {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface
            .configure(&self.context.device, &self.surface_config);
        let (texture, view, msaa_view) = create_path_intermediate_textures(
            &self.context.device,
            self.surface_config.format,
            width,
            height,
        );
        self.path_intermediate_texture = texture;
        self.path_intermediate_texture_view = view;
        self.path_intermediate_msaa_texture_view = msaa_view;
    }

    #[allow(dead_code)]
    pub(crate) fn viewport_size(&self) -> Size<DevicePixels> {
        Size {
            width: DevicePixels(self.surface_config.width as i32),
            height: DevicePixels(self.surface_config.height as i32),
        }
    }

    pub(crate) fn sprite_atlas(&self) -> Arc<WebGpuAtlas> {
        self.atlas.clone()
    }

    pub(crate) fn gpu_specs(&self) -> GpuSpecs {
        let info = &self.context.adapter_info;
        GpuSpecs {
            is_software_emulated: info.device_type == wgpu::DeviceType::Cpu,
            device_name: info.name.clone(),
            driver_name: info.driver.clone(),
            driver_info: info.driver_info.clone(),
        }
    }

    pub(crate) fn draw(&mut self, scene: &Scene) {
        let device = self.context.device.clone();
        let queue = self.context.queue.clone();

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&device, &self.surface_config);
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                log::error!("validation error while acquiring the next frame");
                return;
            }
        };
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let viewport_size = [
            self.surface_config.width as f32,
            self.surface_config.height as f32,
        ];
        let premultiplied_alpha = matches!(
            self.surface_config.alpha_mode,
            wgpu::CompositeAlphaMode::PreMultiplied
        ) as u32;
        let globals = GlobalParams {
            viewport_size,
            premultiplied_alpha,
            pad: 0,
        };
        let path_rasterization_globals = GlobalParams {
            viewport_size,
            premultiplied_alpha: 0,
            pad: 0,
        };
        // SAFETY: `GlobalParams` is repr(C) with no padding.
        queue.write_buffer(&self.globals_buffer, 0, unsafe {
            as_bytes(std::slice::from_ref(&globals))
        });
        queue.write_buffer(&self.path_rasterization_globals_buffer, 0, unsafe {
            as_bytes(std::slice::from_ref(&path_rasterization_globals))
        });

        self.instances.reset();
        // Recorded passes hold their own references, but keeping every
        // per-batch bind group here until after the submit removes any doubt
        // about resource lifetimes.
        let mut bind_groups = Vec::new();

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("main"),
        });
        let mut pass = begin_frame_pass(
            &mut encoder,
            &frame_view,
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        );

        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::Quads(quads) => {
                    if quads.is_empty() {
                        continue;
                    }
                    // SAFETY: `Quad` is repr(C), mirrored by the shader.
                    let (offset, size) = self.instances.quads.upload(
                        &device,
                        &queue,
                        self.storage_alignment,
                        unsafe { as_bytes(quads) },
                    );
                    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("quads"),
                        layout: &self.pipelines.quads.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: self.globals_buffer.as_entire_binding(),
                            },
                            buffer_entry(5, &self.instances.quads.buffer, offset, size),
                        ],
                    });
                    pass.set_pipeline(&self.pipelines.quads.pipeline);
                    pass.set_bind_group(0, &bind_group, &[]);
                    pass.draw(0..4, 0..quads.len() as u32);
                    bind_groups.push(bind_group);
                }
                PrimitiveBatch::Shadows(shadows) => {
                    if shadows.is_empty() {
                        continue;
                    }
                    // SAFETY: `Shadow` is repr(C), mirrored by the shader.
                    let (offset, size) = self.instances.shadows.upload(
                        &device,
                        &queue,
                        self.storage_alignment,
                        unsafe { as_bytes(shadows) },
                    );
                    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("shadows"),
                        layout: &self.pipelines.shadows.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: self.globals_buffer.as_entire_binding(),
                            },
                            buffer_entry(6, &self.instances.shadows.buffer, offset, size),
                        ],
                    });
                    pass.set_pipeline(&self.pipelines.shadows.pipeline);
                    pass.set_bind_group(0, &bind_group, &[]);
                    pass.draw(0..4, 0..shadows.len() as u32);
                    bind_groups.push(bind_group);
                }
                PrimitiveBatch::Paths(paths) => {
                    let Some(first_path) = paths.first() else {
                        continue;
                    };
                    // Rasterize the paths into the intermediate texture in a
                    // pass of their own, then resume the frame pass (loading
                    // what has been rendered so far) and copy the covered
                    // bounds from the intermediate to the frame.
                    drop(pass);
                    self.rasterize_paths(&device, &queue, &mut encoder, paths, &mut bind_groups);
                    pass = begin_frame_pass(&mut encoder, &frame_view, wgpu::LoadOp::Load);

                    // When copying paths from the intermediate texture to the drawable,
                    // each pixel must only be copied once, in case of transparent paths.
                    //
                    // If all paths have the same draw order, then their bounds are all
                    // disjoint, so we can copy each path's bounds individually. If this
                    // batch combines different draw orders, we perform a single copy
                    // for a minimal spanning rect.
                    let sprites = if paths.last().unwrap().order == first_path.order {
                        paths
                            .iter()
                            .map(|path| PathSprite {
                                bounds: path.clipped_bounds(),
                            })
                            .collect()
                    } else {
                        let mut bounds = first_path.clipped_bounds();
                        for path in paths.iter().skip(1) {
                            bounds = bounds.union(&path.clipped_bounds());
                        }
                        vec![PathSprite { bounds }]
                    };
                    // SAFETY: `PathSprite` is repr(C), mirrored by the shader.
                    let (offset, size) = self.instances.path_sprites.upload(
                        &device,
                        &queue,
                        self.storage_alignment,
                        unsafe { as_bytes(&sprites) },
                    );
                    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("paths"),
                        layout: &self.pipelines.paths.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: self.globals_buffer.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::TextureView(
                                    &self.path_intermediate_texture_view,
                                ),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: wgpu::BindingResource::Sampler(&self.atlas_sampler),
                            },
                            buffer_entry(8, &self.instances.path_sprites.buffer, offset, size),
                        ],
                    });
                    pass.set_pipeline(&self.pipelines.paths.pipeline);
                    pass.set_bind_group(0, &bind_group, &[]);
                    pass.draw(0..4, 0..sprites.len() as u32);
                    bind_groups.push(bind_group);
                }
                PrimitiveBatch::Underlines(underlines) => {
                    if underlines.is_empty() {
                        continue;
                    }
                    // SAFETY: `Underline` is repr(C), mirrored by the shader.
                    let (offset, size) = self.instances.underlines.upload(
                        &device,
                        &queue,
                        self.storage_alignment,
                        unsafe { as_bytes(underlines) },
                    );
                    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("underlines"),
                        layout: &self.pipelines.underlines.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: self.globals_buffer.as_entire_binding(),
                            },
                            buffer_entry(9, &self.instances.underlines.buffer, offset, size),
                        ],
                    });
                    pass.set_pipeline(&self.pipelines.underlines.pipeline);
                    pass.set_bind_group(0, &bind_group, &[]);
                    pass.draw(0..4, 0..underlines.len() as u32);
                    bind_groups.push(bind_group);
                }
                PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites,
                } => {
                    if sprites.is_empty() {
                        continue;
                    }
                    let WebGpuTextureInfo { raw_view } = self.atlas.get_texture_info(texture_id);
                    // SAFETY: `MonochromeSprite` is repr(C), mirrored by the shader.
                    let (offset, size) = self.instances.mono_sprites.upload(
                        &device,
                        &queue,
                        self.storage_alignment,
                        unsafe { as_bytes(sprites) },
                    );
                    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("mono sprites"),
                        layout: &self.pipelines.mono_sprites.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: self.globals_buffer.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: self.gamma_ratios_buffer.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: self
                                    .grayscale_enhanced_contrast_buffer
                                    .as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::TextureView(&raw_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: wgpu::BindingResource::Sampler(&self.atlas_sampler),
                            },
                            buffer_entry(10, &self.instances.mono_sprites.buffer, offset, size),
                        ],
                    });
                    pass.set_pipeline(&self.pipelines.mono_sprites.pipeline);
                    pass.set_bind_group(0, &bind_group, &[]);
                    pass.draw(0..4, 0..sprites.len() as u32);
                    bind_groups.push(bind_group);
                }
                PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites,
                } => {
                    if sprites.is_empty() {
                        continue;
                    }
                    let WebGpuTextureInfo { raw_view } = self.atlas.get_texture_info(texture_id);
                    // SAFETY: `PolychromeSprite` is repr(C), mirrored by the
                    // shader; its padding after `grayscale` is masked off there.
                    let (offset, size) = self.instances.poly_sprites.upload(
                        &device,
                        &queue,
                        self.storage_alignment,
                        unsafe { as_bytes(sprites) },
                    );
                    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("poly sprites"),
                        layout: &self.pipelines.poly_sprites.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: self.globals_buffer.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::TextureView(&raw_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: wgpu::BindingResource::Sampler(&self.atlas_sampler),
                            },
                            buffer_entry(11, &self.instances.poly_sprites.buffer, offset, size),
                        ],
                    });
                    pass.set_pipeline(&self.pipelines.poly_sprites.pipeline);
                    pass.set_bind_group(0, &bind_group, &[]);
                    pass.draw(0..4, 0..sprites.len() as u32);
                    bind_groups.push(bind_group);
                }
                PrimitiveBatch::Surfaces(surfaces) => {
                    // Video surfaces are macOS CoreVideo buffers; they cannot
                    // exist on the web.
                    log::error!("cannot draw {} surfaces on the web", surfaces.len());
                }
            }
        }
        drop(pass);

        queue.submit(iter::once(encoder.finish()));
        queue.present(frame);
        drop(bind_groups);
    }

    /// The path pre-pass: rasterizes a batch of paths into the intermediate
    /// texture (multisampled, resolved on store), which the frame pass then
    /// samples via the `paths` pipeline.
    fn rasterize_paths(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        paths: &[Path<ScaledPixels>],
        bind_groups: &mut Vec<wgpu::BindGroup>,
    ) {
        let mut vertices = Vec::new();
        for path in paths {
            vertices.extend(path.vertices.iter().map(|v| PathRasterizationVertex {
                xy_position: v.xy_position,
                st_position: v.st_position,
                color: path.color,
                bounds: path.clipped_bounds(),
            }));
        }
        if vertices.is_empty() {
            return;
        }
        // SAFETY: `PathRasterizationVertex` is repr(C), mirrored by the shader.
        let (offset, size) =
            self.instances
                .path_vertices
                .upload(device, queue, self.storage_alignment, unsafe {
                    as_bytes(&vertices)
                });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("path rasterization"),
            layout: &self.pipelines.path_rasterization.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.path_rasterization_globals_buffer.as_entire_binding(),
                },
                buffer_entry(7, &self.instances.path_vertices.buffer, offset, size),
            ],
        });

        let color_attachment = match &self.path_intermediate_msaa_texture_view {
            Some(msaa_view) => wgpu::RenderPassColorAttachment {
                view: msaa_view,
                depth_slice: None,
                resolve_target: Some(&self.path_intermediate_texture_view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Discard,
                },
            },
            None => wgpu::RenderPassColorAttachment {
                view: &self.path_intermediate_texture_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            },
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rasterize paths"),
            color_attachments: &[Some(color_attachment)],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipelines.path_rasterization.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..vertices.len() as u32, 0..1);
        drop(pass);
        bind_groups.push(bind_group);
    }
}

fn buffer_entry<'a>(
    binding: u32,
    buffer: &'a wgpu::Buffer,
    offset: u64,
    size: NonZeroU64,
) -> wgpu::BindGroupEntry<'a> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
            buffer,
            offset,
            size: Some(size),
        }),
    }
}

fn begin_frame_pass<'a>(
    encoder: &'a mut wgpu::CommandEncoder,
    frame_view: &'a wgpu::TextureView,
    load: wgpu::LoadOp<wgpu::Color>,
) -> wgpu::RenderPass<'static> {
    encoder
        .begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("main"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        })
        // The pass records into its own storage; untying it from the encoder
        // borrow lets the batch loop suspend the frame pass for the path
        // pre-pass. It must still be dropped before `encoder.finish()`.
        .forget_lifetime()
}

fn create_path_intermediate_textures(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView, Option<wgpu::TextureView>) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("path intermediate"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let msaa_view = (PATH_SAMPLE_COUNT > 1).then(|| {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("path intermediate msaa"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: PATH_SAMPLE_COUNT,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default())
    });

    (texture, view, msaa_view)
}

// Gamma ratios for brightening/darkening edges for better contrast, ported
// from `RenderingParameters` in blade_renderer.rs.
// https://github.com/microsoft/terminal/blob/1283c0f5b99a2961673249fa77c6b986efb5086c/src/renderer/atlas/dwrite.cpp#L50
fn get_gamma_ratios(gamma: f32) -> [f32; 4] {
    const GAMMA_INCORRECT_TARGET_RATIOS: [[f32; 4]; 13] = [
        [0.0000 / 4.0, 0.0000 / 4.0, 0.0000 / 4.0, 0.0000 / 4.0], // gamma = 1.0
        [0.0166 / 4.0, -0.0807 / 4.0, 0.2227 / 4.0, -0.0751 / 4.0], // gamma = 1.1
        [0.0350 / 4.0, -0.1760 / 4.0, 0.4325 / 4.0, -0.1370 / 4.0], // gamma = 1.2
        [0.0543 / 4.0, -0.2821 / 4.0, 0.6302 / 4.0, -0.1876 / 4.0], // gamma = 1.3
        [0.0739 / 4.0, -0.3963 / 4.0, 0.8167 / 4.0, -0.2287 / 4.0], // gamma = 1.4
        [0.0933 / 4.0, -0.5161 / 4.0, 0.9926 / 4.0, -0.2616 / 4.0], // gamma = 1.5
        [0.1121 / 4.0, -0.6395 / 4.0, 1.1588 / 4.0, -0.2877 / 4.0], // gamma = 1.6
        [0.1300 / 4.0, -0.7649 / 4.0, 1.3159 / 4.0, -0.3080 / 4.0], // gamma = 1.7
        [0.1469 / 4.0, -0.8911 / 4.0, 1.4644 / 4.0, -0.3234 / 4.0], // gamma = 1.8
        [0.1627 / 4.0, -1.0170 / 4.0, 1.6051 / 4.0, -0.3347 / 4.0], // gamma = 1.9
        [0.1773 / 4.0, -1.1420 / 4.0, 1.7385 / 4.0, -0.3426 / 4.0], // gamma = 2.0
        [0.1908 / 4.0, -1.2652 / 4.0, 1.8650 / 4.0, -0.3476 / 4.0], // gamma = 2.1
        [0.2031 / 4.0, -1.3864 / 4.0, 1.9851 / 4.0, -0.3501 / 4.0], // gamma = 2.2
    ];

    const NORM13: f32 = ((0x10000 as f64) / (255.0 * 255.0) * 4.0) as f32;
    const NORM24: f32 = ((0x100 as f64) / (255.0) * 4.0) as f32;

    let index = ((gamma * 10.0).round() as usize).clamp(10, 22) - 10;
    let ratios = GAMMA_INCORRECT_TARGET_RATIOS[index];

    [
        ratios[0] * NORM13,
        ratios[1] * NORM24,
        ratios[2] * NORM13,
        ratios[3] * NORM24,
    ]
}
