struct VertexInput {
    float2 position : POSITION;
    float2 texcoord : TEXCOORD0;
};

struct PixelInput {
    float4 position : SV_POSITION;
    float2 texcoord : TEXCOORD0;
};

PixelInput vs_main(VertexInput input) {
    PixelInput output;
    output.position = float4(input.position, 0.0f, 1.0f);
    output.texcoord = input.texcoord;
    return output;
}

#ifdef VIDEO_ARRAY
Texture2DArray<float4> input_plane_0 : register(t0);
Texture2DArray<float4> input_plane_1 : register(t1);
#define VIDEO_COORD float3(input.texcoord, 0.0f)
#else
Texture2D<float4> input_plane_0 : register(t0);
Texture2D<float4> input_plane_1 : register(t1);
#define VIDEO_COORD input.texcoord
#endif


SamplerState video_sampler : register(s0);

cbuffer ColorTransform : register(b0) {
    float4 color_row_0;
    float4 color_row_1;
    float4 color_row_2;
    // x: SDR(0), scRGB HDR(1), HDR -> SDR(2); y: source mastering peak nits.
    float4 hdr_parameters;
};

float3 pq_to_scrgb(float3 rgb) {
    float3 p = pow(saturate(rgb), 1.0f / 78.84375f);
    float3 linear2020 = pow(max(p - 0.8359375f, 0.0f) / max(18.8515625f - 18.6875f * p, 0.000001f), 1.0f / 0.1593017578125f) * 125.0f;
    return float3(dot(float3(1.660496f,-0.587656f,-0.072840f),linear2020),
                  dot(float3(-0.124547f,1.132895f,-0.008348f),linear2020),
                  dot(float3(-0.018154f,-0.100597f,1.118751f),linear2020));
}

float filmic(float x) {
    return (x*(0.15f*x+0.05f)+0.004f)/(x*(0.15f*x+0.5f)+0.06f)-0.0666666667f;
}

float3 hdr_to_sdr(float3 rgb_linear) {
    float peak = hdr_parameters.y;
    float white = peak > 0.0f && peak <= 10000.0f ? max(peak / 80.0f, 1.0f) : 12.5f;
    float maximum = max(rgb_linear.r, max(rgb_linear.g, rgb_linear.b));
    rgb_linear = maximum > 0.0f ? rgb_linear * (filmic(maximum) / max(filmic(white), 0.000001f) / maximum) : 0.0f;
    // Filmic curve and rgb_linear-to-sRGB approximation.
    float3 nonlinear = 1.13005f*sqrt(max(rgb_linear-0.00228f,0.0f))-0.13448f*rgb_linear+0.005719f;
    return float3(rgb_linear.r < 0.0031308f ? rgb_linear.r*12.92f : nonlinear.r,
                  rgb_linear.g < 0.0031308f ? rgb_linear.g*12.92f : nonlinear.g,
                  rgb_linear.b < 0.0031308f ? rgb_linear.b*12.92f : nonlinear.b);
}

float4 finish_yuv(float3 rgb) {
    if (hdr_parameters.x < 0.5f) return float4(rgb,1.0f);
    float3 rgb_linear=pq_to_scrgb(rgb);
    return float4(hdr_parameters.x > 1.5f ? hdr_to_sdr(rgb_linear) : rgb_linear,1.0f);
}

float4 ps_yuv(PixelInput input) : SV_TARGET {
    float y = input_plane_0.Sample(video_sampler, VIDEO_COORD).r;
    float2 uv = input_plane_1.Sample(video_sampler, VIDEO_COORD).rg;
    float4 yuv = float4(y, uv.x, uv.y, 1.0f);
    return finish_yuv(float3(
        dot(color_row_0, yuv),
        dot(color_row_1, yuv),
        dot(color_row_2, yuv)
    ));
}

float4 ps_rgba(PixelInput input) : SV_TARGET {
    float4 rgb=input_plane_0.Sample(video_sampler, VIDEO_COORD);
    // HDR RGBA effect targets already contain rgb_linear scRGB values.
    if (hdr_parameters.x > 1.5f) rgb.rgb=hdr_to_sdr(rgb.rgb);
    return rgb;
}



float4 ps_packed_yuv(PixelInput input) : SV_TARGET {
    float4 packed = input_plane_0.Sample(video_sampler, VIDEO_COORD);
#ifdef VIDEO_Y410
    // Y410 -> R10G10B10A2 is U,Y,V,A. Convert UNORM10 to high-aligned UNORM16
    // so it uses the same 10-bit color transform as P010.
    float4 yuv = float4(packed.grb * (1023.0f * 64.0f / 65535.0f), 1.0f);
#else
    // AYUV -> RGBA8 exposes little-endian bytes V,U,Y,A.
    float4 yuv = float4(packed.bgr, 1.0f);
#endif
    return finish_yuv(float3(dot(color_row_0, yuv), dot(color_row_1, yuv), dot(color_row_2, yuv)));
}
