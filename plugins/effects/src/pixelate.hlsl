Texture2D<float4> current_frame : register(t0);
SamplerState linear_sampler : register(s0);
cbuffer Timing : register(b1) { float phase; float input_width; float input_height; float history_valid; };
cbuffer Parameters : register(b2) { float4 settings; };
float4 main(float4 position:SV_POSITION,float2 uv:TEXCOORD0):SV_TARGET {
    if(settings.x<=1) return current_frame.Sample(linear_sampler,uv);
    float2 size=float2(input_width,input_height);
    float2 at=(floor(uv*size/settings.x)*settings.x+settings.x*0.5)/size;
    return current_frame.Sample(linear_sampler,at);
}
