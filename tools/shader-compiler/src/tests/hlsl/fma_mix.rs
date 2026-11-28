use crate::hlsl::float64::tests::translate;
use crate::Stage;

// v_fma_mix_f32 v3, v1, SRC1, v4 (VOP3P opcode 0x20).
fn mix(clamp: bool, src1: u32, half_sources: u32) -> String {
    let w0 = 0xcc20_0003 | u32::from(clamp) << 15 | (half_sources >> 2 & 1) << 14;
    let w1 = 257 | src1 << 9 | 260 << 18 | (half_sources & 3) << 27;
    translate(Stage::Compute, &[w0, w1]).unwrap()
}

#[test]
fn mixed_fma_applies_clamp() {
    assert!(!mix(false, 258, 0).contains("saturate("));
    let hlsl = mix(true, 258, 0);
    assert!(hlsl.contains("saturate(mad(asfloat(v1), asfloat(v2), asfloat(v4)))"), "{hlsl}");
}

#[test]
fn half_inline_constant_is_a_half_value() {
    // SRC1 = 1.0 (selector 242), read as a 16-bit source.
    let hlsl = mix(false, 242, 2);
    assert!(hlsl.contains("mad(asfloat(v1), asfloat(0x3f800000u), asfloat(v4))"), "{hlsl}");
    // 1/(2*pi) (selector 248) has its own binary16 value.
    let hlsl = mix(false, 248, 2);
    assert!(hlsl.contains("asfloat(0x3e230000u)"), "{hlsl}");
}
