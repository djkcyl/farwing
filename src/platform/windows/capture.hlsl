Texture2D<float4> desktop : register(t0);
Texture2D<float4> pointer_shape : register(t1);
SamplerState filtering : register(s0);
cbuffer Params : register(b0) {
    float4 params; // rotation, conversion mode, SDR white scale, HDR peak nits
    float4 pointer_rect; // top-left and shape size, in desktop pixels
    float4 pointer_meta; // desktop size, DDA shape kind, visible
}
struct Pixel { float4 position : SV_POSITION; float2 uv : TEXCOORD0; };
Pixel vs_main(uint id : SV_VertexID) {
    Pixel p;
    p.uv = float2((id << 1) & 2, id & 2);
    p.position = float4(p.uv.x * 2.0 - 1.0, 1.0 - p.uv.y * 2.0, 0.0, 1.0);
    return p;
}
float gamma_sdr(float x) {
    // Use a branch: an inactive sqrt/pow must not poison the linear segment.
    if (x < .0031308) return 12.92*x;
    return 1.13005*sqrt(max(x-.00228,0.0))-.13448*x+.005719;
}
float3 to_sdr(float3 v) {return float3(gamma_sdr(v.r),gamma_sdr(v.g),gamma_sdr(v.b));}
float inverse_srgb(float x) { if(x<=.04045) return x/12.92; return pow((x+.055)/1.055,2.4); }
float3 linear_srgb(float3 v) {return float3(inverse_srgb(v.r),inverse_srgb(v.g),inverse_srgb(v.b));}
float hable(float x) { return (x*(.15*x+.05)+.004)/(x*(.15*x+.5)+.06)-.02/.3; }
// One output pixel covers dx/dy of the desktop in UV. When that footprint
// spans more than one desktop pixel (downscaling), average four bilinear
// taps spread over it so every covered pixel contributes; a single tap
// would skip pixels and alias thin text. 1:1 and upscaling keep one tap.
float3 sample_desktop(float2 uv, float2 dx, float2 dy) {
    float width, height;
    desktop.GetDimensions(width, height);
    float2 footprint=(abs(dx)+abs(dy))*float2(width,height);
    float3 rgb=desktop.SampleLevel(filtering,uv,0).rgb;
    if (max(footprint.x,footprint.y)>1.01) {
        float2 a=.25*(dx+dy), b=.25*(dx-dy);
        rgb=.25*(desktop.SampleLevel(filtering,uv+a,0).rgb+desktop.SampleLevel(filtering,uv-a,0).rgb
            +desktop.SampleLevel(filtering,uv+b,0).rgb+desktop.SampleLevel(filtering,uv-b,0).rgb);
    }
    return rgb;
}
float4 ps_main(Pixel p) : SV_TARGET {
    float2 uv=p.uv;
    if (params.x==2.0) uv=float2(p.uv.y,1.0-p.uv.x);
    else if (params.x==3.0) uv=1.0-p.uv;
    else if (params.x==4.0) uv=float2(1.0-p.uv.y,p.uv.x);
    float2 dx=ddx(uv), dy=ddy(uv);
    float3 rgb=sample_desktop(uv,dx,dy);
    if (params.y==1.0) {
        rgb=saturate(rgb/max(params.z,0.0001));
        rgb=to_sdr(rgb);
    } else if (params.y==2.0) {
        float peak=(params.w>0.0 && params.w<=10000.0) ? params.w : 1000.0;
        float maximum=max(max(rgb.r,rgb.g),rgb.b);
        if (maximum>0.0 && isfinite(maximum)) rgb *= hable(maximum)/(maximum*hable(max(peak/80.0,1.0)));
        else rgb=0.0;
        rgb=to_sdr(saturate(rgb));
    } else if (params.y==3.0) {
        rgb=linear_srgb(saturate(rgb))*params.z;
    }
    float2 pointer_pixel=p.uv*pointer_meta.xy-pointer_rect.xy;
    if (pointer_meta.w>0.5 && all(pointer_pixel>=0.0) && all(pointer_pixel<pointer_rect.zw)) {
        float4 shape=pointer_shape.Load(int3(int2(pointer_pixel),0));
        // HDR desktop and SDR cursor share the same linear scRGB workspace.
        // Logic masks still act on the displayed SDR cursor representation.
        bool hdr=params.y>=3.0;
        float3 untouched=rgb;
        if (hdr) rgb=to_sdr(saturate(rgb/max(params.z,.0001)));
        uint3 dest=(uint3)round(saturate(rgb)*255.0);
        if (pointer_meta.z==1.0) {
            uint and_mask=shape.r>0.5 ? 255 : 0;
            uint xor_mask=shape.g>0.5 ? 255 : 0;
            if(hdr && and_mask==255 && xor_mask==0) return float4(untouched,1.0);
            rgb=float3((dest & and_mask) ^ xor_mask)/255.0;
        } else if (pointer_meta.z==4.0) {
            rgb=shape.a>0.5 ? float3(dest ^ (uint3)round(shape.rgb*255.0))/255.0 : shape.rgb;
        } else {
            if (hdr) {
                // Restore untouched HDR highlights before alpha composition.
                float3 original=sample_desktop(uv,dx,dy);
                if(params.y==3.0) original=linear_srgb(saturate(original))*params.z;
                return float4(lerp(original,linear_srgb(shape.rgb)*params.z,shape.a),1.0);
            }
            rgb=lerp(rgb,shape.rgb,shape.a);
        }
        if(hdr) rgb=linear_srgb(saturate(rgb))*params.z;
    }
    return float4(rgb,1.0);
}
