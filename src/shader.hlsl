// Single pass from the decoder's NV12 picture to the screen:
// bicubic luma upscale -> contrast-adaptive sharpening -> BT.709 limited-range
// conversion -> saturation / contrast.

Texture2DArray<float> lumaTex : register(t0);
Texture2DArray<float2> chromaTex : register(t1);
SamplerState linearClamp : register(s0);

cbuffer Params : register(b0) {
    float2 visibleSize; // picture size in luma pixels
    float2 textureSize; // decoder texture size (may include padding)
    float sharpen;      // 0 = off, 1 = strongest
    float saturation;   // 1 = unchanged
    float contrast;     // 1 = unchanged
    float padding;
};

struct VSOut {
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

// One oversized triangle covering the screen; no vertex buffer needed.
VSOut vs_main(uint id : SV_VertexID) {
    VSOut o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.position = float4(o.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}

// Luma at a position given in luma pixels, expanded from 16..235 to 0..1.
float Luma(float2 p) {
    p = clamp(p, 0.5, visibleSize - 0.5);
    float y = lumaTex.SampleLevel(linearClamp, float3(p / textureSize, 0), 0);
    return (y - 16.0 / 255.0) * (255.0 / 219.0);
}

// Catmull-Rom bicubic in 9 bilinear taps.
float LumaBicubic(float2 p) {
    float2 base = floor(p - 0.5) + 0.5;
    float2 f = p - base;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 p0 = base - 1.0;
    float2 p3 = base + 2.0;
    float2 p12 = base + w2 / w12;

    float result = 0;
    result += Luma(float2(p0.x, p0.y)) * w0.x * w0.y;
    result += Luma(float2(p12.x, p0.y)) * w12.x * w0.y;
    result += Luma(float2(p3.x, p0.y)) * w3.x * w0.y;
    result += Luma(float2(p0.x, p12.y)) * w0.x * w12.y;
    result += Luma(float2(p12.x, p12.y)) * w12.x * w12.y;
    result += Luma(float2(p3.x, p12.y)) * w3.x * w12.y;
    result += Luma(float2(p0.x, p3.y)) * w0.x * w3.y;
    result += Luma(float2(p12.x, p3.y)) * w12.x * w3.y;
    result += Luma(float2(p3.x, p3.y)) * w3.x * w3.y;
    return result;
}

float4 ps_main(VSOut input) : SV_Target {
    float2 p = input.uv * visibleSize;
    float y = LumaBicubic(p);

    if (sharpen > 0) {
        // Contrast-adaptive sharpening: strong on soft detail, backs off near
        // black/white and on already-hard edges so it does not ring.
        float n = Luma(p + float2(0, -1));
        float s = Luma(p + float2(0, 1));
        float w = Luma(p + float2(-1, 0));
        float e = Luma(p + float2(1, 0));
        float lo = saturate(min(y, min(min(n, s), min(w, e))));
        float hi = saturate(max(y, max(max(n, s), max(w, e))));
        float amount = sqrt(saturate(min(lo, 1.0 - hi) / max(hi, 1e-4)));
        float weight = amount * (-1.0 / lerp(8.0, 5.0, saturate(sharpen)));
        y = (y + (n + s + w + e) * weight) / (1.0 + 4.0 * weight);
    }

    // Chroma is half resolution and sited on even luma columns.
    float2 cp = clamp(p + float2(0.5, 0), 1.0, visibleSize - 1.0);
    float2 c = chromaTex.SampleLevel(linearClamp, float3(cp / textureSize, 0), 0);
    float cb = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
    float cr = (c.y - 128.0 / 255.0) * (255.0 / 224.0);

    float3 rgb = float3(
        y + 1.5748 * cr,
        y - 0.1873 * cb - 0.4681 * cr,
        y + 1.8556 * cb);

    float gray = dot(rgb, float3(0.2126, 0.7152, 0.0722));
    rgb = lerp(gray, rgb, saturation);
    rgb = (rgb - 0.5) * contrast + 0.5;
    return float4(saturate(rgb), 1);
}
