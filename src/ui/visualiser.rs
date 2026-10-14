//! The 3D lighting visualiser: fixtures, beams and the light they throw on the floor, drawn
//! from the latest DMX frame.
//!
//! Rendering happens in an egui paint callback: `prepare` draws the scene into an offscreen
//! HDR colour + depth target with its own pipelines (floor with grid and pools of light, fixture
//! bodies, additive beam cones), `paint` copies it into the panel with a tone curve. Nothing here
//! touches the show while drawing: the callback gets plain arrays. Without a GPU (headless) the
//! page says so. Picking and aiming run on the CPU from the same camera.

use super::{App, theme};
use crate::i18n::t;
use crate::light::Lighting;
use crate::light::model::FixtureId;
use crate::light::visual::{self, FixtureLook, M3, V3, v3};
use eframe::egui_wgpu::{self, CallbackResources, CallbackTrait, ScreenDescriptor};
use eframe::wgpu;
use eframe::wgpu::util::DeviceExt;
use egui::{Color32, RichText, Ui};

/// Lights (beams) that light the floor, at most.
const MAX_LIGHTS: usize = 256;
/// Floor half size, metres.
const FLOOR: f32 = 20.0;
const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

const SHADER: &str = r#"
struct Globals { vp: mat4x4<f32>, eye: vec4<f32>, params: vec4<f32> };
struct Light { pos: vec4<f32>, dir: vec4<f32>, color: vec4<f32> };
struct Lights { l: array<Light, 256> };
@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var<uniform> lights: Lights;

// pos.w = cos of the outer half angle, color.w = cos of the inner (full) half angle.
fn spot(l: Light, p: vec3<f32>, n: vec3<f32>) -> vec3<f32> {
    let to = p - l.pos.xyz;
    let d = max(length(to), 0.001);
    let v = to / d;
    let s = smoothstep(l.pos.w, l.color.w, dot(v, l.dir.xyz));
    let ndl = max(dot(n, -v), 0.0);
    return l.color.rgb * s * ndl * (6.0 / (1.0 + d * d * 0.12));
}

struct FOut { @builtin(position) pos: vec4<f32>, @location(0) world: vec3<f32> };

@vertex
fn floor_vs(@builtin(vertex_index) i: u32) -> FOut {
    var c = array<vec2<f32>, 6>(vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(1.0, 1.0), vec2(-1.0, -1.0), vec2(1.0, 1.0), vec2(-1.0, 1.0));
    let w = vec3<f32>(c[i].x * g.params.z, 0.0, c[i].y * g.params.z);
    var o: FOut;
    o.pos = g.vp * vec4<f32>(w, 1.0);
    o.world = w;
    return o;
}

@fragment
fn floor_fs(i: FOut) -> @location(0) vec4<f32> {
    let gc = i.world.xz;
    let gw = max(fwidth(gc), vec2<f32>(0.0001));
    let gl = abs(fract(gc - 0.5) - 0.5) / gw;
    let line = 1.0 - min(min(gl.x, gl.y), 1.0);
    let axis = 1.0 - min(min(abs(gc.x) / gw.x, abs(gc.y) / gw.y), 1.0);
    var col = vec3<f32>(0.012) + vec3<f32>(0.022) * line + vec3<f32>(0.03, 0.03, 0.05) * axis;
    let n = u32(g.params.y);
    for (var k = 0u; k < n; k++) {
        col += spot(lights.l[k], i.world, vec3<f32>(0.0, 1.0, 0.0)) * 0.12;
    }
    return vec4<f32>(col, 1.0);
}

struct BIn {
    @location(0) pos: vec3<f32>,
    @location(1) nrm: vec3<f32>,
    @location(2) m0: vec4<f32>,
    @location(3) m1: vec4<f32>,
    @location(4) m2: vec4<f32>,
    @location(5) m3: vec4<f32>,
    @location(6) color: vec4<f32>,
    @location(7) emit: vec4<f32>,
};
struct BOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) n: vec3<f32>,
    @location(1) color: vec3<f32>,
    @location(2) emit: vec3<f32>,
    @location(3) lens: f32,
};

