// The video output's shaders.
//
// Layers are composited into a canvas texture (one per output window, the size of the part of the
// stage it shows): `vs_layer` places a layer's mesh, `fs_black` / `fs_white` sample its picture
// (RGBA, planar YUV, NV12 or a text coverage mask, converted to RGB here), apply its video effects
// and output a premultiplied colour for the pipeline's blend mode, in gamma-encoded values.
// `fs_warp` then draws the canvas onto the window through the inverse corner-pin homography, with
// soft-edge blend zones and black outside the canvas; `fs_overlay` adds the mapping pins.

struct Fx {
    // pixel size, blur radius, sharpen amount, sharpen radius (source pixels)
    spatial: vec4<f32>,
    // x = number of colour effects
    count: vec4<f32>,
    // per colour effect: (code, value 0, value 1, value 2), (red, green, blue, 0)
    slots: array<vec4<f32>, 16>,
};

struct Layer {
    // Straight RGBA multiplier: tint × plane colour, alpha × opacity.
    color: vec4<f32>,
    // x: 0 premultiplied RGBA, 1 straight RGBA, 2 planar YUV, 3 NV12, 4 coverage mask;
    // y: planar YUV has an alpha plane; z: mirrored.
    kind: vec4<f32>,
    // YUV → RGB rows: rgb[i] = dot(m[i].xyz, yuv) + m[i].w
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    fx: Fx,
};

@group(0) @binding(0) var t0: texture_2d<f32>;
@group(0) @binding(1) var t1: texture_2d<f32>;
@group(0) @binding(2) var t2: texture_2d<f32>;
@group(0) @binding(3) var t3: texture_2d<f32>;
@group(0) @binding(4) var samp: sampler;
@group(0) @binding(5) var<uniform> layer: Layer;

struct View {
    // The part of the source stage this output shows: x0, y0, x1, y1 (0..1).
    crop: vec4<f32>,
};
@group(1) @binding(0) var<uniform> view: View;

