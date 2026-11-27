// 窗口的背景：日冕（深色圆盘边缘的光，光丝向外放射、缓缓流动）。只有一段弧被点亮（从起点顺时针到进度），弧前端可以有
// 一颗亮点，没点亮的一段只剩一圈暗的边；一块面板里光从朝圆盘的那条竖边按指数漫进去。坐标是像素，原点在左上。输出已做
// 色调映射与 sRGB 编码，不透明。

struct Params {
    // 宽、高（像素）、每个逻辑点几个像素、光芒在一个循环里走到哪（0 到 1）
    size: vec4f,
    // 圆盘中心 x、y 与半径（像素），整体强度（0 到 1）
    disc: vec4f,
    // 点亮的弧长（0 到 1，1 为整圈）、弧的起点从 12 点顺时针转过的圈数（0 到 1）、光丝与光晕伸出多远的倍数（大于 0）、
    // 弧前端亮点的亮度（0 到 1）
    arc: vec4f,
    // 光的主色、越往外越偏向的颜色、紧贴圆盘边缘的亮环的颜色（线性的 0 到 1；w 不用）
    light: vec4f,
    deep: vec4f,
    core: vec4f,
    // 底色（sRGB 编码的 0 到 1；w 不用）：画面不比它暗
    ground: vec4f,
    // 面板：左、上、右、下（像素）
    panel: vec4f,
    // 有没有面板（0 或 1）、面板的圆角半径（像素）
    shape: vec4f,
}

@group(0) @binding(0) var<uniform> params: Params;