@vertex
fn box_vs(v: BIn) -> BOut {
    let m = mat4x4<f32>(v.m0, v.m1, v.m2, v.m3);
    var o: BOut;
    o.pos = g.vp * (m * vec4<f32>(v.pos, 1.0));
    o.n = normalize((m * vec4<f32>(v.nrm, 0.0)).xyz);
    o.color = v.color.rgb;
    o.emit = v.emit.rgb;
    // The bottom face (local -y) is the lens.
    o.lens = select(0.0, 1.0, v.nrm.y < -0.5);
    return o;
}

@fragment
fn box_fs(i: BOut) -> @location(0) vec4<f32> {
    let key = normalize(vec3<f32>(0.4, 1.0, 0.7));
    let diff = max(dot(normalize(i.n), key), 0.0) * 0.7 + 0.3;
    return vec4<f32>(i.color * diff + i.emit * i.lens, 1.0);
}

struct CIn {
    @location(0) radial: vec2<f32>,
    @location(1) t: f32,
    @location(2) m0: vec4<f32>,
    @location(3) m1: vec4<f32>,
    @location(4) m2: vec4<f32>,
    @location(5) m3: vec4<f32>,
    @location(6) color: vec4<f32>,
    @location(7) size: vec4<f32>,
};
struct COut {
    @builtin(position) pos: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) n: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) t: f32,
    @location(4) len: f32,
};

@vertex
fn beam_vs(v: CIn) -> COut {
    let m = mat4x4<f32>(v.m0, v.m1, v.m2, v.m3);
    let local = vec3<f32>(v.radial.x * v.t, -v.t, v.radial.y * v.t);
    let w = m * vec4<f32>(local, 1.0);
    // Normal of the scaled cone side: S^-1 of the unit cone's, then the rotation.
    let r = v.size.x;
    let l = v.size.y;
    let nl = vec3<f32>(v.radial.x / r, 1.0 / l, v.radial.y / r);
    let n = normalize(v.m0.xyz / r * nl.x + v.m1.xyz / l * nl.y + v.m2.xyz / r * nl.z);
    var o: COut;
    o.pos = g.vp * w;
    o.world = w.xyz;
    o.n = n;
    o.color = v.color.rgb;
    o.t = v.t;
    o.len = l;
    return o;
}

@fragment
fn beam_fs(i: COut) -> @location(0) vec4<f32> {
    let view = normalize(g.eye.xyz - i.world);
    // Brightest through the middle of the cone, fading to its edges and along its length.
    let facing = abs(dot(normalize(i.n), view));
    let core = pow(facing, 2.5);
    let fall = 1.0 / (1.0 + i.t * i.len * 0.25);
    // Both sides of the cone add up, so each gives half.
    let a = g.params.x * core * fall * 0.06;
    return vec4<f32>(i.color * a, 0.0);
}

struct QOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;

@vertex
fn blit_vs(@builtin(vertex_index) i: u32) -> QOut {
    let x = f32(i32(i & 1u) * 4 - 1);
    let y = f32(i32(i >> 1u) * 4 - 1);
    var o: QOut;
    o.pos = vec4<f32>(x, y, 0.0, 1.0);
    o.uv = vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5);
    return o;
}

fn tone(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(1.0) - exp(-c * 1.4);
}

@fragment
fn blit_fs(i: QOut) -> @location(0) vec4<f32> {
    // A gamma-encoded target (egui's usual): encode here.
    return vec4<f32>(pow(tone(textureSample(hdr, smp, i.uv).rgb), vec3<f32>(1.0 / 2.2)), 1.0);
}

@fragment
fn blit_fs_linear(i: QOut) -> @location(0) vec4<f32> {
    return vec4<f32>(tone(textureSample(hdr, smp, i.uv).rgb), 1.0);
}
"#;

/// Column-major 4×4 matrix.
type M4 = [f32; 16];

fn mul4(a: &M4, b: &M4) -> M4 {
    let mut r = [0.0; 16];
    for c in 0..4 {
        for row in 0..4 {
            r[c * 4 + row] = (0..4).map(|k| a[k * 4 + row] * b[c * 4 + k]).sum();
        }
    }
    r
}

