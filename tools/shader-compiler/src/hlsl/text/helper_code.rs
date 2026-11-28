//! HLSL functions pasted into translations that need them.
pub(in crate::hlsl) const XYZW: [&str; 4] = ["x", "y", "z", "w"];

/// RDNA2 cube-map helpers. v_cube{ma,sc,tc,id}_f32 follow the ISA definition (major axis ×2,
/// face-local S/T, face index 0..5 = +X,-X,+Y,-Y,+Z,-Z). Hardware cube sampling takes
/// (s, t, face) with s,t = sc/|ma| + 1.5 (so each is in [1, 2]); `tb_cube_dir` inverts that
/// to the direction vector HLSL TextureCube sampling expects.
pub(in crate::hlsl) const CUBE_HELPERS: &str = r#"uint tb_perm_byte(uint hi, uint lo, uint sel) { if (sel >= 13u) return 0xffu; if (sel == 12u) return 0u; if (sel == 11u) return (hi >> 31) * 0xffu; if (sel == 10u) return ((hi >> 15) & 1u) * 0xffu; if (sel == 9u) return (lo >> 31) * 0xffu; if (sel == 8u) return ((lo >> 15) & 1u) * 0xffu; return sel >= 4u ? ((hi >> ((sel - 4u) * 8u)) & 0xffu) : ((lo >> (sel * 8u)) & 0xffu); }
uint tb_perm(uint hi, uint lo, uint sel) { return tb_perm_byte(hi, lo, sel & 0xffu) | (tb_perm_byte(hi, lo, (sel >> 8) & 0xffu) << 8) | (tb_perm_byte(hi, lo, (sel >> 16) & 0xffu) << 16) | (tb_perm_byte(hi, lo, sel >> 24) << 24); }
bool tb_cmp_class(uint b, uint mask, uint fractionBits, uint exponentMask) {
    uint fractionUnit = 1u << fractionBits;
    uint e = (b >> fractionBits) & exponentMask;
    uint m = b & (fractionUnit - 1u);
    bool neg = (b & ((exponentMask + 1u) << fractionBits)) != 0u;
    uint c;
    if (e == exponentMask) { c = (m != 0u) ? (((m & (fractionUnit >> 1)) != 0u) ? 1u : 0u) : (neg ? 2u : 9u); } else if (e == 0u) { c = (m == 0u) ? (neg ? 5u : 6u) : (neg ? 4u : 7u); } else { c = neg ? 3u : 8u; } return ((mask >> c) & 1u) != 0u; }uint tb_bfe_i32(uint a, uint b) { uint o = b & 31u, w = (b >> 16) & 127u; if (w == 0u) return 0u; if (w + o >= 32u) return asuint(asint(a) >> o); return asuint(asint(a << (32u - w - o)) >> (32u - w)); }uint tb_mulhi_u32(uint a, uint b) { uint a0 = a & 0xffffu, a1 = a >> 16, b0 = b & 0xffffu, b1 = b >> 16; uint p00 = a0 * b0, p01 = a0 * b1, p10 = a1 * b0, p11 = a1 * b1; uint mid = (p00 >> 16) + (p01 & 0xffffu) + (p10 & 0xffffu); return p11 + (p01 >> 16) + (p10 >> 16) + (mid >> 16); }