const TAU: f32 = 6.2831853;
// 光漫进面板的深度（逻辑点）
const SPILL: f32 = 85.0;

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4f {
    // 一个盖住整个画面的大三角形
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    return vec4f(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn hash(q: vec3f) -> f32 {
    var p = fract(q * 0.3183099 + vec3f(0.11, 0.17, 0.13));
    p = p * 17.0;
    return fract(p.x * p.y * p.z * (p.x + p.y + p.z));
}

fn noise(x: vec3f) -> f32 {
    let i = floor(x);
    var f = fract(x);
    f = f * f * (3.0 - 2.0 * f);
    return mix(
        mix(mix(hash(i), hash(i + vec3f(1.0, 0.0, 0.0)), f.x),
            mix(hash(i + vec3f(0.0, 1.0, 0.0)), hash(i + vec3f(1.0, 1.0, 0.0)), f.x), f.y),
        mix(mix(hash(i + vec3f(0.0, 0.0, 1.0)), hash(i + vec3f(1.0, 0.0, 1.0)), f.x),
            mix(hash(i + vec3f(0.0, 1.0, 1.0)), hash(i + vec3f(1.0, 1.0, 1.0)), f.x), f.y),
        f.z);
}

fn fbm(start: vec3f) -> f32 {
    var p = start;
    var sum = 0.0;
    var amplitude = 0.5;
    for (var i = 0; i < 5; i++) {
        sum += amplitude * noise(p);
        p = p * 2.03 + vec3f(1.7, 9.2, 3.1);
        amplitude *= 0.5;
    }
    return sum;
}

fn aces(x: vec3f) -> vec3f {
    return clamp((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14), vec3f(0.0), vec3f(1.0));
}

// 点 p 到圆角矩形 rect（左、上、右、下）边上的有符号距离，里面为负
fn rounded(p: vec2f, rect: vec4f, radius: f32) -> f32 {
    let center = (rect.xy + rect.zw) * 0.5;
    let q = abs(p - center) - (rect.zw - rect.xy) * 0.5 + radius;
    return length(max(q, vec2f(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

// 点 p 离面板 rect 朝圆盘（中心横坐标 toward）的那条竖边多远；圆盘的横坐标落在面板里时取近的那条
fn depth(p: vec2f, rect: vec4f, toward: f32) -> f32 {
    if (toward >= rect.z) {
        return rect.z - p.x;
    }
    if (toward <= rect.x) {
        return p.x - rect.x;
    }
    return min(p.x - rect.x, rect.z - p.x);
}

@fragment
fn fragment(@builtin(position) position: vec4f) -> @location(0) vec4f {
    let p = position.xy;
    let scale = params.size.z;
    let phase = params.size.w;
    let center = params.disc.xy;
    let radius = params.disc.z;
    let strength = params.disc.w;
    let progress = params.arc.x;
    let start = params.arc.y;
    let reach = params.arc.z;
    let bead = params.arc.w;
    let uv = p - center;
    let r = length(uv);
    let angle = atan2(uv.y, uv.x);
    let d = (r - radius) / radius;
    let dir = vec2f(cos(angle), sin(angle));
    let around = 0.7 * vec2f(cos(TAU * phase), sin(TAU * phase));

    // 日冕：先扭曲坐标再取噪声，光丝弯曲、成簇
    let warp = fbm(vec3f(dir * 2.2 + around * 0.6, d * 1.2));
    var streak = fbm(vec3f(dir * 9.0 + warp * 1.6, d * 0.9 - around.x * 0.5 + around.y * 0.3));
    streak = pow(clamp(streak * 1.25, 0.0, 1.0), 3.0) * 2.2;
    let fine = fbm(vec3f(dir * 38.0 + warp, d * 3.0 + around.y));
    streak *= 0.65 + 0.7 * fine;
    let outside = max(d, 0.0);

    // 点亮的弧：从起点顺时针量到 progress。离圆盘越远两端收得越缓，光丝在端头渐隐而不是一刀切；弧长在最后 3% 里把
    // 两端之间的缺口合上，到整圈时没有接缝，也不在 1 处跳变
    let along = fract(atan2(uv.x, -uv.y) / TAU - start);
    let soft = 0.07 * outside;
    let closed = smoothstep(0.97, 1.0, progress);
    let ends = smoothstep(0.0, 0.012 + soft, along)
        * (1.0 - smoothstep(progress - 0.014 - soft, progress + 0.003, along));
    let lit = mix(ends, 1.0, closed);

    let corona = exp(-outside / ((0.035 + 0.32 * streak) * reach)) * (0.18 + 0.8 * streak);
    let halo = exp(-outside / 0.05);
    let glow = exp(-outside / (0.30 * reach));
    let ambient = exp(-outside / 1.8);
    // 亮环：红、绿、蓝的边缘各错开一点，带色散的彩边；没点亮的一段只留一圈暗的
    let w = 0.008;
    let rim = vec3f(exp(-abs(d - 0.007) / w), exp(-abs(d) / w), exp(-abs(d + 0.006) / w));
    let base = mix(params.light.rgb, params.deep.rgb, clamp(d / 0.9, 0.0, 1.0));
    var hdr = (rim * params.core.rgb * 3.2 + halo * base * 1.2 + corona * base * 1.15 + glow * params.deep.rgb * 0.12) * lit
        + rim * vec3f(0.16) * (1.0 - lit)
        + ambient * mix(params.deep.rgb, params.light.rgb, 0.35) * (0.05 + 0.10 * streak * lit) * min(reach, 1.0);
    // 弧前端的亮点：到整圈时随缺口一起淡掉
    if (bead > 0.0) {
        let a = TAU * (start + progress);
        let head = center + radius * vec2f(sin(a), -cos(a));
        let q = length(p - head) / radius;
        hdr += params.core.rgb * (exp(-q / 0.012) * 6.0 + exp(-q / 0.07) * 0.9) * bead * (1.0 - closed);
    }
    hdr *= smoothstep(-0.01, 0.0, d) * strength;

    // 面板里光从朝圆盘的那条竖边漫进去；边上一个像素过渡
    if (params.shape.x > 0.5) {
        let rect = params.panel;
        let cover = clamp(0.5 - rounded(p, rect, params.shape.y), 0.0, 1.0);
        let spill = exp(-max(depth(p, rect, center.x), 0.0) / (SPILL * scale));
        hdr *= mix(1.0, spill, cover);
    }
    var color = pow(aces(hdr), vec3f(1.0 / 2.2));

    // 胶片颗粒
    color += (hash(vec3f(p, floor(phase * 720.0))) - 0.5) * 0.035;
    return vec4f(max(color, params.ground.rgb), 1.0);
}