struct LOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_layer(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> LOut {
    let size = max(view.crop.zw - view.crop.xy, vec2<f32>(0.000001));
    let p = (pos - view.crop.xy) / size;
    var o: LOut;
    o.pos = vec4<f32>(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

// The layer's picture at `uv`, premultiplied.
fn px(uv0: vec2<f32>) -> vec4<f32> {
    var uv = uv0;
    if (layer.kind.z > 0.5) {
        uv.x = 1.0 - uv.x;
    }
    let k = i32(layer.kind.x + 0.5);
    if (k == 0) {
        return textureSampleLevel(t0, samp, uv, 0.0);
    }
    if (k == 1) {
        let c = textureSampleLevel(t0, samp, uv, 0.0);
        return vec4<f32>(c.rgb * c.a, c.a);
    }
    if (k == 4) {
        return vec4<f32>(textureSampleLevel(t0, samp, uv, 0.0).r);
    }
    let y = textureSampleLevel(t0, samp, uv, 0.0).r;
    var c = vec2<f32>(0.0);
    if (k == 3) {
        c = textureSampleLevel(t1, samp, uv, 0.0).rg;
    } else {
        c = vec2<f32>(textureSampleLevel(t1, samp, uv, 0.0).r, textureSampleLevel(t2, samp, uv, 0.0).r);
    }
    let yuv = vec3<f32>(y, c);
    let rgb = clamp(
        vec3<f32>(dot(layer.m0.xyz, yuv) + layer.m0.w, dot(layer.m1.xyz, yuv) + layer.m1.w, dot(layer.m2.xyz, yuv) + layer.m2.w),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    var a = 1.0;
    if (layer.kind.y > 0.5) {
        a = textureSampleLevel(t3, samp, uv, 0.0).r;
    }
    return vec4<f32>(rgb * a, a);
}

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


// The picture with the spatial effects (pixellate, blur, sharpen); premultiplied.
fn sample_fx(uv0: vec2<f32>) -> vec4<f32> {
    let dims = vec2<f32>(textureDimensions(t0));
    var uv = uv0;
    let pxs = layer.fx.spatial.x;
    if (pxs > 1.0) {
        uv = (floor(uv * dims / pxs) + 0.5) * pxs / dims;
    }
    var c = px(uv);
    let r = layer.fx.spatial.y;
    if (r > 0.25) {
        // Golden-angle spiral of taps with Gaussian weights (sigma = radius / 2).
        var acc = c;
        var total = 1.0;
        for (var k = 1; k < 48; k++) {
            let f = f32(k) / 47.0;
            let a = f32(k) * 2.39996323;
            let off = vec2<f32>(cos(a), sin(a)) * r * sqrt(f) / dims;
            let w = exp(-2.0 * f);
            acc += px(uv + off) * w;
            total += w;
        }
        c = acc / total;
    }
    let sh = layer.fx.spatial.z;
    if (sh > 0.0) {
        let o = layer.fx.spatial.w / dims;
        let around = (px(uv + vec2<f32>(o.x, 0.0)) + px(uv - vec2<f32>(o.x, 0.0)) + px(uv + vec2<f32>(0.0, o.y)) + px(uv - vec2<f32>(0.0, o.y))) * 0.25;
        c = clamp(c + (c - around) * sh, vec4<f32>(0.0), vec4<f32>(1.0));
        c = vec4<f32>(min(c.rgb, vec3<f32>(c.a)), c.a);
    }
    return c;
}

// The colour effects, in order, on a premultiplied colour.
fn colour_fx(cin: vec4<f32>, uv: vec2<f32>) -> vec4<f32> {
    let n = i32(layer.fx.count.x + 0.5);
    if (n == 0) {
        return cin;
    }
    var a = cin.a;
    var rgb = select(vec3<f32>(0.0), cin.rgb / max(a, 0.0001), a > 0.0001);
    for (var k = 0; k < 8; k++) {
        if (k >= n) {
            break;
        }
        let h = layer.fx.slots[k * 2];
        let col = layer.fx.slots[k * 2 + 1].rgb;
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


// Premultiplied layer colour: over black (Normal, Add, Screen, Lighten, Subtract) or over white
// (Multiply, Darken), so a transparent pixel leaves the canvas unchanged either way.
fn over_black(uv: vec2<f32>) -> vec4<f32> {
    let tint = vec4<f32>(layer.color.rgb * layer.color.a, layer.color.a);
    return colour_fx(tint * sample_fx(uv), uv);
}

@fragment
fn fs_black(i: LOut) -> @location(0) vec4<f32> {
    return over_black(i.uv);
}

@fragment
fn fs_white(i: LOut) -> @location(0) vec4<f32> {
    let c = over_black(i.uv);
    return vec4<f32>(c.rgb + vec3<f32>(1.0 - c.a), c.a);
}

// ---------------------------------------------------------------- canvas → window

struct Warp {
    // Inverse corner-pin homography rows: window (x, y, 1) → k·(u, v, 1) on the canvas.
    r0: vec4<f32>,
    r1: vec4<f32>,
    r2: vec4<f32>,
    // Soft-edge blend widths: left, top, right, bottom (fractions of the canvas).
    blend: vec4<f32>,
    // x: blend gamma.
    misc: vec4<f32>,
};

@group(0) @binding(0) var canvas: texture_2d<f32>;
@group(0) @binding(1) var csamp: sampler;
@group(0) @binding(2) var<uniform> warp: Warp;

struct FOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle covering the target; uv runs 0..1 across it, y down.
@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> FOut {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var o: FOut;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}

// Brightness across a blend zone, t = 0 at the outer edge to 1 inside (see geometry::blend_gain).
fn blend_gain(x: f32) -> f32 {
    let t = clamp(x, 0.0, 1.0);
    var s = 0.0;
    if (t < 0.5) {
        s = 2.0 * t * t;
    } else {
        s = 1.0 - 2.0 * (1.0 - t) * (1.0 - t);
    }
    return pow(s, 1.0 / max(warp.misc.x, 0.1));
}

fn edge(d: f32, w: f32) -> f32 {
    if (w > 0.0 && d < w) {
        return blend_gain(d / w);
    }
    return 1.0;
}

fn warped(uv: vec2<f32>) -> vec3<f32> {
    let p = vec3<f32>(uv, 1.0);
    let q = vec3<f32>(dot(warp.r0.xyz, p), dot(warp.r1.xyz, p), dot(warp.r2.xyz, p));
    if (q.z <= 0.000001) {
        return vec3<f32>(0.0);
    }
    let c = q.xy / q.z;
    if (c.x < 0.0 || c.y < 0.0 || c.x > 1.0 || c.y > 1.0) {
        return vec3<f32>(0.0);
    }
    let g = edge(c.x, warp.blend.x) * edge(c.y, warp.blend.y) * edge(1.0 - c.x, warp.blend.z) * edge(1.0 - c.y, warp.blend.w);
    return textureSampleLevel(canvas, csamp, c, 0.0).rgb * g;
}

fn linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c < vec3<f32>(0.04045));
}

@fragment
fn fs_warp(i: FOut) -> @location(0) vec4<f32> {
    return vec4<f32>(warped(i.uv), 1.0);
}

@fragment
fn fs_warp_linear(i: FOut) -> @location(0) vec4<f32> {
    return vec4<f32>(linear(warped(i.uv)), 1.0);
}

// ---------------------------------------------------------------- mapping pins

@group(0) @binding(0) var ov: texture_2d<f32>;
@group(0) @binding(1) var osamp: sampler;

@fragment
fn fs_overlay(i: FOut) -> @location(0) vec4<f32> {
    return textureSampleLevel(ov, osamp, i.uv, 0.0);
}

@fragment
fn fs_overlay_linear(i: FOut) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(ov, osamp, i.uv, 0.0);
    let a = max(c.a, 0.0001);
    return vec4<f32>(linear(c.rgb / a) * c.a, c.a);
}
