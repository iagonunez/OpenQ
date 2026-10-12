//! Layer blend modes on stage windows.
//!
//! egui draws every mesh with premultiplied "over" blending, so a layer with another
//! [`BlendMode`] is drawn through a wgpu paint callback instead: the same meshes and the same egui
//! textures, with a pipeline whose fixed-function blend state does the mode. Modes that need the
//! layer "over white" (Multiply, Darken) get that from the shader, so opacity still fades them
//! out evenly. Like egui itself, blending happens on the gamma-encoded values.

use crate::model::BlendMode;
use eframe::egui_wgpu::{self, CallbackResources, CallbackTrait, RenderState, ScreenDescriptor};
use eframe::wgpu;
use eframe::wgpu::util::DeviceExt;
use egui::{Rect, Shape};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ops::Range;

const SHADER: &str = r#"
struct Out {
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @builtin(position) pos: vec4<f32>,
};

fn unpack(c: u32) -> vec4<f32> {
    return vec4<f32>(f32(c & 255u), f32((c >> 8u) & 255u), f32((c >> 16u) & 255u), f32((c >> 24u) & 255u)) / 255.0;
}

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) color: u32) -> Out {
    var o: Out;
    o.uv = uv;
    o.color = unpack(color);
    o.pos = vec4<f32>(pos, 0.0, 1.0);
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

fn linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c < vec3<f32>(0.04045));
}

// Premultiplied layer colour: over black (Add, Screen, Lighten, Subtract) or over white
// (Multiply, Darken), so a transparent pixel leaves the stage unchanged either way.
fn over_black(i: Out) -> vec4<f32> {
    return i.color * textureSample(tex, samp, i.uv);
}
fn over_white(i: Out) -> vec4<f32> {
    let c = over_black(i);
    return vec4<f32>(c.rgb + vec3<f32>(1.0 - c.a), c.a);
}
fn to_linear(c: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(linear(c.rgb), c.a);
}

@fragment
fn fs_black(i: Out) -> @location(0) vec4<f32> { return over_black(i); }
@fragment
fn fs_white(i: Out) -> @location(0) vec4<f32> { return over_white(i); }
@fragment
fn fs_black_linear(i: Out) -> @location(0) vec4<f32> { return to_linear(over_black(i)); }
@fragment
fn fs_white_linear(i: Out) -> @location(0) vec4<f32> { return to_linear(over_white(i)); }
"#;