fn perspective(fovy: f32, aspect: f32, near: f32, far: f32) -> M4 {
    let f = 1.0 / (fovy / 2.0).tan();
    let nf = near - far;
    [f / aspect, 0.0, 0.0, 0.0, 0.0, f, 0.0, 0.0, 0.0, 0.0, far / nf, -1.0, 0.0, 0.0, near * far / nf, 0.0]
}

fn look_at(eye: V3, target: V3, up: V3) -> M4 {
    let f = v3::norm(v3::sub(target, eye));
    let s = v3::norm(v3::cross(f, up));
    let u = v3::cross(s, f);
    [s[0], u[0], -f[0], 0.0, s[1], u[1], -f[1], 0.0, s[2], u[2], -f[2], 0.0, -v3::dot(s, eye), -v3::dot(u, eye), v3::dot(f, eye), 1.0]
}

/// Model matrix: rotation `r` (rows), per-axis scale, translation.
fn model(r: &M3, scale: V3, t: V3) -> M4 {
    let mut m = [0.0; 16];
    for j in 0..3 {
        for i in 0..3 {
            m[j * 4 + i] = r[i][j] * scale[j];
        }
    }
    m[12] = t[0];
    m[13] = t[1];
    m[14] = t[2];
    m[15] = 1.0;
    m
}

/// A rotation whose local -y is `dir`.
fn along(dir: V3) -> M3 {
    let y = v3::scale(v3::norm(dir), -1.0);
    let helper = if y[1].abs() < 0.9 { [0.0, 1.0, 0.0] } else { [1.0, 0.0, 0.0] };
    let x = v3::norm(v3::cross(helper, y));
    let z = v3::cross(x, y);
    // Columns x, y, z → rows.
    [[x[0], y[0], z[0]], [x[1], y[1], z[1]], [x[2], y[2], z[2]]]
}

/// Orbit camera around a target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Camera {
    /// Around the vertical axis, radians (0 = from the audience).
    pub yaw: f32,
    /// Above the floor, radians.
    pub pitch: f32,
    /// Distance from the target, metres.
    pub dist: f32,
    /// Point looked at.
    pub target: V3,
}

impl Default for Camera {
    fn default() -> Self {
        Self { yaw: 0.35, pitch: 0.38, dist: 18.0, target: [0.0, 2.5, 0.0] }
    }
}

impl Camera {
    fn eye(&self) -> V3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        v3::add(self.target, [self.dist * cp * sy, self.dist * sp, self.dist * cp * cy])
    }

    fn view_proj(&self, aspect: f32) -> M4 {
        mul4(&perspective(45f32.to_radians(), aspect, 0.1, 200.0), &look_at(self.eye(), self.target, [0.0, 1.0, 0.0]))
    }

    /// Screen position (in `rect`) of a point, if in front of the camera.
    fn project(&self, rect: egui::Rect, p: V3) -> Option<egui::Pos2> {
        let m = self.view_proj(rect.aspect_ratio());
        let c = [0, 1, 2, 3].map(|r| m[r] * p[0] + m[4 + r] * p[1] + m[8 + r] * p[2] + m[12 + r]);
        if c[3] <= 0.01 {
            return None;
        }
        let (x, y) = (c[0] / c[3], c[1] / c[3]);
        Some(egui::pos2(rect.left() + (x * 0.5 + 0.5) * rect.width(), rect.top() + (0.5 - y * 0.5) * rect.height()))
    }

    /// Ray through a screen position: (origin, unit direction).
    fn ray(&self, rect: egui::Rect, pos: egui::Pos2) -> (V3, V3) {
        let eye = self.eye();
        let f = v3::norm(v3::sub(self.target, eye));
        let s = v3::norm(v3::cross(f, [0.0, 1.0, 0.0]));
        let u = v3::cross(s, f);
        let h = (45f32.to_radians() / 2.0).tan();
        let x = ((pos.x - rect.left()) / rect.width() * 2.0 - 1.0) * h * rect.aspect_ratio();
        let y = (1.0 - (pos.y - rect.top()) / rect.height() * 2.0) * h;
        (eye, v3::norm(v3::add(f, v3::add(v3::scale(s, x), v3::scale(u, y)))))
    }
}

/// Visualiser UI state.
pub(crate) struct VisUi {
    pub(crate) camera: Camera,
    /// Haze: how visible beams are, 0..=1.
    haze: f32,
    labels: bool,
    /// Clicks on the floor aim the selected moving heads there.
    aim: bool,
}

