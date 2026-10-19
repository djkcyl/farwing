// Independent implementation of the current ordinary desktop color contract.
Texture2D<float4> source : register(t0);
#if PLANAR_444
RWTexture2D<float> planar444 : register(u0);
#elif PLANAR
RWTexture2D<float> luma : register(u0);
RWTexture2D<float2> chroma : register(u1);
#else
RWTexture2D<float4> packed : register(u0);
#endif

float3 pq2020(float3 rgb) {
    float3 wide = float3(dot(rgb,float3(.627402,.329292,.043306)),
                        dot(rgb,float3(.069095,.919544,.011360)),
                        dot(rgb,float3(.016394,.088028,.895578)));
    float3 p = pow(saturate(wide*.008), 2610.0/16384.0);
    return pow((3424.0/4096.0+(2413.0/128.0)*p)/(1.0+(2392.0/128.0)*p),2523.0/32.0);
}
float3 yuv(float3 rgb) {
#if HDR
    rgb = pq2020(rgb);
    return float3(dot(rgb,float3(.2627,.678,.0593)),
                  dot(rgb,float3(-.139630,-.360370,.5))+512.0/1023.0,
                  dot(rgb,float3(.5,-.459786,-.040214))+512.0/1023.0);
#else
    float3 sdr = float3(dot(rgb,float3(.256788,.504129,.097906))+16.0/255.0,
                  dot(rgb,float3(-.148223,-.290993,.439216))+128.0/255.0,
                  dot(rgb,float3(.439216,-.367788,-.071427))+128.0/255.0);
#if SDR_TEN
    // Quantize to ten code bits, then store P010 in the high ten bits.
    float3 code = round(sdr * 1020.0);
#if PLANAR || PLANAR_444
    return code * (64.0 / 65535.0);
#else
    return code / 1023.0;
#endif
#else
    return sdr;
#endif
#endif
}
[numthreads(8,8,1)]
void cs_main(uint3 id : SV_DispatchThreadID) {
    uint width,height;
    source.GetDimensions(width,height);
    if (id.x>=width || id.y>=height) return;
    float3 rgb=source.Load(int3(id.xy,0)).rgb;
    float3 value=yuv(rgb);
#if PLANAR_444
#if HDR
    value = round(saturate(value) * 1023.0) * (64.0 / 65535.0);
#endif
    planar444[id.xy] = value.x;
    planar444[id.xy + uint2(0,height)] = value.y;
    planar444[id.xy + uint2(0,height*2)] = value.z;
#elif PLANAR
    luma[id.xy]=value.x;
    if ((id.x&1)==0 && (id.y&1)==0) {
        // The HDR producer averages linear light before its PQ conversion.
        rgb += source.Load(int3(id.xy+uint2(1,0),0)).rgb;
        rgb += source.Load(int3(id.xy+uint2(0,1),0)).rgb;
        rgb += source.Load(int3(id.xy+uint2(1,1),0)).rgb;
        chroma[id.xy/2]=yuv(rgb*.25).yz;
    }
#else
#if HDR || SDR_TEN
    packed[id.xy]=float4(value.y,value.x,value.z,1.0); // Y410: U,Y,V,A
#else
    packed[id.xy]=float4(value.z,value.y,value.x,1.0); // AYUV: V,U,Y,A
#endif
#endif
}
struct Pixel {float4 position:SV_POSITION;};
Pixel vs_main(uint id:SV_VertexID) {
    Pixel p;float2 uv=float2((id<<1)&2,id&2);
    p.position=float4(uv.x*2.0-1.0,1.0-uv.y*2.0,0.0,1.0);return p;
}
float4 ps_main(Pixel p):SV_TARGET {
    uint2 at=(uint2)p.position.xy;
#if PLANE == 1
    at*=2;
    float3 rgb=(source.Load(int3(at,0)).rgb+source.Load(int3(at+uint2(1,0),0)).rgb
        +source.Load(int3(at+uint2(0,1),0)).rgb+source.Load(int3(at+uint2(1,1),0)).rgb)*.25;
    return float4(yuv(rgb).yz,0.0,1.0);
#else
    float3 value=yuv(source.Load(int3(at,0)).rgb);
#if PLANAR
    return float4(value.x,0.0,0.0,1.0);
#elif HDR || SDR_TEN
    return float4(value.y,value.x,value.z,1.0);
#else
    return float4(value.z,value.y,value.x,1.0);
#endif
#endif
}
