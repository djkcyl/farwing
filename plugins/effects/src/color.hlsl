Texture2D<float4> current_frame : register(t0);
SamplerState linear_sampler : register(s0);
cbuffer Timing : register(b1) { float phase; float input_width; float input_height; float history_valid; };
cbuffer Parameters : register(b2) { float4 settings; };
float4 main(float4 position:SV_POSITION,float2 uv:TEXCOORD0):SV_TARGET {
    float4 c=current_frame.Sample(linear_sampler,uv);
    if(settings.x==0 && settings.y==1 && settings.z==1) return c;
    float y=dot(c.rgb,float3(0.2126,0.7152,0.0722));
    float3 rgb=lerp(y.xxx,c.rgb,settings.z);
    return float4(saturate((rgb-0.5)*settings.y+0.5+settings.x),c.a);
}