impl Default for VisUi {
    fn default() -> Self {
        Self { camera: Camera::default(), haze: 0.5, labels: true, aim: false }
    }
}

/// One frame of the scene for the GPU.
struct Scene {
    globals: Vec<f32>,
    lights: Vec<f32>,
    boxes: Vec<f32>,
    beams: Vec<f32>,
    size: [u32; 2],
}

const BOX_FLOATS: usize = 24;
const INSTANCE: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4, 7 => Float32x4];
const BEAM_FLOATS: usize = 24;

impl Scene {
    fn build(looks: &[FixtureLook], selected: &[FixtureId], cam: &Camera, aspect: f32, haze: f32, size: [u32; 2]) -> Self {
        let mut boxes = Vec::with_capacity(looks.len() * 2 * BOX_FLOATS);
        let mut beams = Vec::new();
        let mut lights = vec![0.0f32; MAX_LIGHTS * 12];
        let mut n_lights = 0;
        let push = |out: &mut Vec<f32>, m: &M4, a: [f32; 4], b: [f32; 4]| {
            out.extend_from_slice(m);
            out.extend_from_slice(&a);
            out.extend_from_slice(&b);
        };
        for f in looks {
            let sel = selected.contains(&f.id);
            let body = if sel { [0.18, 0.32, 0.75, 1.0] } else { [0.16, 0.16, 0.18, 1.0] };
            let lit = v3::scale(f.color, f.intensity * 4.0);
            let emit = [lit[0], lit[1], lit[2], 0.0];
            if f.moving {
                // Base above, head below it turned by pan/tilt.
                let base = model(&f.mount, [0.4, 0.12, 0.4], f.position);
                push(&mut boxes, &base, body, [0.0; 4]);
                let at = v3::add(f.position, visual::mul(&f.mount, [0.0, -0.28, 0.0]));
                let head = model(&visual::mul_m(&f.mount, &f.head), [0.26, 0.34, 0.26], at);
                push(&mut boxes, &head, body, emit);
            } else {
                // A can whose lens faces along the beam.
                let m = model(&along(f.dir), [0.3, 0.42, 0.3], f.position);
                push(&mut boxes, &m, body, emit);
            }
            if f.intensity <= 0.003 {
                continue;
            }
            let origin = if f.moving { v3::add(f.position, visual::mul(&f.mount, [0.0, -0.28, 0.0])) } else { f.position };
            let len = visual::floor_hit(origin, f.dir).map_or(25.0, |h| v3::len(v3::sub(h, origin)).min(40.0));
            let half = (f.angle * 0.5).to_radians();
            let radius = len * half.tan();
            let m = model(&along(f.dir), [radius, len, radius], origin);
            push(&mut beams, &m, [lit[0], lit[1], lit[2], 1.0], [radius, len, 0.0, 0.0]);
            if n_lights < MAX_LIGHTS {
                let l = &mut lights[n_lights * 12..n_lights * 12 + 12];
                l[..3].copy_from_slice(&origin);
                l[3] = (half * 1.08).cos();
                l[4..7].copy_from_slice(&f.dir);
                l[7] = len;
                l[8..11].copy_from_slice(&lit);
                l[11] = (half * 0.8).cos();
                n_lights += 1;
            }
        }
        let mut globals = Vec::with_capacity(24);
        globals.extend_from_slice(&cam.view_proj(aspect));
        let eye = cam.eye();
        globals.extend_from_slice(&[eye[0], eye[1], eye[2], 1.0]);
        globals.extend_from_slice(&[haze, n_lights as f32, FLOOR, 0.0]);
        Self { globals, lights, boxes, beams, size }
    }
}

fn bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Unit cube: position + normal, 36 vertices.
fn cube() -> Vec<f32> {
    let mut v = Vec::with_capacity(36 * 6);
    let faces: [(V3, V3, V3); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    for (n, a, b) in faces {
        let c = v3::scale(n, 0.5);
        let corner = |sa: f32, sb: f32| v3::add(c, v3::add(v3::scale(a, 0.5 * sa), v3::scale(b, 0.5 * sb)));
        for (sa, sb) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            v.extend_from_slice(&corner(sa, sb));
            v.extend_from_slice(&n);
        }
    }
    v
}

