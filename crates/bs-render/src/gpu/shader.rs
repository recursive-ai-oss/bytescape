//! The one shader every GPU draw uses, as WGSL text.

pub(crate) const SOURCE: &str = r#"
// One pipeline draws everything: solid rectangles, label and image quads,
// and diagonal hatches. Each rectangle is one instance.

struct View {
    size: vec2<f32>,
    pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct Inst {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) params: vec4<f32>,
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) params: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VOut {
    let c = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let p = inst.rect.xy + c * inst.rect.zw;
    var o: VOut;
    o.pos = vec4<f32>(p.x / view.size.x * 2.0 - 1.0, 1.0 - p.y / view.size.y * 2.0, 0.0, 1.0);
    o.uv = mix(inst.uv.xy, inst.uv.zw, c);
    o.color = inst.color;
    o.params = inst.params;
    return o;
}

fn tri_cdf(z: f32) -> f32 {
    let c = clamp(z, -1.0, 1.0);
    if (c < 0.0) {
        let a = 1.0 + c;
        return 0.5 * a * a;
    }
    let b = 1.0 - c;
    return 1.0 - 0.5 * b * b;
}

// params.x: 0 solid, 1 image (premultiplied texels scaled by the color),
// 2 rising hatch, 3 falling hatch. For hatches params.y is the spacing and
// params.z the line width, in device pixels.
@fragment
fn fs(i: VOut) -> @location(0) vec4<f32> {
    let kind = i.params.x;
    if (kind < 0.5) {
        return i.color;
    }
    if (kind < 1.5) {
        return textureSampleLevel(tex, samp, i.uv, 0.0) * i.color;
    }
    let s = i.params.y;
    let w = i.params.z;
    var t = i.pos.x + i.pos.y;
    if (kind > 2.5) {
        t = i.pos.x - i.pos.y;
    }
    let m = t - s * round(t / s);
    var cover: f32;
    if (w <= 1.0) {
        // Lines up to a pixel wide are drawn as hairlines: the pixel the line
        // runs through is solid and its neighbours fall off linearly.
        cover = clamp(1.0 - abs(m), 0.0, 1.0) * w;
    } else {
        // The exact area of the pixel inside the stripe. Across a pixel the
        // sum of the two offsets is triangularly distributed.
        let h = w * 0.70710678;
        cover = tri_cdf(h - m) - tri_cdf(-h - m);
    }
    return i.color * cover;
}
"#;
