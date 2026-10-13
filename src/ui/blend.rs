//! Layer blend modes and video effects on stage windows.
//!
//! egui draws every mesh with premultiplied "over" blending, so a layer with another
//! [`BlendMode`] or with video effects is drawn through a wgpu paint callback instead: the same
//! meshes and the same egui textures, with a pipeline whose fixed-function blend state does the
//! mode. Modes that need the layer "over white" (Multiply, Darken) get that from the shader, so
//! opacity still fades them out evenly. Like egui itself, blending happens on the gamma-encoded
//! values.
//!
//! Video effects ([`crate::effects::VideoEffect`]) run in the same fragment shader from a
//! per-layer uniform ([`crate::effects::video_fx_uniform`]): pixellate, blur and sharpen change
//! how the texture is sampled, then the colour effects run in list order on the un-premultiplied
//! colour (after the layer's tint and opacity).

use crate::effects::{VIDEO_FX_FLOATS, VideoEffect, video_fx_uniform};
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

struct Fx {
    // pixel size, blur radius, sharpen amount, sharpen radius (source pixels)
    spatial: vec4<f32>,
    // x = number of colour effects
    count: vec4<f32>,
    // per colour effect: (code, value 0, value 1, value 2), (red, green, blue, 0)
    slots: array<vec4<f32>, 16>,
};
@group(1) @binding(0) var<uniform> fx: Fx;

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn chroma(c: vec3<f32>) -> vec2<f32> {
    return vec2<f32>(dot(c, vec3<f32>(-0.1146, -0.3854, 0.5)), dot(c, vec3<f32>(0.5, -0.4542, -0.0458)));
}

fn hue_rotate(c: vec3<f32>, degrees: f32) -> vec3<f32> {
    let a = radians(degrees);
    let k = vec3<f32>(0.57735027);
    let ca = cos(a);
    return c * ca + cross(k, c) * sin(a) + k * dot(k, c) * (1.0 - ca);
}

// The texture with the spatial effects (pixellate, blur, sharpen); premultiplied.
fn sample_fx(uv0: vec2<f32>) -> vec4<f32> {
    // Gradients taken up front (uniform control flow) keep mipmapped textures filtering right.
    let gx = dpdx(uv0);
    let gy = dpdy(uv0);
    let dims = vec2<f32>(textureDimensions(tex));
    var uv = uv0;
    let px = fx.spatial.x;
    if (px > 1.0) {
        uv = (floor(uv * dims / px) + 0.5) * px / dims;
    }
    var c = textureSampleGrad(tex, samp, uv, gx, gy);
    let r = fx.spatial.y;
    if (r > 0.25) {
        // Golden-angle spiral of taps with Gaussian weights (sigma = radius / 2).
        var acc = c;
        var total = 1.0;
        for (var k = 1; k < 48; k++) {
            let f = f32(k) / 47.0;
            let a = f32(k) * 2.39996323;
            let off = vec2<f32>(cos(a), sin(a)) * r * sqrt(f) / dims;
            let w = exp(-2.0 * f);
            acc += textureSampleGrad(tex, samp, uv + off, gx, gy) * w;
            total += w;
        }
        c = acc / total;
    }
    let sh = fx.spatial.z;
    if (sh > 0.0) {
        let o = fx.spatial.w / dims;
        let around = (textureSampleGrad(tex, samp, uv + vec2<f32>(o.x, 0.0), gx, gy)
            + textureSampleGrad(tex, samp, uv - vec2<f32>(o.x, 0.0), gx, gy)
            + textureSampleGrad(tex, samp, uv + vec2<f32>(0.0, o.y), gx, gy)
            + textureSampleGrad(tex, samp, uv - vec2<f32>(0.0, o.y), gx, gy)) * 0.25;
        c = clamp(c + (c - around) * sh, vec4<f32>(0.0), vec4<f32>(1.0));
        c = vec4<f32>(min(c.rgb, vec3<f32>(c.a)), c.a);
    }
    return c;
}