/// Open cone side: radial direction (x, z) + distance from the apex (0..1) per vertex.
fn cone() -> Vec<f32> {
    const N: usize = 32;
    let mut v = Vec::with_capacity(N * 3 * 3);
    for i in 0..N {
        let a0 = i as f32 / N as f32 * std::f32::consts::TAU;
        let a1 = (i + 1) as f32 / N as f32 * std::f32::consts::TAU;
        // The apex vertex takes the middle direction so the normal is smooth there.
        let am = (a0 + a1) * 0.5;
        v.extend_from_slice(&[am.cos(), am.sin(), 0.0]);
        v.extend_from_slice(&[a0.cos(), a0.sin(), 1.0]);
        v.extend_from_slice(&[a1.cos(), a1.sin(), 1.0]);
    }
    v
}

struct Target {
    size: [u32; 2],
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
    blit: wgpu::BindGroup,
}

/// GPU objects, made once and kept in egui's callback resources.
struct Resources {
    globals: wgpu::Buffer,
    lights: wgpu::Buffer,
    scene_bind: wgpu::BindGroup,
    floor: wgpu::RenderPipeline,
    boxes: wgpu::RenderPipeline,
    beams: wgpu::RenderPipeline,
    cube: wgpu::Buffer,
    cone: wgpu::Buffer,
    blit_layout: wgpu::BindGroupLayout,
    blit: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    target: Option<Target>,
    instances: Option<(wgpu::Buffer, wgpu::Buffer)>,
}

impl Resources {
    fn new(device: &wgpu::Device, out: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("openq_vis"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let uniform = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let scene_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("openq_vis_scene"), entries: &[uniform(0), uniform(1)] });
        let globals = device.create_buffer(&wgpu::BufferDescriptor { label: Some("openq_vis_globals"), size: 24 * 4, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let lights = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("openq_vis_lights"),
            size: (MAX_LIGHTS * 12 * 4) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let scene_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("openq_vis_scene"),
            layout: &scene_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: lights.as_entire_binding() },
            ],
        });
        let scene_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("openq_vis"), bind_group_layouts: &[Some(&scene_layout)], immediate_size: 0 });
        let depth = |write: bool| wgpu::DepthStencilState {
            format: DEPTH,
            depth_write_enabled: Some(write),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        };
        let pipeline = |label, vs, fs, buffers: &[wgpu::VertexBufferLayout], blend: Option<wgpu::BlendState>, write: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&scene_pl),
                vertex: wgpu::VertexState { module: &module, entry_point: Some(vs), buffers, compilation_options: Default::default() },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
                depth_stencil: Some(depth(write)),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    targets: &[Some(wgpu::ColorTargetState { format: HDR, blend, write_mask: wgpu::ColorWrites::ALL })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        // Per instance: a model matrix (4 columns) and two vec4s (colour + extra).
        let instance = wgpu::VertexBufferLayout { array_stride: 24 * 4, step_mode: wgpu::VertexStepMode::Instance, attributes: &INSTANCE };
        let floor = pipeline("openq_vis_floor", "floor_vs", "floor_fs", &[], None, true);
        let box_vertex = wgpu::VertexBufferLayout {
            array_stride: 6 * 4,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
        };
        let boxes = pipeline("openq_vis_boxes", "box_vs", "box_fs", &[box_vertex, instance.clone()], None, true);
        let cone_vertex = wgpu::VertexBufferLayout {
            array_stride: 3 * 4,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32],
        };
        let add = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add };
        let beams = pipeline("openq_vis_beams", "beam_vs", "beam_fs", &[cone_vertex, instance], Some(wgpu::BlendState { color: add, alpha: add }), false);
        let cube = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("openq_vis_cube"), contents: &bytes(&cube()), usage: wgpu::BufferUsages::VERTEX });
        let cone = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("openq_vis_cone"), contents: &bytes(&cone()), usage: wgpu::BufferUsages::VERTEX });
        let blit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("openq_vis_blit"),
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
                wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });
        let blit_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("openq_vis_blit"), bind_group_layouts: &[Some(&blit_layout)], immediate_size: 0 });
        let blit = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("openq_vis_blit"),
            layout: Some(&blit_pl),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("blit_vs"), buffers: &[], compilation_options: Default::default() },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(if out.is_srgb() { "blit_fs_linear" } else { "blit_fs" }),
                targets: &[Some(wgpu::ColorTargetState { format: out, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor { label: Some("openq_vis"), mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, ..Default::default() });
        Self { globals, lights, scene_bind, floor, boxes, beams, cube, cone, blit_layout, blit, sampler, target: None, instances: None }
    }

    fn target(&mut self, device: &wgpu::Device, size: [u32; 2]) -> &Target {
        if self.target.as_ref().is_none_or(|t| t.size != size) {
            let tex = |label, format, usage| {
                device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: Some(label),
                        size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage,
                        view_formats: &[],
                    })
                    .create_view(&Default::default())
            };
            let color = tex("openq_vis_color", HDR, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING);
            let depth = tex("openq_vis_depth", DEPTH, wgpu::TextureUsages::RENDER_ATTACHMENT);
            let blit = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("openq_vis_blit"),
                layout: &self.blit_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&color) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.target = Some(Target { size, color, depth, blit });
        }
        self.target.as_ref().expect("made above")
    }
}

