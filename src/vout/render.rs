//! The video output's renderer (wgpu, see `shader.wgsl`).
//!
//! Each output window has a [`Target`]: its layers are composited into a canvas texture the size
//! of the part of the stage it shows, which is then drawn onto the window through the inverse
//! corner-pin homography (exact per pixel, black outside the canvas, soft-edge blend zones).
//! Decoded pictures are uploaded as they come from FFmpeg: planar YUV and NV12 planes go to the
//! GPU untouched and are converted to RGB in the layer shader. Uploads happen only when a
//! picture's generation changes; a layer seen by several windows (a spanned surface) is
//! uploaded once.

use crate::effects::{VIDEO_FX_FLOATS, video_fx_uniform};
use crate::geometry::{Homography, LayerQuad, layer_quad};
use crate::media::mask::{MaskImage, MaskState};
use crate::media::video::{Picture, Pixels, PlaneLayout, StageLayer};
use crate::model::{BlendMode, CueId, Stage};
use anyhow::Context;
use eframe::wgpu;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Format of the canvases: gamma-encoded values, blended like egui did (and like most video
/// mixers): the look of blend modes and fades stays as it was.
pub const CANVAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Mesh subdivisions per side of a layer.
const GRID: usize = 16;

/// Floats in a layer's uniform (see `Layer` in the shader).
const LAYER_FLOATS: usize = 20 + VIDEO_FX_FLOATS;

/// The effect uniform of a layer without effects.
const NO_FX: [f32; VIDEO_FX_FLOATS] = {
    let mut u = [0.0; VIDEO_FX_FLOATS];
    u[0] = 1.0;
    u[3] = 0.5;
    u
};

/// A draw unused this long has its textures freed.
const DRAW_TTL: Duration = Duration::from_secs(2);

/// Fixed-function blending for `mode` and whether the shader outputs the layer over white.
pub fn blend_state(mode: BlendMode) -> (wgpu::BlendComponent, bool) {
    use wgpu::{BlendComponent as C, BlendFactor as F, BlendOperation as Op};
    let c = |src_factor, dst_factor, operation| C { src_factor, dst_factor, operation };
    match mode {
        BlendMode::Normal => (c(F::One, F::OneMinusSrcAlpha, Op::Add), false),
        BlendMode::Add => (c(F::One, F::One, Op::Add), false),
        BlendMode::Screen => (c(F::One, F::OneMinusSrc, Op::Add), false),
        BlendMode::Multiply => (c(F::Zero, F::Src, Op::Add), true),
        BlendMode::Lighten => (c(F::One, F::One, Op::Max), false),
        BlendMode::Darken => (c(F::One, F::One, Op::Min), true),
        BlendMode::Subtract => (c(F::One, F::One, Op::ReverseSubtract), false),
    }
}

/// Size and format of one picture plane's texture.
type PlaneSpec = ([u32; 2], wgpu::TextureFormat);

/// One textured mesh: a movie/still/camera picture, or one plane of a Text cue.
struct Draw {
    generation: u64,
    /// Plane textures with their size and format.
    planes: Vec<(wgpu::Texture, [u32; 2], wgpu::TextureFormat)>,
    /// The mask texture bound (a key of [`Gpu::masks`]).
    mask: Option<usize>,
    bind: Option<wgpu::BindGroup>,
    uniform: wgpu::Buffer,
    vertices: wgpu::Buffer,
    last_used: Instant,
}

/// What one output window shows this frame.
pub struct Scene<'a> {
    /// The output's own stage (size, corners, crop, blend zones).
    pub stage: &'a Stage,
    /// The stage whose layers it shows (itself, or the surface it spans).
    pub source: &'a Stage,
    /// The layers on `source`, bottom to top.
    pub layers: &'a [StageLayer],
    /// Draw the mapping test pattern under the layers.
    pub mapping: bool,
    /// The corners to warp with (the stage's, or ones being dragged).
    pub corners: [[f32; 2]; 4],
    /// Screen-space picture drawn on top (mapping pins), premultiplied RGBA at the target size.
    pub overlay: Option<&'a Overlay>,
    /// The output's mask and whether it is inverted.
    pub mask: Option<(MaskState, bool)>,
}

/// A mask on the GPU.
struct MaskTexture {
    /// Keeps the picture alive, so its address (the key) isn't reused while this exists.
    _image: Arc<MaskImage>,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    last_used: Instant,
}

/// A premultiplied RGBA picture the size of the window, drawn over everything.
pub struct Overlay {
    /// Bumped when the picture changes.
    pub generation: u64,
    /// Width and height.
    pub size: [u32; 2],
    /// Premultiplied RGBA.
    pub rgba: Vec<u8>,
}

