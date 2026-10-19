Texture2D<float4> current_frame : register(t0);
SamplerState linear_sampler : register(s0);
cbuffer Timing : register(b1) { float phase; float input_width; float input_height; float history_valid; };
cbuffer Parameters : register(b2) { float4 settings; };
float4 main(float4 position:SV_POSITION,float2 uv:TEXCOORD0):SV_TARGET {
    float4 c=current_frame.Sample(linear_sampler,uv);
    if(settings.x==0) return c;
    float2 step=float2(1.0/input_width,1.0/input_height);
    float3 neighbors=current_frame.Sample(linear_sampler,uv+float2(step.x,0)).rgb
        +current_frame.Sample(linear_sampler,uv-float2(step.x,0)).rgb
        +current_frame.Sample(linear_sampler,uv+float2(0,step.y)).rgb
        +current_frame.Sample(linear_sampler,uv-float2(0,step.y)).rgb;
    return float4(saturate(c.rgb+settings.x*(4*c.rgb-neighbors)),c.a);
}