/// The paint callback for one frame.
struct VisCallback {
    scene: Scene,
    format: wgpu::TextureFormat,
}

impl CallbackTrait for VisCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let s = &self.scene;
        let res = resources.entry::<Resources>().or_insert_with(|| Resources::new(device, self.format));
        queue.write_buffer(&res.globals, 0, &bytes(&s.globals));
        queue.write_buffer(&res.lights, 0, &bytes(&s.lights));
        let inst = |label, data: &[f32]| {
            // Never empty: a zero-size vertex buffer isn't allowed.
            let data = if data.is_empty() { &[0.0f32; 24][..] } else { data };
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: &bytes(data), usage: wgpu::BufferUsages::VERTEX })
        };
        res.instances = Some((inst("openq_vis_boxes", &s.boxes), inst("openq_vis_beams", &s.beams)));
        res.target(device, s.size);
        let Resources { target: Some(t), instances: Some((bi, ci)), .. } = &*res else { return Vec::new() };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("openq_vis"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &t.color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.006, g: 0.006, b: 0.009, a: 1.0 }), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &t.depth,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &res.scene_bind, &[]);
        pass.set_pipeline(&res.floor);
        pass.draw(0..6, 0..1);
        let n_boxes = (s.boxes.len() / BOX_FLOATS) as u32;
        if n_boxes > 0 {
            pass.set_pipeline(&res.boxes);
            pass.set_vertex_buffer(0, res.cube.slice(..));
            pass.set_vertex_buffer(1, bi.slice(..));
            pass.draw(0..36, 0..n_boxes);
        }
        let n_beams = (s.beams.len() / BEAM_FLOATS) as u32;
        if n_beams > 0 {
            pass.set_pipeline(&res.beams);
            pass.set_vertex_buffer(0, res.cone.slice(..));
            pass.set_vertex_buffer(1, ci.slice(..));
            pass.draw(0..32 * 3, 0..n_beams);
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(res) = resources.get::<Resources>() else { return };
        let Some(t) = &res.target else { return };
        pass.set_pipeline(&res.blit);
        pass.set_bind_group(0, &t.blit, &[]);
        pass.draw(0..3, 0..1);
    }
}