// The colour effects, in order, on a premultiplied colour.
fn colour_fx(cin: vec4<f32>, uv: vec2<f32>) -> vec4<f32> {
    let n = i32(fx.count.x + 0.5);
    if (n == 0) {
        return cin;
    }
    var a = cin.a;
    var rgb = select(vec3<f32>(0.0), cin.rgb / max(a, 0.0001), a > 0.0001);
    for (var k = 0; k < 8; k++) {
        if (k >= n) {
            break;
        }
        let h = fx.slots[k * 2];
        let col = fx.slots[k * 2 + 1].rgb;
        switch i32(h.x + 0.5) {
            case 1: {
                rgb = (rgb - 0.5) * h.z + 0.5 + h.y;
                rgb = mix(vec3<f32>(luma(rgb)), rgb, h.w);
            }
            case 2: {
                rgb = pow(max(rgb * exp2(h.y), vec3<f32>(0.0)), vec3<f32>(1.0 / max(h.z, 0.01)));
            }
            case 3: {
                rgb = hue_rotate(rgb, h.y);
            }
            case 4: {
                rgb = mix(rgb, col * luma(rgb), h.y);
            }
            case 5: {
                let s = vec3<f32>(dot(rgb, vec3<f32>(0.393, 0.769, 0.189)), dot(rgb, vec3<f32>(0.349, 0.686, 0.168)), dot(rgb, vec3<f32>(0.272, 0.534, 0.131)));
                rgb = mix(rgb, s, h.y);
            }
            case 6: {
                rgb = mix(rgb, 1.0 - rgb, h.y);
            }
            case 7: {
                let levels = max(h.y - 1.0, 1.0);
                rgb = round(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * levels) / levels;
            }
            case 8: {
                rgb = mix(rgb, vec3<f32>(step(h.y, luma(rgb))), h.z);
            }
            case 9: {
                let d = length(uv - vec2<f32>(0.5)) * 2.0;
                rgb = rgb * (1.0 - h.w * smoothstep(h.y, h.y + h.z, d));
            }
            case 10: {
                let d = distance(chroma(rgb), chroma(col));
                let lo = h.y * 0.5;
                let hi = lo + h.z * 0.5 + 0.0001;
                a = a * smoothstep(lo, hi, d);
                // Spill: take the key colour's cast off the edges that stay.
                rgb = mix(rgb, vec3<f32>(luma(rgb)), h.w * (1.0 - smoothstep(lo, hi + 0.15, d)));
            }
            default: {}
        }
        rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    return vec4<f32>(rgb * a, a);
}

fn linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c < vec3<f32>(0.04045));
}

// Premultiplied layer colour: over black (Add, Screen, Lighten, Subtract) or over white
// (Multiply, Darken), so a transparent pixel leaves the stage unchanged either way.
fn over_black(i: Out) -> vec4<f32> {
    return colour_fx(i.color * sample_fx(i.uv), i.uv);
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

/// The effect uniform for a layer without effects.
const NO_FX: [f32; VIDEO_FX_FLOATS] = {
    let mut u = [0.0; VIDEO_FX_FLOATS];
    u[0] = 1.0;
    u[3] = 0.5;
    u
};

/// Pipelines per blend mode, kept in the renderer's callback resources.
struct Pipelines {
    format: wgpu::TextureFormat,
    fx_layout: wgpu::BindGroupLayout,
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
        let fx_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("openq_layer_fx"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("openq_blend"),
            bind_group_layouts: &[Some(&bgl), Some(&fx_layout)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("openq_blend"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        Self { format, fx_layout, layout, module, by_mode: HashMap::new() }
    }

    fn get(&mut self, device: &wgpu::Device, mode: BlendMode) -> &wgpu::RenderPipeline {
        let Self { format, layout, module, by_mode, .. } = self;
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

/// One layer's meshes drawn with a blend mode and effects.
struct BlendCallback {
    mode: BlendMode,
    format: wgpu::TextureFormat,
    /// The layer's effect uniform.
    fx: [f32; VIDEO_FX_FLOATS],
    /// Interleaved vertices: clip-space x, y, texture u, v, colour (as f32 bits).
    vertices: Vec<u32>,
    indices: Vec<u32>,
    /// Texture bind group and index range of each mesh.
    draws: Vec<(wgpu::BindGroup, Range<u32>)>,
    /// Vertex and index buffers, and the effect uniform's bind group (made in `prepare`).
    buffers: Mutex<Option<(wgpu::Buffer, wgpu::Buffer, wgpu::BindGroup)>>,
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
        let ub = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("openq_layer_fx"),
            contents: &bytes(&self.fx.map(f32::to_bits)),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let fx = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("openq_layer_fx"),
            layout: &p.fx_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: ub.as_entire_binding() }],
        });
        *self.buffers.lock() = Some((vb, ib, fx));
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(p) = resources.get::<Pipelines>() else { return };
        let Some(pipeline) = p.by_mode.get(&self.mode) else { return };
        let buffers = self.buffers.lock();
        let Some((vb, ib, fx)) = buffers.as_ref() else { return };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(1, fx, &[]);
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
    /// Draw the meshes as they are (Normal mode without effects, or no wgpu renderer, e.g.
    /// headless tests).
    Plain(Vec<Shape>),
    /// A texture isn't on the GPU yet (first frame of a layer): draw nothing this frame, rather
    /// than a wrongly blended picture.
    NotReady,
}

/// Wrap a layer's meshes so they draw with `mode` and `effects` (see [`Blended`]).
pub(super) fn blend(gpu: Option<&RenderState>, mode: BlendMode, effects: &[VideoEffect], shapes: Vec<Shape>, rect: Rect) -> Blended {
    let fx = video_fx_uniform(effects);
    let Some(gpu) = gpu.filter(|_| (mode != BlendMode::Normal || fx.is_some()) && rect.width() > 0.0 && rect.height() > 0.0) else {
        return Blended::Plain(shapes);
    };
    let renderer = gpu.renderer.read();
    let mut cb = BlendCallback {
        mode,
        format: gpu.target_format,
        fx: fx.unwrap_or(NO_FX),
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
