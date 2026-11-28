//! A heap descriptor copied to vector registers and served by a waterfall loop.
use super::heap_resinfo::{fssm_rows, heap_extract};
use super::{translate, translate_paired_wave64};

/// Assembled with llvm-mc for gfx1030, 64-lane waves:
///
/// ```text
///         v_readfirstlane_b32 s24, v2          ; a heap index, the same in every lane
///         s_lshl_b32 s24, s24, 5
///         s_load_dwordx8 s[12:19], s[10:11], s24
///         v_mov_b32 v57..v64, s12..s19         ; eight copies
///         s_mov_b64 s[2:3], exec
///   loop: v_readfirstlane_b32 s32..s39, v57..v64
///         v_cmp_eq_u64 vcc, s[32:33], v[57:58]
///         v_cmp_eq_u64 s[0:1], s[34:35], v[59:60]
///         s_and_b64 s[0:1], vcc, s[0:1]
///         s_and_saveexec_b64 s[0:1], s[0:1]
///         image_load v10, v[4:5], s[32:39] dmask:0x1 dim:SQ_RSRC_IMG_2D
///         s_xor_b64 exec, exec, s[0:1]
///         s_cbranch_execnz loop
///         s_mov_b64 exec, s[2:3]
///         s_endpgm
/// ```
pub(super) const WATERFALL: [u32; 32] = [
    0x7e300502, 0x8f188518, 0xf40c0305, 0x30000000,
    0x7e72020c, 0x7e74020d, 0x7e76020e, 0x7e78020f, 0x7e7a0210, 0x7e7c0211, 0x7e7e0212, 0x7e800213,
    0xbe82047e,
    0x7e400539, 0x7e42053a, 0x7e44053b, 0x7e46053c, 0x7e48053d, 0x7e4a053e, 0x7e4c053f, 0x7e4e0540,
    0x7dc47220, 0xd4e20000, 0x00027622, 0x8780006a, 0xbe802400,
    0xf0000108, 0x00080a04, 0x89fe007e, 0xbf89ffef, 0xbefe0402, 0xbf810000,
];

fn bytes(words: &[u32]) -> Vec<u8> { words.iter().flat_map(|w| w.to_le_bytes()).collect() }

#[test]
fn the_descriptor_read_back_indexes_the_heap() {
    for hlsl in [translate(&heap_extract(&bytes(&WATERFALL))), translate_paired_wave64(&heap_extract(&bytes(&WATERFALL)))] {
        let hlsl = hlsl.unwrap().hlsl;
        assert!(hlsl.contains("= ResourceDescriptorHeap[s32];"), "{hlsl}");
    }
}

#[test]
fn a_wave_with_no_active_lane_reads_heap_slot_zero() {
    // The console reads lane 0 and then loads nothing. Here the load still runs,
    // so the index must not be whatever lane 0 holds.
    let paired = translate_paired_wave64(&heap_extract(&bytes(&WATERFALL))).unwrap().hlsl;
    assert_eq!(paired.matches("if ((exec_lo | exec_hi) == 0u) s32 = 0u;").count(), 1, "{paired}");
    // Only the index word: the other descriptor words stay as read.
    assert_eq!(paired.matches("== 0u) s3").count(), 1, "{paired}");
    let single = translate(&heap_extract(&bytes(&WATERFALL))).unwrap().hlsl;
    assert_eq!(single.matches(" == 0xffffffffu ? 0u : WaveReadLaneAt(v57, ").count(), 1, "{single}");
    assert_eq!(single.matches(" == 0xffffffffu ? 0u : WaveReadLaneAt(").count(), 1, "{single}");
}

#[test]
fn a_register_written_after_the_copy_is_not_a_descriptor() {
    // v_mov_b32 v57, v3 after the copies: the read no longer returns s12.
    let mut words = WATERFALL.to_vec();
    words[12] = 0x7e720303;
    words.insert(12, 0xbe82047e);
    words[30] = 0xbf89ffef;
    for refused in [translate(&heap_extract(&bytes(&words))), translate_paired_wave64(&heap_extract(&bytes(&words)))] {
        let reason = refused.map(|t| t.hlsl).unwrap_err();
        assert!(reason.contains("s32 not an image descriptor"), "{reason}");
    }
}

/// A texture and a sampler from the heaps, sampled at each thread's own coordinates
/// with implicit derivatives:
///
/// ```text
///   v_readfirstlane_b32 s24, v2;  s_lshl_b32 s24, s24, 5;  s_load_dwordx8 s[32:39], s[10:11], s24
///   s_lshl_b32 s25, s24, 4;  s_load_dwordx4 s[40:43], s[14:15], s25
///   v_cvt_f32_u32 v4, v0;  v_cvt_f32_u32 v5, v1
///   image_sample v10, v[4:5], s[32:39], s[40:43] dmask:0x1 dim:SQ_RSRC_IMG_2D
/// ```
pub(super) const SAMPLE: [u32; 12] = [
    0x7e300502, 0x8f188518, 0xf40c0805, 0x30000000, 0x8f198418, 0xf4080a07, 0x32000000,
    0x7e080d00, 0x7e0a0d01, 0xf0800108, 0x01480a04, 0xbf810000,
];

fn sampling(group: [u32; 3]) -> crate::ShaderExtract {
    let mut ex = heap_extract(&bytes(&SAMPLE));
    // The resource heap in s10 and the sampler heap in s14.
    ex.fssm = fssm_rows(&[[5, 0, 1, 0, 10], [6, 0, 1, 0, 14]]);
    ex.psv_threads = Some(group);
    ex
}

#[test]
fn derivative_quads_follow_the_group_shape_in_the_two_lane_form() {
    // One row: lanes are the threads in order, four in a row to a quad.
    let row = translate_paired_wave64(&sampling([64, 1, 1])).unwrap().hlsl;
    assert!(row.contains("v0__[1] = (tb_first + tb_pair + 32u) % 64u;"), "{row}");
    // Rows: lanes 4k to 4k+3 are the two-by-two block k.
    let rows = translate_paired_wave64(&sampling([8, 8, 1])).unwrap().hlsl;
    assert!(rows.contains("[numthreads(32, 1, 1)]"), "{rows}");
    assert!(rows.contains("v0__[0] = 2u * (((tb_first + tb_pair + 0u) / 4u) % 4u) + ((tb_first + tb_pair + 0u) & 1u); \
v1__[0] = 2u * (((tb_first + tb_pair + 0u) / 4u) / 4u) + (((tb_first + tb_pair + 0u) >> 1u) & 1u); v2__[0] = 0u;"), "{rows}");
    assert!(translate_paired_wave64(&sampling([16, 8, 1])).unwrap().hlsl.contains(") / 4u) % 8u) + "));
    // The sample is written once and differenced within each lane's own copy.
    assert_eq!(rows.matches(".Sample(").count(), 1, "{rows}");
    // No two-by-two blocks in an odd width, and none across a third dimension.
    for group in [[4, 4, 4], [1, 64, 1]] {
        assert!(translate_paired_wave64(&sampling(group)).is_err(), "{group:?}");
    }
}
