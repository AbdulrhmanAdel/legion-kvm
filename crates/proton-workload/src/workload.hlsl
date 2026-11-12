cbuffer Frame : register(b0) { uint seed; uint rounds; };

float4 vs_main(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
}

float4 ps_main(float4 position : SV_Position) : SV_Target {
    uint v = (uint(position.x) + uint(position.y) * 65537u) ^ seed;
    [loop] for (uint i = 0; i < rounds; ++i) {
        v ^= v >> 16;
        v *= 0x7feb352du;
        v ^= v >> 15;
        v = v * 0x846ca68bu + i;
    }
    return float4(v & 255u, (v >> 8) & 255u, (v >> 16) & 255u, 255u) / 255.0;
}
