use super::*;
use crate::rdna2::disassemble;

/// The 64-lane exclusive prefix sum of v18 into v2 from a ProjectScarlett culling
/// program (9dfb6122 at 0x31dc): v3 takes the lower rows' totals and s50 the lower
/// half's, and neither is read again.
const SCAN: [u32; 28] = [
    0xbeb0_25c1,              // s_or_saveexec_b64 s[48:49], -1
    0xd501_0001, 0x00c2_2480, // v_cndmask_b32 v1, 0, v18, s[48:49]
    0x7e04_0301,              // v_mov_b32 v2, v1
    0x4a04_04fa, 0xff09_1102, // v_add_nc_u32 v2, row_shr:1 bound_ctrl
    0x4a04_04fa, 0xff01_1202, // row_shr:2
    0x4a04_04fa, 0xff01_1402, // row_shr:4
    0x4a04_04fa, 0xff01_1802, // row_shr:8
    0xbefe_03ff, 0xffff_0000, // s_mov_b32 exec_lo, 0xffff0000
    0xbeff_03ff, 0xffff_0000, // s_mov_b32 exec_hi, 0xffff0000
    0xd778_4803, 0x0305_8302, // v_permlanex16_b32 v3, v2, -1, -1 fi
    0x4a04_0503,              // v_add_nc_u32 v2, v3, v2
    0xd760_0032, 0x0001_3f02, // v_readlane_b32 s50, v2, 31
    0xbefe_0380,              // s_mov_b32 exec_lo, 0
    0xbeff_03c1,              // s_mov_b32 exec_hi, -1
    0x4a04_0432,              // v_add_nc_u32 v2, s50, v2
    0xbefe_03c1, 0xbeff_03c1, // s_mov_b32 exec_lo, -1; s_mov_b32 exec_hi, -1
    0x4c04_0302,              // v_sub_nc_u32 v2, v2, v1
    0xbefe_0430,              // s_mov_b64 exec, s[48:49]
];

fn program(before: &[u32], after: &[u32]) -> Vec<Insn> {
    let words = [before, &SCAN[..], after, &[0xbf81_0000]].concat();
    disassemble(&words.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<u8>>())
}

/// Whether the scan becomes one prefix sum over the wave.
fn lowered(before: &[u32], after: &[u32]) -> bool {
    let mut insns = program(before, after);
    let width = prepare(&mut insns, Stage::Compute).unwrap();
    let found = insns.iter().any(|i| i.mnemonic == PREFIX);
    assert_eq!(width, found.then_some(64));
    assert_eq!(found, !insns.iter().any(|i| i.mnemonic == "v_permlanex16_b32"));
    found
}

#[test]
fn a_scan_whose_scratch_dies_is_a_prefix_sum() {
    assert!(lowered(&[], &[]));
    // The cross-row temporary and the half-wave total are read afterwards.
    assert!(!lowered(&[], &[0x7e08_0303])); // v_mov_b32 v4, v3
    assert!(!lowered(&[], &[0x8014_8132])); // s_add_u32 s20, s50, 1
}

#[test]
fn a_scan_in_a_loop_reads_only_the_temporary_it_wrote() {
    // s_add_u32 s20, s20, 1; s_cmp_lt_u32 s20, 3; s_cbranch_scc1 back to the scan.
    // On the way round, the scan's own add reads v3 in the lanes its permute wrote.
    let back = 0xbf85_0000 | (-(SCAN.len() as i32 + 3) as u16 as u32);
    assert!(lowered(&[], &[0x8014_8114, 0xbf0a_8314, back]));
    // A write under the saved EXEC defines v3 only while that EXEC lasts.
    let partial = 0x7e06_0301; // v_mov_b32 v3, v1
    assert!(lowered(&[], &[partial, 0x7e08_0303]));
    assert!(!lowered(&[], &[partial, 0xbefe_04c1, 0x7e08_0303])); // s_mov_b64 exec, -1 between
    // A lane read by number may name a lane that EXEC did not have.
    assert!(!lowered(&[], &[partial, 0xd760_0014, 0x0001_0b03])); // v_readlane_b32 s20, v3, 5
}

#[test]
fn operands_beside_the_scratch_do_not_keep_it_alive() {
    // The address in v0 of a 32-bit LDS read and of an indexed buffer atomic is one register.
    assert!(lowered(&[], &[0xd8dc_0100, 0x0600_0000])); // ds_read2_b32 v[6:7], v0
    assert!(lowered(&[], &[0xe0e8_2000, 0x8000_0700])); // buffer_atomic_or v7, v0, s[0:3], 0 idxen
    // An index and an offset are two.
    let reads = |r, words: &[u32]| reads(&program(&[], words)[SCAN_INSNS], r, true);
    assert!(reads(Reg::V(0), &[0xe0e8_2000, 0x8000_0700]));
    assert!(!reads(Reg::V(1), &[0xe0e8_2000, 0x8000_0700]));
    assert!(reads(Reg::V(1), &[0xe0e8_3000, 0x8000_0700])); // idxen offen
    assert!(!reads(Reg::V(2), &[0xe0e8_3000, 0x8000_0700]));
    // The scalar addend s49 of a vector instruction is not the first word of a mask...
    assert!(lowered(&[], &[0xd746_0007, 0x00c5_0506])); // v_lshl_add_u32 v7, v6, 2, s49
    // ...as the third source of a select is.
    assert!(!lowered(&[], &[0xd501_0007, 0x00c6_0d07])); // v_cndmask_b32 v7, v7, v6, s[49:50]
}

/// Instructions in `SCAN`.
const SCAN_INSNS: usize = 19;