/// GPU state of one output window.
pub struct Target {
    canvas: Option<(wgpu::Texture, wgpu::TextureView, [u32; 2])>,
    view_uniform: wgpu::Buffer,
    view_bind: wgpu::BindGroup,
    warp_uniform: wgpu::Buffer,
    warp_bind: Option<wgpu::BindGroup>,
    /// The mask in `warp_bind`, and the canvas size it was made for.
    warp_made: Option<(Option<usize>, [u32; 2])>,
    /// The mapping test pattern: what it was drawn for, and its draw.
    pattern: Option<(String, Draw)>,
    overlay: Option<(u64, [u32; 2], wgpu::Texture, wgpu::BindGroup)>,
}

/// The device, pipelines and uploaded layer pictures shared by every window.
pub struct Gpu {
    /// The adapter in use.
    pub adapter: wgpu::Adapter,
    /// The device.
    pub device: wgpu::Device,
    /// Its queue.
    pub queue: wgpu::Queue,
    /// Largest texture side the GPU takes.
    pub max_side: u32,
    module: wgpu::ShaderModule,
    layer_bgl: wgpu::BindGroupLayout,
    view_bgl: wgpu::BindGroupLayout,
    warp_bgl: wgpu::BindGroupLayout,
    overlay_bgl: wgpu::BindGroupLayout,
    layer_layout: wgpu::PipelineLayout,
    warp_layout: wgpu::PipelineLayout,
    overlay_layout: wgpu::PipelineLayout,
    layer_pipes: HashMap<BlendMode, wgpu::RenderPipeline>,
    warp_pipes: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    overlay_pipes: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    sampler: wgpu::Sampler,
    index: wgpu::Buffer,
    /// 1×1 white, bound in place of unused planes.
    dummy: wgpu::TextureView,
    draws: HashMap<(CueId, usize), Draw>,
    /// Uploaded masks, by their picture's address.
    masks: HashMap<usize, MaskTexture>,
    /// Masks that couldn't be read, already reported.
    mask_errors: HashSet<Arc<str>>,
    /// Pictures too big for this GPU already reported.
    too_big_reported: bool,
}