uint tb_dot4_i8(uint a, uint b, uint acc) {
    [unroll] for (uint i = 0; i < 4; ++i) {
        acc += asuint((asint(a << 24) >> 24) * (asint(b << 24) >> 24));
        a >>= 8; b >>= 8;
    }
    return acc;
}
uint tb_mulhi_i32(uint a, uint b) { uint h = tb_mulhi_u32(a, b); if (asint(a) < 0) h -= b; if (asint(b) < 0) h -= a; return h; }
float tb_cubema(float x, float y, float z) { float ax = abs(x), ay = abs(y), az = abs(z); if (az >= ax && az >= ay) return 2.0 * z; if (ay >= ax) return 2.0 * y; return 2.0 * x; }
float tb_cubesc(float x, float y, float z) { float ax = abs(x), ay = abs(y), az = abs(z); if (az >= ax && az >= ay) return z < 0.0 ? -x : x; if (ay >= ax) return x; return x < 0.0 ? z : -z; }
float tb_cubetc(float x, float y, float z) { float ax = abs(x), ay = abs(y), az = abs(z); if (az >= ax && az >= ay) return -y; if (ay >= ax) return y < 0.0 ? -z : z; return -y; }
float tb_cubeid(float x, float y, float z) { float ax = abs(x), ay = abs(y), az = abs(z); if (az >= ax && az >= ay) return z < 0.0 ? 5.0 : 4.0; if (ay >= ax) return y < 0.0 ? 3.0 : 2.0; return x < 0.0 ? 1.0 : 0.0; }
float3 tb_cube_dir(float s, float t, float face) { float u = (s - 1.5) * 2.0, v = (t - 1.5) * 2.0; int f = (int)round(face); if (f == 0) return float3(1.0, -v, -u); if (f == 1) return float3(-1.0, -v, u); if (f == 2) return float3(u, 1.0, v); if (f == 3) return float3(u, -1.0, -v); if (f == 4) return float3(u, -v, 1.0); return float3(-u, -v, -1.0); }
float tb_mull(float a, float b) { return (a == 0.0 || b == 0.0) ? 0.0 : a * b; }
// A host's log2 can be exact only to an absolute error, which is far more than
// one unit of a result close to zero. Close to 1 the logarithm comes from the
// series of 2 atanh((x - 1) / (x + 1)), whose error is relative to the result.
float tb_log2(float x) {
    precise float d = x - 1.0;
    precise float s = d / (2.0 + d);
    precise float q = s * s;
    precise float near = 2.885390081777927 * s * (1.0 + q * (0.3333333333 + q * (0.2 + q * (0.1428571429 + q * (0.1111111111 + q * 0.0909090909)))));
    return (x > 0.75 && x < 1.5) ? near : log2(x);
}
uint tb_f16_cvt(float x, bool rtz) {
    uint u = asuint(x);
    uint s = (u >> 16) & 0x8000u;
    uint a = u & 0x7fffffffu;
    uint nearest = uint(!rtz);
    // Computed without branches, as a program inlines this at every conversion.
    // Each range of magnitudes has a result; the last matching selection wins.
    uint r = (a >> 13) - 0x1c000u;
    uint rem = a & 0x1fffu;
    r += nearest & (uint(rem > 0x1000u) | (uint(rem == 0x1000u) & r & 1u));
    uint sh = (126u - (a >> 23)) & 31u;
    uint mant = (a & 0x7fffffu) | 0x800000u;
    uint q = mant >> sh;
    uint lost = mant & ((1u << sh) - 1u);
    uint half = 1u << ((sh - 1u) & 31u);
    q += nearest & (uint(lost > half) | (uint(lost == half) & q & 1u));
    uint value = s | q;
    value ^= (value ^ s) & (0u - uint(a < 0x33000000u));
    value ^= (value ^ (s | r)) & (0u - uint(a >= 0x38800000u));
    value ^= (value ^ (s | (0x7c00u - uint(rtz)))) & (0u - uint(a >= 0x477ff000u - 0x1000u * uint(rtz)));
    value ^= (value ^ (s | 0x7c00u)) & (0u - uint(a == 0x7f800000u));
    value ^= (value ^ (s | 0x7e00u | ((a >> 13) & 0x3ffu))) & (0u - uint(a > 0x7f800000u));
    return value;
}
"#;

pub(in crate::hlsl) const MAD24_SAT: &str = r#"
uint tb_mad24_sat(uint a, uint b, uint acc, bool isSigned) {
    a = isSigned ? asuint(asint(a << 8) >> 8) : (a & 0xffffffu);
    b = isSigned ? asuint(asint(b << 8) >> 8) : (b & 0xffffffu);
    uint product = a * b;
    uint low = product + acc;
    uint carry = uint(low < product);
    if (!isSigned) return tb_mulhi_u32(a, b) + carry == 0u ? low : 0xffffffffu;
    // Add the sign-extended accumulator before testing the full sum for overflow.
    int high = asint(tb_mulhi_i32(a, b)) + (asint(acc) >> 31) + int(carry);
    if (high == (asint(low) >> 31)) return low;
    return high < 0 ? 0x80000000u : 0x7fffffffu;
}
"#;

