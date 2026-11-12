#version 450
layout(push_constant) uniform Parameters { uint seed; uint iterations; } params;
layout(location = 0) out vec4 colour;
void main() {
    uint v = uint(gl_FragCoord.x) * 1664525u ^ uint(gl_FragCoord.y) * 1013904223u ^ params.seed;
    for (uint i = 0u; i < params.iterations; ++i) {
        v = (v ^ (v >> 16u)) * 2246822519u;
        v = (v ^ (v >> 13u)) * 3266489917u;
    }
    colour = vec4(float(v & 255u), float((v >> 8u) & 255u), float((v >> 16u) & 255u), 255.0) / 255.0;
}