impl App {
    /// The 3D page: the scene, with orbit/pan/zoom, click to select, and (aim mode) click on the
    /// floor to aim the selected moving heads.
    pub(super) fn visualiser_page(&mut self, ui: &mut Ui, show: &mut crate::show::Show) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        let design = self.can_design();
        ui.horizontal(|ui| {
            ui.label(t("Haze"));
            ui.add(egui::Slider::new(&mut self.vis.haze, 0.0..=1.0).show_value(false));
            ui.checkbox(&mut self.vis.labels, t("Numbers"));
            if design {
                let (fill, fg) = if self.vis.aim { (theme::AMBER, Color32::BLACK) } else { (theme::BG_RAISED, theme::TEXT) };
                if ui
                    .add(egui::Button::new(RichText::new(theme::with_icon(super::icons::CONTROL_CAMERA, t("Aim"))).color(fg)).fill(fill))
                    .on_hover_text(t("Click the floor to point the selected moving heads there"))
                    .clicked()
                {
                    self.vis.aim = !self.vis.aim;
                }
            }
            ui.separator();
            for (label, yaw, pitch) in [(t("Front"), 0.0, 0.25), (t("Top"), 0.0, 1.5), (t("Side"), std::f32::consts::FRAC_PI_2, 0.2)] {
                if ui.button(label).clicked() {
                    self.vis.camera.yaw = yaw;
                    self.vis.camera.pitch = pitch;
                }
            }
            if ui.button(t("Reset view")).clicked() {
                self.vis.camera = Camera::default();
            }
            ui.label(RichText::new(t("Drag to orbit · Shift/right-drag to pan · scroll to zoom")).weak().small());
        });
        let (rect, r) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        if rect.width() < 8.0 || rect.height() < 8.0 {
            return;
        }
        // Camera.
        let cam = &mut self.vis.camera;
        let pan_drag = r.dragged_by(egui::PointerButton::Secondary) || r.dragged_by(egui::PointerButton::Middle) || (r.dragged() && ui.input(|i| i.modifiers.shift));
        let d = r.drag_delta();
        if pan_drag {
            let eye = cam.eye();
            let f = v3::norm(v3::sub(cam.target, eye));
            let s = v3::norm(v3::cross(f, [0.0, 1.0, 0.0]));
            let u = v3::cross(s, f);
            let k = cam.dist * 0.0015;
            cam.target = v3::add(cam.target, v3::add(v3::scale(s, -d.x * k), v3::scale(u, d.y * k)));
        } else if r.dragged_by(egui::PointerButton::Primary) {
            cam.yaw -= d.x * 0.008;
            cam.pitch = (cam.pitch + d.y * 0.006).clamp(0.02, 1.55);
        }
        if r.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                cam.dist = (cam.dist * (1.0 - scroll * 0.0015)).clamp(1.0, 150.0);
            }
        }
        let cam = *cam;
        let frame = show.media.light.frame();
        let l = &show.ws.lighting;
        let looks = visual::looks(l, &frame);
        // Clicks: aim at the floor, or pick a fixture.
        if r.clicked()
            && let Some(pos) = r.interact_pointer_pos()
        {
            let (o, dir) = cam.ray(rect, pos);
            if self.vis.aim && design {
                if let Some(hit) = visual::floor_hit(o, dir) {
                    self.aim_selection(l, hit);
                }
            } else {
                let picked = pick(&looks, o, dir);
                let add = ui.input(|i| i.modifiers.command);
                let sel = &mut self.light.selection;
                match (picked, add) {
                    (Some(id), true) => {
                        if let Some(i) = sel.iter().position(|x| *x == id) {
                            sel.remove(i);
                        } else {
                            sel.push(id);
                        }
                    }
                    (Some(id), false) => *sel = vec![id],
                    (None, false) => sel.clear(),
                    (None, true) => {}
                }
            }
        }
        let p = ui.painter_at(rect);
        match &self.gpu {
            Some(gpu) => {
                let ppp = ui.ctx().pixels_per_point();
                let size = [((rect.width() * ppp).round() as u32).max(1), ((rect.height() * ppp).round() as u32).max(1)];
                let scene = Scene::build(&looks, &self.light.selection, &cam, rect.aspect_ratio(), self.vis.haze, size);
                p.add(egui_wgpu::Callback::new_paint_callback(rect, VisCallback { scene, format: gpu.target_format }));
            }
            None => {
                p.rect_filled(rect, 6.0, theme::BG_INPUT);
                p.text(rect.center(), egui::Align2::CENTER_CENTER, t("The 3D view needs a GPU renderer."), egui::FontId::proportional(14.0), theme::TEXT_DIM);
            }
        }
        if self.vis.labels {
            for f in &looks {
                if let Some(at) = cam.project(rect, v3::add(f.position, [0.0, 0.35, 0.0])) {
                    let sel = self.light.selection.contains(&f.id);
                    p.text(at, egui::Align2::CENTER_BOTTOM, f.id.to_string(), theme::semibold(11.5), if sel { theme::BLUE } else { theme::TEXT_DIM });
                }
            }
        }
        if self.vis.aim {
            p.text(rect.left_top() + egui::vec2(10.0, 8.0), egui::Align2::LEFT_TOP, t("Aim: click the floor"), theme::semibold(12.0), theme::AMBER);
        }
    }

    /// Point the selected moving heads at `target` (programmer pan/tilt).
    fn aim_selection(&mut self, l: &Lighting, target: V3) {
        use crate::light::model::{Attribute, Value};
        for f in self.light.selection.clone() {
            if let Some((pan, tilt)) = visual::aim(l, f, target) {
                let v = self.light.programmer.entry(f).or_default();
                v.insert(Attribute::Pan, Value::Level(pan));
                v.insert(Attribute::Tilt, Value::Level(tilt));
            }
        }
    }
}