fn layout_entry(binding: u32, ty: wgpu::BindingType, vertex: bool) -> wgpu::BindGroupLayoutEntry {
    let visibility = if vertex { wgpu::ShaderStages::VERTEX_FRAGMENT } else { wgpu::ShaderStages::FRAGMENT };
    wgpu::BindGroupLayoutEntry { binding, visibility, ty, count: None }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    layout_entry(
        binding,
        wgpu::BindingType::Texture {
            multisampled: false,
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        false,
    )
}

fn uniform_entry(binding: u32, vertex: bool) -> wgpu::BindGroupLayoutEntry {
    layout_entry(binding, wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, vertex)
}

fn bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Grid vertices of a quad: canvas position (x, y) and texture (u, v) per vertex.
fn grid(q: &LayerQuad) -> Vec<f32> {
    let mut v = Vec::with_capacity((GRID + 1) * (GRID + 1) * 4);
    for j in 0..=GRID {
        for i in 0..=GRID {
            let (s, t) = (i as f32 / GRID as f32, j as f32 / GRID as f32);
            let [x, y] = q.point(s, t);
            let [u, w] = q.uv_at(s, t);
            v.extend([x, y, u, w]);
        }
    }
    v
}

fn grid_indices() -> Vec<u32> {
    let w = (GRID + 1) as u32;
    let mut out = Vec::with_capacity(GRID * GRID * 6);
    for j in 0..GRID as u32 {
        for i in 0..GRID as u32 {
            let a = j * w + i;
            out.extend([a, a + 1, a + w, a + 1, a + w + 1, a + w]);
        }
    }
    out
}

/// The part of `source` an output shows.
pub fn crop_of(stage: &Stage, source: &Stage) -> [f32; 4] {
    if source.id == stage.id { Stage::FULL_CROP } else { stage.crop }
}

/// Pixel size of an output's canvas: its crop of the source stage, at the source's resolution.
pub fn canvas_size(stage: &Stage, source: &Stage, max: u32) -> [u32; 2] {
    let c = crop_of(stage, source);
    let w = (source.width.max(1) as f32 * (c[2] - c[0]).abs()).round() as u32;
    let h = (source.height.max(1) as f32 * (c[3] - c[1]).abs()).round() as u32;
    [w.clamp(1, max), h.clamp(1, max)]
}

impl Gpu {
    /// Open the GPU (one that can draw to `surface`, when given).
    pub fn new(instance: &wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> anyhow::Result<Self> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: surface,
            force_fallback_adapter: false,
        }))
        .context("no GPU adapter")?;
        let limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("openq-video"),
            required_features: wgpu::Features::empty(),
            required_limits: limits.clone(),
            ..Default::default()
        }))
        .context("cannot open the GPU")?;
        let info = adapter.get_info();
        log::info!("video output on {} ({:?})", info.name, info.backend);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("openq-video"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let sampler_entry = |b| layout_entry(b, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), false);
        let layer_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("openq-layer"),
            entries: &[texture_entry(0), texture_entry(1), texture_entry(2), texture_entry(3), sampler_entry(4), uniform_entry(5, false), texture_entry(6)],
        });
        let view_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("openq-view"),
            entries: &[uniform_entry(0, true)],
        });
        let warp_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("openq-warp"),
            entries: &[texture_entry(0), sampler_entry(1), uniform_entry(2, false), texture_entry(3)],
        });
        let overlay_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("openq-overlay"),
            entries: &[texture_entry(0), sampler_entry(1)],
        });
        let pl = |label, groups: &[Option<&wgpu::BindGroupLayout>]| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: groups, immediate_size: 0 })
        };
        let layer_layout = pl("openq-layer", &[Some(&layer_bgl), Some(&view_bgl)]);
        let warp_layout = pl("openq-warp", &[Some(&warp_bgl)]);
        let overlay_layout = pl("openq-overlay", &[Some(&overlay_bgl)]);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("openq-linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let index = wgpu::util::DeviceExt::create_buffer_init(
            &device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("openq-grid"),
                contents: &grid_indices().iter().flat_map(|i| i.to_le_bytes()).collect::<Vec<u8>>(),
                usage: wgpu::BufferUsages::INDEX,
            },
        );
        let white = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("openq-white"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_plane(&queue, &white, &[255; 4], 4, [1, 1], 4);
        let dummy = white.create_view(&Default::default());
        Ok(Self {
            max_side: limits.max_texture_dimension_2d,
            adapter,
            device,
            queue,
            module,
            layer_bgl,
            view_bgl,
            warp_bgl,
            overlay_bgl,
            layer_layout,
            warp_layout,
            overlay_layout,
            layer_pipes: HashMap::new(),
            warp_pipes: HashMap::new(),
            overlay_pipes: HashMap::new(),
            sampler,
            index,
            dummy,
            draws: HashMap::new(),
            masks: HashMap::new(),
            mask_errors: HashSet::new(),
            too_big_reported: false,
        })
    }

    /// A new window's GPU state.
    pub fn target(&self) -> Target {
        let buf = |label, size| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let view_uniform = buf("openq-view", 16);
        let view_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("openq-view"),
            layout: &self.view_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: view_uniform.as_entire_binding() }],
        });
        Target { canvas: None, view_uniform, view_bind, warp_uniform: buf("openq-warp", 80), warp_bind: None, warp_made: None, pattern: None, overlay: None }
    }

    fn layer_pipe(&mut self, mode: BlendMode) -> &wgpu::RenderPipeline {
        let Self { device, module, layer_layout, layer_pipes, .. } = self;
        layer_pipes.entry(mode).or_insert_with(|| {
            let (color, white) = blend_state(mode);
            // The canvas alpha stays as it is.
            let alpha = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::Zero, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add };
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("openq-layer"),
                layout: Some(layer_layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_layer"),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: 16,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                    }],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(if white { "fs_white" } else { "fs_black" }),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: CANVAS_FORMAT,
                        blend: Some(wgpu::BlendState { color, alpha }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        })
    }

    fn full_pipe(&mut self, format: wgpu::TextureFormat, overlay: bool) -> &wgpu::RenderPipeline {
        let Self { device, module, warp_layout, overlay_layout, warp_pipes, overlay_pipes, .. } = self;
        let (pipes, layout, entry, blend) = if overlay {
            let e = if format.is_srgb() { "fs_overlay_linear" } else { "fs_overlay" };
            (overlay_pipes, &*overlay_layout, e, Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING))
        } else {
            (warp_pipes, &*warp_layout, if format.is_srgb() { "fs_warp_linear" } else { "fs_warp" }, None)
        };
        pipes.entry(format).or_insert_with(|| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(layout),
                vertex: wgpu::VertexState { module, entry_point: Some("vs_full"), buffers: &[], compilation_options: Default::default() },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        })
    }

    fn new_draw(&self) -> Draw {
        let uniform = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("openq-layer"),
            size: (LAYER_FLOATS * 4) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let vertices = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("openq-layer-mesh"),
            size: ((GRID + 1) * (GRID + 1) * 16) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Draw { generation: u64::MAX, planes: Vec::new(), mask: None, bind: None, uniform, vertices, last_used: Instant::now() }
    }

    /// The shader's mask mode for `mask` (see `shader.wgsl`) and its texture (uploaded on first use).
    fn mask(&mut self, mask: &Option<(MaskState, bool)>) -> (f32, Option<usize>) {
        let Some((state, invert)) = mask else { return (0.0, None) };
        let img = match state {
            MaskState::Loading => return (3.0, None),
            MaskState::Failed(e) => {
                if self.mask_errors.insert(e.clone()) {
                    log::error!("{e}");
                }
                return (0.0, None);
            }
            MaskState::Ready(img) => img,
        };
        let key = Arc::as_ptr(img) as usize;
        if let Some(m) = self.masks.get_mut(&key) {
            m.last_used = Instant::now();
        } else {
            let size = [img.size[0].max(1), img.size[1].max(1)];
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("openq-mask"),
                size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            write_plane(&self.queue, &texture, &img.data, size[0] as usize, size, 1);
            let view = texture.create_view(&Default::default());
            self.masks.insert(key, MaskTexture { _image: img.clone(), _texture: texture, view, last_used: Instant::now() });
        }
        (if *invert { 2.0 } else { 1.0 }, Some(key))
    }

    fn mask_view(&self, key: Option<usize>) -> &wgpu::TextureView {
        key.and_then(|k| self.masks.get(&k)).map_or(&self.dummy, |m| &m.view)
    }

    /// Make `d`'s plane textures match `want` (size, format) and bind `mask`, making its bind group
    /// again if either changes.
    fn ensure_planes(&self, d: &mut Draw, want: &[PlaneSpec], mask: Option<usize>) {
        let same = d.planes.len() == want.len() && d.planes.iter().zip(want).all(|(p, w)| p.1 == w.0 && p.2 == w.1);
        if same && d.mask == mask && d.bind.is_some() {
            return;
        }
        d.mask = mask;
        if !same {
            d.generation = u64::MAX;
            d.planes = want
            .iter()
            .map(|&(size, format)| {
                let t = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("openq-plane"),
                    size: wgpu::Extent3d { width: size[0].max(1), height: size[1].max(1), depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                (t, size, format)
            })
            .collect();
        }
        let views: Vec<wgpu::TextureView> = d.planes.iter().map(|p| p.0.create_view(&Default::default())).collect();
        let view = |i: usize| views.get(i).unwrap_or(&self.dummy);
        d.bind = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("openq-layer"),
            layout: &self.layer_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view(0)) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(view(1)) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(view(2)) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(view(3)) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                wgpu::BindGroupEntry { binding: 5, resource: d.uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(self.mask_view(mask)) },
            ],
        }));
    }

    /// Upload a layer's picture if it changed and set its draws' uniforms and meshes. Returns the
    /// draw keys, bottom to top.
    fn prepare_layer(&mut self, l: &StageLayer, canvas: [f32; 2]) -> Vec<(CueId, usize)> {
        let f = l.frame.lock();
        if !f.has_picture() {
            return Vec::new();
        }
        if f.width.max(f.height) > self.max_side {
            if !self.too_big_reported {
                self.too_big_reported = true;
                log::error!("a {}×{} picture is larger than this GPU's {} px textures; not shown", f.width, f.height, self.max_side);
            }
            return Vec::new();
        }
        let (mask_mode, mask) = self.mask(&l.mask);
        let q = layer_quad(canvas, f.stage_size(), l.fit, &l.geometry);
        let mesh = bytes(&grid(&q));
        let fx = video_fx_uniform(&l.effects).unwrap_or(NO_FX);
        let color = |c: [f32; 4]| [c[0] * l.tint[0], c[1] * l.tint[1], c[2] * l.tint[2], c[3] * l.tint[3] * l.opacity];
        let mut keys = Vec::new();
        // (plane index, colour, kind, matrix, plane sizes/formats, uploader)
        match &f.planes {
            None => {
                let (kind, matrix, want): (f32, [[f32; 4]; 3], Vec<PlaneSpec>) = match &f.pixels {
                    Pixels::None => return Vec::new(),
                    Pixels::Rgba { premultiplied, .. } => {
                        (if *premultiplied { 0.0 } else { 1.0 }, [[0.0; 4]; 3], vec![([f.width, f.height], wgpu::TextureFormat::Rgba8Unorm)])
                    }
                    Pixels::Picture(p) => {
                        let fr = &p.frame;
                        let size = |i: usize| [fr.plane_width(i), fr.plane_height(i)];
                        let r8 = wgpu::TextureFormat::R8Unorm;
                        match p.layout {
                            PlaneLayout::Rgba => (1.0, p.matrix, vec![(size(0), wgpu::TextureFormat::Rgba8Unorm)]),
                            PlaneLayout::Nv12 => (3.0, p.matrix, vec![(size(0), r8), (size(1), wgpu::TextureFormat::Rg8Unorm)]),
                            PlaneLayout::Planar { alpha } => {
                                let mut v = vec![(size(0), r8), (size(1), r8), (size(2), r8)];
                                if alpha {
                                    v.push((size(3), r8));
                                }
                                (2.0, p.matrix, v)
                            }
                        }
                    }
                };
                let key = (l.id, 0);
                let mut d = self.draws.remove(&key).unwrap_or_else(|| self.new_draw());
                self.ensure_planes(&mut d, &want, mask);
                if d.generation != f.generation {
                    d.generation = f.generation;
                    match &f.pixels {
                        Pixels::Rgba { data, .. } => write_plane(&self.queue, &d.planes[0].0, data, f.width as usize * 4, [f.width, f.height], 4),
                        Pixels::Picture(p) => {
                            for (i, (tex, size, format)) in d.planes.iter().enumerate() {
                                let bpp = format.block_copy_size(None).unwrap_or(1) as usize;
                                write_plane(&self.queue, tex, p.frame.data(i), p.frame.stride(i), *size, bpp);
                            }
                        }
                        Pixels::None => {}
                    }
                }
                let mut u = Vec::with_capacity(LAYER_FLOATS);
                u.extend(color([1.0; 4]));
                let alpha = matches!(&f.pixels, Pixels::Picture(Picture { layout: PlaneLayout::Planar { alpha: true }, .. }));
                u.extend([kind, if alpha { 1.0 } else { 0.0 }, if f.mirror { 1.0 } else { 0.0 }, mask_mode]);
                u.extend(matrix.iter().flatten());
                u.extend(fx);
                self.queue.write_buffer(&d.uniform, 0, &bytes(&u));
                self.queue.write_buffer(&d.vertices, 0, &mesh);
                d.last_used = Instant::now();
                self.draws.insert(key, d);
                keys.push(key);
            }
            Some(p) => {
                let size = [f.width, f.height];
                let n = p.masks.len();
                for k in 0..n + usize::from(p.colour.is_some()) {
                    let (c, kind, format, data, bpp): ([f32; 4], f32, _, &[u8], usize) = if k < n {
                        let (part, mask) = &p.masks[k];
                        (l.text_colors[*part as usize], 4.0, wgpu::TextureFormat::R8Unorm, mask, 1)
                    } else {
                        let rgba = p.colour.as_deref().unwrap_or_default();
                        ([1.0, 1.0, 1.0, l.text_colors[3][3]], 1.0, wgpu::TextureFormat::Rgba8Unorm, rgba, 4)
                    };
                    if c[3] <= 0.0 || data.len() < size[0] as usize * size[1] as usize * bpp {
                        continue;
                    }
                    let key = (l.id, k);
                    let mut d = self.draws.remove(&key).unwrap_or_else(|| self.new_draw());
                    self.ensure_planes(&mut d, &[(size, format)], mask);
                    if d.generation != f.generation {
                        d.generation = f.generation;
                        write_plane(&self.queue, &d.planes[0].0, data, size[0] as usize * bpp, size, bpp);
                    }
                    let mut u = Vec::with_capacity(LAYER_FLOATS);
                    u.extend(color(c));
                    u.extend([kind, 0.0, 0.0, mask_mode]);
                    u.extend([0.0; 12]);
                    u.extend(fx);
                    self.queue.write_buffer(&d.uniform, 0, &bytes(&u));
                    self.queue.write_buffer(&d.vertices, 0, &mesh);
                    d.last_used = Instant::now();
                    self.draws.insert(key, d);
                    keys.push(key);
                }
            }
        }
        keys
    }

    /// Free the pictures of layers no window has drawn for a while.
    pub fn collect_garbage(&mut self) {
        let now = Instant::now();
        self.draws.retain(|_, d| now.duration_since(d.last_used) < DRAW_TTL);
        self.masks.retain(|_, m| now.duration_since(m.last_used) < DRAW_TTL);
    }

    /// The mapping test pattern of `stage`, rasterised at its canvas size (once per look).
    fn ensure_pattern(&self, t: &mut Target, stage: &Stage, crop: [f32; 4], size: [u32; 2]) {
        let key = format!("{}|{}x{}|{size:?}|{crop:?}", stage.name, stage.width, stage.height);
        if t.pattern.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let rgba = match super::pattern::test_pattern(stage, size) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("test pattern: {e:#}");
                vec![255; size[0] as usize * size[1] as usize * 4]
            }
        };
        let mut d = self.new_draw();
        self.ensure_planes(&mut d, &[(size, wgpu::TextureFormat::Rgba8Unorm)], None);
        write_plane(&self.queue, &d.planes[0].0, &rgba, size[0] as usize * 4, size, 4);
        let mut u = vec![1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        u.extend([0.0; 12]);
        u.extend(NO_FX);
        self.queue.write_buffer(&d.uniform, 0, &bytes(&u));
        // The pattern fills this output's canvas: its crop of the source.
        let q = LayerQuad { corners: [[crop[0], crop[1]], [crop[2], crop[1]], [crop[2], crop[3]], [crop[0], crop[3]]], uv: [0.0, 0.0, 1.0, 1.0] };
        self.queue.write_buffer(&d.vertices, 0, &bytes(&grid(&q)));
        t.pattern = Some((key, d));
    }

    /// Draw `scene` into `out` (a window's surface texture or an offscreen picture).
    pub fn render(&mut self, t: &mut Target, scene: &Scene, out: &wgpu::TextureView, format: wgpu::TextureFormat) {
        let crop = crop_of(scene.stage, scene.source);
        let size = canvas_size(scene.stage, scene.source, self.max_side.min(8192));
        if t.canvas.as_ref().is_none_or(|c| c.2 != size) {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("openq-canvas"),
                size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: CANVAS_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = tex.create_view(&Default::default());
            t.canvas = Some((tex, view, size));
        }
        let (mask_mode, mask) = self.mask(&scene.mask);
        if t.warp_made != Some((mask, size)) {
            let canvas = &t.canvas.as_ref().expect("canvas").1;
            t.warp_bind = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("openq-warp"),
                layout: &self.warp_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(canvas) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 2, resource: t.warp_uniform.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(self.mask_view(mask)) },
                ],
            }));
            t.warp_made = Some((mask, size));
        }
        self.queue.write_buffer(&t.view_uniform, 0, &bytes(&crop));
        let inv = Homography::square_to_quad(scene.corners).inverse().unwrap_or([[0.0; 3]; 3]);
        let mut warp: Vec<f32> = inv.iter().flat_map(|r| [r[0], r[1], r[2], 0.0]).collect();
        warp.extend(scene.stage.blend.map(|w| w.clamp(0.0, 1.0)));
        warp.extend([scene.stage.blend_gamma, mask_mode, scene.stage.black_lift.clamp(0.0, 0.25), 0.0]);
        self.queue.write_buffer(&t.warp_uniform, 0, &bytes(&warp));

        if scene.mapping {
            let own = [scene.stage.width.clamp(1, 2048), scene.stage.height.clamp(1, 2048)];
            self.ensure_pattern(t, scene.stage, crop, own);
        }
        let source_canvas = [scene.source.width.max(1) as f32, scene.source.height.max(1) as f32];
        let mut draws: Vec<((CueId, usize), BlendMode)> = Vec::new();
        for l in scene.layers {
            for k in self.prepare_layer(l, source_canvas) {
                draws.push((k, l.blend_mode));
            }
        }
        if scene.mapping {
            self.layer_pipe(BlendMode::Normal);
        }
        for (_, mode) in &draws {
            self.layer_pipe(*mode);
        }
        self.full_pipe(format, false);
        let overlay = scene.overlay.filter(|o| o.size[0] > 0 && o.size[1] > 0 && o.rgba.len() == o.size[0] as usize * o.size[1] as usize * 4);
        if let Some(o) = overlay {
            self.full_pipe(format, true);
            if t.overlay.as_ref().is_none_or(|(g, s, ..)| *g != o.generation || *s != o.size) {
                let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("openq-overlay"),
                    size: wgpu::Extent3d { width: o.size[0], height: o.size[1], depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                write_plane(&self.queue, &tex, &o.rgba, o.size[0] as usize * 4, o.size, 4);
                let view = tex.create_view(&Default::default());
                let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("openq-overlay"),
                    layout: &self.overlay_bgl,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    ],
                });
                t.overlay = Some((o.generation, o.size, tex, bind));
            }
        }

        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("openq-frame") });
        {
            let canvas = &t.canvas.as_ref().expect("canvas").1;
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("openq-canvas"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: canvas,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_index_buffer(self.index.slice(..), wgpu::IndexFormat::Uint32);
            pass.set_bind_group(1, &t.view_bind, &[]);
            let n = (GRID * GRID * 6) as u32;
            if scene.mapping
                && let Some((_, d)) = &t.pattern
                && let Some(bind) = &d.bind
            {
                pass.set_pipeline(&self.layer_pipes[&BlendMode::Normal]);
                pass.set_bind_group(0, bind, &[]);
                pass.set_vertex_buffer(0, d.vertices.slice(..));
                pass.draw_indexed(0..n, 0, 0..1);
            }
            for (key, mode) in &draws {
                let Some(d) = self.draws.get(key) else { continue };
                let Some(bind) = &d.bind else { continue };
                pass.set_pipeline(&self.layer_pipes[mode]);
                pass.set_bind_group(0, bind, &[]);
                pass.set_vertex_buffer(0, d.vertices.slice(..));
                pass.draw_indexed(0..n, 0, 0..1);
            }
        }
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("openq-warp"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: out,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.warp_pipes[&format]);
            pass.set_bind_group(0, t.warp_bind.as_ref().expect("warp bind group"), &[]);
            pass.draw(0..3, 0..1);
            if overlay.is_some()
                && let Some((.., bind)) = &t.overlay
            {
                pass.set_pipeline(&self.overlay_pipes[&format]);
                pass.set_bind_group(0, bind, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
    }

    /// Draw `scene` into a new `size` picture and read it back (RGBA).
    pub fn render_to_image(&mut self, t: &mut Target, scene: &Scene, size: [u32; 2]) -> anyhow::Result<image::RgbaImage> {
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("openq-snapshot"),
            size: wgpu::Extent3d { width: size[0].max(1), height: size[1].max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.render(t, scene, &tex.create_view(&Default::default()), format);
        let (w, h) = (size[0].max(1), size[1].max(1));
        let row = (w * 4).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("openq-snapshot"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv_timeout(Duration::from_secs(10)).context("GPU read timed out")?.context("GPU read failed")?;
        let data = buf.slice(..).get_mapped_range();
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            out.extend_from_slice(&data[y * row as usize..y * row as usize + w as usize * 4]);
        }
        drop(data);
        buf.unmap();
        image::RgbaImage::from_raw(w, h, out).context("bad picture size")
    }
}

/// Upload one plane: `size` pixels of `bpp` bytes, rows `stride` bytes apart in `data`.
fn write_plane(queue: &wgpu::Queue, tex: &wgpu::Texture, data: &[u8], stride: usize, size: [u32; 2], bpp: usize) {
    let (w, h) = (size[0].max(1) as usize, size[1].max(1) as usize);
    let need = stride * (h - 1) + w * bpp;
    if data.len() < need || stride < w * bpp {
        log::warn!("picture plane too small ({} < {need} bytes); not uploaded", data.len());
        return;
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &data[..need],
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride as u32), rows_per_image: Some(h as u32) },
        wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_is_valid() {
        use eframe::wgpu::naga;
        let module = naga::front::wgsl::parse_str(include_str!("shader.wgsl")).expect("WGSL parses");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
            .validate(&module)
            .expect("WGSL validates");
        for entry in ["vs_layer", "fs_black", "fs_white", "vs_full", "fs_warp", "fs_warp_linear", "fs_overlay", "fs_overlay_linear"] {
            assert!(module.entry_points.iter().any(|e| e.name == entry), "{entry}");
        }
    }

    #[test]
    fn modes_that_hide_white_draw_over_white() {
        for m in BlendMode::ALL {
            let (_, white) = blend_state(m);
            assert_eq!(white, matches!(m, BlendMode::Multiply | BlendMode::Darken), "{m:?}");
        }
    }

    /// Draw layers offscreen on the real GPU (skipped without one) and read the pixels back:
    /// RGBA stills, YUV pictures converted in the shader, the corner-pin warp and black outside.
    #[test]
    fn layers_render_on_the_gpu() {
        use crate::media::video::{Frame, FrameSlot, StageLayer};
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let Ok(mut gpu) = Gpu::new(&instance, None) else {
            eprintln!("no GPU: skipped");
            return;
        };
        let stage = Stage { width: 64, height: 32, ..Stage::new("T") };
        let layer = |frame: FrameSlot| StageLayer {
            id: CueId::new_v4(),
            stage: None,
            frame,
            opacity: 1.0,
            geometry: Default::default(),
            layer: 0,
            blend_mode: BlendMode::Normal,
            fit: crate::model::VideoFit::Stretch,
            order: 0,
            tint: [1.0; 4],
            text_colors: [[1.0; 4]; 4],
            animated: false,
            effects: Vec::new(),
            mask: None,
        };
        let green = FrameSlot::default();
        *green.lock() = Frame {
            width: 4,
            height: 4,
            pixels: Pixels::Rgba { data: [0, 255, 0, 255].repeat(16), premultiplied: true },
            generation: 1,
            ..Default::default()
        };
        let layers = [layer(green)];
        let mut t = gpu.target();
        let scene = Scene { stage: &stage, source: &stage, layers: &layers, mapping: false, corners: Stage::IDENTITY, overlay: None, mask: None };
        let img = gpu.render_to_image(&mut t, &scene, [64, 32]).unwrap();
        assert_eq!(img.get_pixel(32, 16).0, [0, 255, 0, 255]);
        // Pinned into the left half: the right half is black.
        let half = Scene { corners: [[0.0, 0.0], [0.5, 0.0], [0.5, 1.0], [0.0, 1.0]], ..scene };
        let img = gpu.render_to_image(&mut t, &half, [64, 32]).unwrap();
        assert_eq!(img.get_pixel(10, 16).0, [0, 255, 0, 255]);
        assert_eq!(img.get_pixel(50, 16).0, [0, 0, 0, 255]);

        // Limited-range BT.601 red as planar YUV 4:2:0, and as NV12.
        use ffmpeg_next::format::Pixel as P;
        crate::media::video::init().unwrap();
        for fmt in [P::YUV420P, P::NV12] {
            let mut f = ffmpeg_next::util::frame::video::Video::new(fmt, 8, 8);
            f.set_color_space(ffmpeg_next::util::color::Space::BT470BG);
            f.data_mut(0).fill(81);
            if fmt == P::NV12 {
                for uv in f.data_mut(1).as_chunks_mut::<2>().0 {
                    *uv = [90, 240];
                }
            } else {
                f.data_mut(1).fill(90);
                f.data_mut(2).fill(240);
            }
            let slot = FrameSlot::default();
            *slot.lock() = Frame { width: 8, height: 8, pixels: Pixels::Picture(Picture::of(&f).unwrap()), generation: 1, ..Default::default() };
            let layers = [layer(slot)];
            let scene = Scene { stage: &stage, source: &stage, layers: &layers, mapping: false, corners: Stage::IDENTITY, overlay: None, mask: None };
            let px = gpu.render_to_image(&mut t, &scene, [64, 32]).unwrap().get_pixel(32, 16).0;
            assert!(px[0] > 245 && px[1] < 10 && px[2] < 10, "{fmt:?}: {px:?}");
        }
    }

    /// A layer's mask hides where it is black; an output's inverted mask hides where it is white;
    /// a mask still loading hides the layer.
    #[test]
    fn masks_hide_parts_of_layers_and_outputs() {
        use crate::media::video::{Frame, FrameSlot};
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let Ok(mut gpu) = Gpu::new(&instance, None) else {
            eprintln!("no GPU: skipped");
            return;
        };
        let stage = Stage { width: 64, height: 32, ..Stage::new("T") };
        let green = FrameSlot::default();
        *green.lock() = Frame { width: 4, height: 4, pixels: Pixels::Rgba { data: [0, 255, 0, 255].repeat(16), premultiplied: true }, generation: 1, ..Default::default() };
        // Left half white, right half black.
        let half = MaskState::Ready(Arc::new(MaskImage { size: [2, 1], data: vec![255, 0] }));
        let layer = |mask| StageLayer {
            id: CueId::new_v4(),
            stage: None,
            frame: green.clone(),
            opacity: 1.0,
            geometry: Default::default(),
            layer: 0,
            blend_mode: BlendMode::Normal,
            fit: crate::model::VideoFit::Stretch,
            order: 0,
            tint: [1.0; 4],
            text_colors: [[1.0; 4]; 4],
            animated: false,
            effects: Vec::new(),
            mask,
        };
        let mut t = gpu.target();
        let mut draw = |layers: &[StageLayer], mask| {
            let scene = Scene { stage: &stage, source: &stage, layers, mapping: false, corners: Stage::IDENTITY, overlay: None, mask };
            let img = gpu.render_to_image(&mut t, &scene, [64, 32]).unwrap();
            (img.get_pixel(8, 16).0, img.get_pixel(56, 16).0)
        };
        let (on, off) = ([0, 255, 0, 255], [0, 0, 0, 255]);
        assert_eq!(draw(&[layer(Some((half.clone(), false)))], None), (on, off));
        assert_eq!(draw(&[layer(Some((half.clone(), true)))], None), (off, on));
        assert_eq!(draw(&[layer(None)], Some((half.clone(), true))), (off, on), "output mask, inverted");
        assert_eq!(draw(&[layer(Some((MaskState::Loading, false)))], None), (off, off), "hidden while loading");
        assert_eq!(draw(&[layer(Some((MaskState::Failed("gone".into()), false)))], None), (on, on), "a broken mask hides nothing");
    }

    /// Black is lifted outside the blend zones only.
    #[test]
    fn black_lift_spares_the_blend_zones() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let Ok(mut gpu) = Gpu::new(&instance, None) else {
            eprintln!("no GPU: skipped");
            return;
        };
        let stage = Stage { width: 64, height: 32, blend: [0.25, 0.0, 0.0, 0.0], black_lift: 0.1, ..Stage::new("T") };
        let mut t = gpu.target();
        let scene = Scene { stage: &stage, source: &stage, layers: &[], mapping: false, corners: Stage::IDENTITY, overlay: None, mask: None };
        let img = gpu.render_to_image(&mut t, &scene, [64, 32]).unwrap();
        assert_eq!(img.get_pixel(4, 16).0, [0, 0, 0, 255], "in the blend zone");
        let lifted = img.get_pixel(48, 16).0;
        assert!((24..=28).contains(&lifted[0]) && lifted[0] == lifted[2], "{lifted:?}");
    }

    #[test]
    fn canvas_sizes_follow_the_crop() {
        let src = Stage { width: 3840, height: 1080, ..Stage::new("Wide") };
        let mut left = Stage { span_of: Some(src.id), crop: [0.0, 0.0, 0.5, 1.0], ..Stage::new("Left") };
        assert_eq!(canvas_size(&left, &src, 8192), [1920, 1080]);
        assert_eq!(canvas_size(&src, &src, 8192), [3840, 1080]);
        assert_eq!(canvas_size(&src, &src, 2048), [2048, 1080]);
        left.crop = [0.5, 0.0, 0.5, 1.0];
        assert_eq!(canvas_size(&left, &src, 8192), [1, 1080]);
    }
}
