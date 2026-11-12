cbuffer Frame : register(b0) { uint seed; uint width; uint iterations; };
struct Vertex { float4 position : SV_Position; };
Vertex vs_main(uint id : SV_VertexID) {
    Vertex v;
    v.position = float4(id == 2 ? 3.0 : -1.0, id == 1 ? 3.0 : -1.0, 0.0, 1.0);
    return v;
}
float4 color(Vertex input) {
    uint2 p = uint2(input.position.xy);
    uint load_bit = 0;
    if (iterations != 0) {
        uint state = seed ^ (p.x + p.y * width);
        [loop] for (uint i = 0; i < iterations; ++i)
            state = state * 1664525u + 1013904223u;
        load_bit = (state ^ (state >> 16)) & 1;
    }
    uint r = (p.x + seed) & 255;
    uint g = (p.y + (seed >> 1)) & 255;
    uint b = ((((p.x / 64) ^ (p.y / 64) ^ (seed >> 7)) & 1) ? 192 : 32) + load_bit;
    return float4(r,g,b,255) / 255.0;
}
float4 ps_main(Vertex input) : SV_Target0 { return color(input); }
float4 ps_overlay(Vertex input) : SV_Target0 { return color(input).bgra; }