/// The fixture nearest along a ray (within 0.35 m of it).
fn pick(looks: &[FixtureLook], o: V3, dir: V3) -> Option<FixtureId> {
    looks
        .iter()
        .filter_map(|f| {
            let to = v3::sub(f.position, o);
            let along = v3::dot(to, dir);
            if along <= 0.0 {
                return None;
            }
            let off = v3::len(v3::sub(to, v3::scale(dir, along)));
            (off < 0.35).then_some((along, f.id))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|x| x.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_is_valid() {
        use eframe::wgpu::naga;
        let module = naga::front::wgsl::parse_str(SHADER).expect("WGSL parses");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty()).validate(&module).expect("WGSL validates");
    }

    #[test]
    fn camera_projects_and_picks() {
        let cam = Camera::default();
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let p = cam.project(rect, cam.target).unwrap();
        assert!((p.x - 400.0).abs() < 0.5 && (p.y - 300.0).abs() < 0.5, "target in the middle: {p:?}");
        // A ray through the projection of a point passes through that point.
        let pt = [3.0, 1.0, -2.0];
        let (o, d) = cam.ray(rect, cam.project(rect, pt).unwrap());
        let to = v3::sub(pt, o);
        let off = v3::len(v3::sub(to, v3::scale(d, v3::dot(to, d))));
        assert!(off < 0.01, "{off}");
        let look = FixtureLook {
            id: 9,
            position: pt,
            mount: visual::rot_x(0.0),
            moving: false,
            head: visual::rot_x(0.0),
            dir: visual::DOWN,
            angle: 20.0,
            color: [1.0; 3],
            intensity: 1.0,
        };
        assert_eq!(pick(std::slice::from_ref(&look), o, d), Some(9));
    }

    #[test]
    fn along_maps_down_to_dir() {
        for d in [[0.0, -1.0, 0.0], [0.6, -0.8, 0.0], [0.0, 1.0, 0.0], [0.3, 0.1, -0.9]] {
            let d = v3::norm(d);
            let r = along(d);
            let got = visual::mul(&r, visual::DOWN);
            assert!(v3::len(v3::sub(got, d)) < 1e-5, "{d:?} → {got:?}");
        }
    }

    #[test]
    fn scene_has_a_beam_per_lit_fixture() {
        let mut looks = vec![];
        for (i, lit) in [1.0, 0.0].iter().enumerate() {
            looks.push(FixtureLook {
                id: i as u32 + 1,
                position: [i as f32, 5.0, 0.0],
                mount: visual::rot_x(0.0),
                moving: i == 0,
                head: visual::rot_x(0.0),
                dir: visual::DOWN,
                angle: 20.0,
                color: [1.0, 0.5, 0.0],
                intensity: *lit,
            });
        }
        let s = Scene::build(&looks, &[], &Camera::default(), 1.5, 0.5, [100, 100]);
        assert_eq!(s.boxes.len(), 3 * BOX_FLOATS, "moving head = base + head");
        assert_eq!(s.beams.len(), BEAM_FLOATS);
        assert_eq!(s.globals[21], 1.0, "one light on the floor");
        // Instance: 16 matrix floats, colour, then (radius, length): the head hangs 0.28 m below.
        assert!((s.beams[21] - 4.72).abs() < 1e-4, "beam reaches the floor: {}", s.beams[21]);
    }
}