pub(in crate::hlsl) const HALF_RNE: &str = "uint tb_f16_rne(float x) { return tb_f16_cvt(x, false); }\n";

// The branch-free implementation keeps the driver control-flow graph small
// when a filter applies the conversion hundreds of times per invocation.
pub(in crate::hlsl) const HALF_RNE_STRAIGHT_LINE: &str = r#"uint tb_f16_rne(float x) {
    uint bits = asuint(x), a = bits & 0x7fffffffu;
    uint sign = (bits >> 16) & 0x8000u;
    uint normal = min(((a + 0xfffu + ((a >> 13) & 1u)) >> 13) - 0x1c000u, 0x7c00u);
    // FP32 spacing at 0.5 is exactly one half-subnormal unit, 2^-24.
    precise float sub = 0.5f + asfloat(a);
    uint finite = a >= 0x38800000u ? normal : asuint(sub) - 0x3f000000u;
    return sign | (a > 0x7f800000u ? 0x7e00u | ((a >> 13) & 0x3ffu) : finite);
}
// The min/max of two half values is already representable, including after
// saturation. Keep the guest quiet-NaN payload rule for the exceptional case.
uint tb_repack_half(float value) {
    uint bits = asuint(value);
    return (bits & 0x7fffffffu) > 0x7f800000u ? (((bits >> 16u) & 0x8000u) | 0x7e00u | ((bits >> 13u) & 0x3ffu)) : f32tof16(value);
}
"#;

pub(in crate::hlsl) const HALF_HELPERS: &str = r#"uint tb_trig_f16(uint bits, bool cosine) {
    uint magnitude = bits & 0x7fffu;
    if (magnitude > 0x7c00u) return (bits & 0xffffu) | 0x200u;
    if (magnitude == 0x7c00u) return 0xfe00u;
    if (magnitude == 0u) return cosine ? 0x3c00u : (bits & 0x8000u);
    // Guest arguments are turns. Reduce before converting to radians so that
    // large integral turns and multiples of a quarter turn stay exact.
    float turn = frac(f16tof32(magnitude));
    if (cosine) turn = frac(turn + 0.25);
    float halfTurn = turn >= 0.5 ? turn - 0.5 : turn;
    float angle = halfTurn <= 0.25 ? halfTurn : 0.5 - halfTurn;
    uint result = tb_f16_rne(sin(angle * 6.283185307179586));
    bool negative = (turn >= 0.5) != (!cosine && (bits & 0x8000u) != 0u);
    return result | ((result != 0u && negative) ? 0x8000u : 0u);
}
uint tb_f16_rtz(float x) { return tb_f16_cvt(x, true); }
bool tb_isnan(float x) { return (asuint(x) & 0x7fffffffu) > 0x7f800000u; }
float tb_min(float a, float b) { return tb_isnan(a) ? b : (tb_isnan(b) ? a : (a < b ? a : b)); }
float tb_max(float a, float b) { return tb_isnan(a) ? b : (tb_isnan(b) ? a : (a > b ? a : b)); }
float tb_med3(float a, float b, float c) { if (tb_isnan(a) || tb_isnan(b) || tb_isnan(c)) return tb_min(tb_min(a, b), c); float mx = tb_max(tb_max(a, b), c); if (mx == a) return tb_max(b, c); if (mx == b) return tb_max(a, c); return tb_max(a, b); }
"#;

/// OMOD scales an instruction's rounded result by 2^k. It adds k to the exponent
/// bits, so no compiler can move the scale into a product and lose an overflow
/// to infinity: a `precise` multiply still came back finite on NVIDIA. With
/// output denormals off OMOD flushes them, so a result below the normal range
/// and a denormal input become a zero of the same sign. Infinity and NaN pass.
pub(in crate::hlsl) const OMOD: &str = r#"float tb_omod(float x, int k) {
    uint b = asuint(x), e = (b >> 23) & 0xffu, sign = b & 0x80000000u;
    if (e == 0xffu) return x;
    int n = int(e) + k;
    if (e == 0u || n <= 0) return asfloat(sign);
    if (n >= 255) return asfloat(sign | 0x7f800000u);
    return asfloat((b & 0x807fffffu) | (uint(n) << 23));
}
"#;