/// Fixed-function blending for `mode` and whether the shader outputs the layer over white.
fn blend_state(mode: BlendMode) -> (wgpu::BlendComponent, bool) {
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

/// Pipelines per blend mode, kept in the renderer's callback resources.
struct Pipelines {
    format: wgpu::TextureFormat,
    layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    by_mode: HashMap<BlendMode, wgpu::RenderPipeline>,
}

impl Pipelines {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        // Structurally the same as egui's texture layout, so egui's bind groups fit it.
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("openq_blend_texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
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
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("openq_blend"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("openq_blend"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        Self { format, layout, module, by_mode: HashMap::new() }
    }

    fn get(&mut self, device: &wgpu::Device, mode: BlendMode) -> &wgpu::RenderPipeline {
        let Self { format, layout, module, by_mode } = self;
        by_mode.entry(mode).or_insert_with(|| {
            let (color, white) = blend_state(mode);
            let entry = match (white, format.is_srgb()) {
                (false, false) => "fs_black",
                (true, false) => "fs_white",
                (false, true) => "fs_black_linear",
                (true, true) => "fs_white_linear",
            };
            // Keep the window's alpha as it is.
            let alpha = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::Zero, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add };
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("openq_blend"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_main"),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: 5 * 4,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Uint32],
                    }],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
                // eframe's defaults: no depth buffer, no multisampling.
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: *format,
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
}

/// One layer's meshes drawn with a blend mode.
struct BlendCallback {
    mode: BlendMode,
    format: wgpu::TextureFormat,
    /// Interleaved vertices: clip-space x, y, texture u, v, colour (as f32 bits).
    vertices: Vec<u32>,
    indices: Vec<u32>,
    /// Texture bind group and index range of each mesh.
    draws: Vec<(wgpu::BindGroup, Range<u32>)>,
    buffers: Mutex<Option<(wgpu::Buffer, wgpu::Buffer)>>,
}

impl CallbackTrait for BlendCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let p = resources.entry::<Pipelines>().or_insert_with(|| Pipelines::new(device, self.format));
        p.get(device, self.mode);
        let bytes = |v: &[u32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("openq_blend_vertices"),
            contents: &bytes(&self.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("openq_blend_indices"),
            contents: &bytes(&self.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        *self.buffers.lock() = Some((vb, ib));
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(p) = resources.get::<Pipelines>() else { return };
        let Some(pipeline) = p.by_mode.get(&self.mode) else { return };
        let buffers = self.buffers.lock();
        let Some((vb, ib)) = buffers.as_ref() else { return };
        pass.set_pipeline(pipeline);
        pass.set_vertex_buffer(0, vb.slice(..));
        pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
        for (bind, range) in &self.draws {
            pass.set_bind_group(0, bind, &[]);
            pass.draw_indexed(range.clone(), 0, 0..1);
        }
    }
}

/// What to draw for a layer whose meshes are `shapes`, painted in the window area `rect`.
pub(super) enum Blended {
    /// Draw this instead (the meshes in a blend callback).
    Shape(Shape),
    /// Draw the meshes as they are (Normal mode, or no wgpu renderer, e.g. headless tests).
    Plain(Vec<Shape>),
    /// A texture isn't on the GPU yet (first frame of a layer): draw nothing this frame, rather
    /// than a wrongly blended picture.
    NotReady,
}

/// Wrap a layer's meshes so they draw with `mode` (see [`Blended`]).
pub(super) fn blend(gpu: Option<&RenderState>, mode: BlendMode, shapes: Vec<Shape>, rect: Rect) -> Blended {
    let Some(gpu) = gpu.filter(|_| mode != BlendMode::Normal && rect.width() > 0.0 && rect.height() > 0.0) else {
        return Blended::Plain(shapes);
    };
    let renderer = gpu.renderer.read();
    let mut cb = BlendCallback {
        mode,
        format: gpu.target_format,
        vertices: Vec::new(),
        indices: Vec::new(),
        draws: Vec::new(),
        buffers: Mutex::new(None),
    };
    for s in &shapes {
        let Shape::Mesh(m) = s else { continue };
        let Some(t) = renderer.texture(&m.texture_id) else { return Blended::NotReady };
        let base = (cb.vertices.len() / 5) as u32;
        for v in &m.vertices {
            // The callback's viewport is `rect`, so positions go to clip space relative to it.
            let x = 2.0 * (v.pos.x - rect.left()) / rect.width() - 1.0;
            let y = 1.0 - 2.0 * (v.pos.y - rect.top()) / rect.height();
            let c = v.color.to_array();
            cb.vertices.extend([x.to_bits(), y.to_bits(), v.uv.x.to_bits(), v.uv.y.to_bits(), u32::from_le_bytes(c)]);
        }
        let start = cb.indices.len() as u32;
        cb.indices.extend(m.indices.iter().map(|i| i + base));
        cb.draws.push((t.bind_group.clone(), start..cb.indices.len() as u32));
    }
    drop(renderer);
    if cb.draws.is_empty() {
        return Blended::Plain(shapes);
    }
    Blended::Shape(Shape::Callback(egui_wgpu::Callback::new_paint_callback(rect, cb)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_is_valid() {
        use eframe::wgpu::naga;
        let module = naga::front::wgsl::parse_str(SHADER).expect("WGSL parses");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
            .validate(&module)
            .expect("WGSL validates");
        for entry in ["vs_main", "fs_black", "fs_white", "fs_black_linear", "fs_white_linear"] {
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
}
