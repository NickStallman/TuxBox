use super::*;

fn insn(mnemonic: &str, enc: Enc, dst: Vec<Opnd>, src: Vec<Opnd>) -> Insn {
    Insn { offset: 0, dwords: 1, enc, op: 0, raw: vec![0], mnemonic: mnemonic.into(), dst, src,
        aux: Default::default() }
}

/// `nop` padding, then GETPC s[0:1]; ADD s0, lo; ADDC s1, hi; SETPC s[0:1] at
/// `at`, and a `nop` at every later offset up to `len` dwords.
fn long_jump(at: usize, low: Opnd, high: i64, len: usize) -> Vec<Insn> {
    let nop = || insn("s_nop", Enc::Sopp, vec![], vec![]);
    let mut ops: Vec<Insn> = (0..at).map(|_| nop()).collect();
    ops.push(insn("s_getpc_b64", Enc::Sop1, vec![Opnd::Sgpr(0)], vec![Opnd::Sgpr(0)]));
    ops.push(insn("s_add_u32", Enc::Sop2, vec![Opnd::Sgpr(0)], vec![Opnd::Sgpr(0), low]));
    ops.push(insn("s_addc_u32", Enc::Sop2, vec![Opnd::Sgpr(1)], vec![Opnd::Sgpr(1), Opnd::ConstInt(high)]));
    ops.push(insn("s_setpc_b64", Enc::Sop1, vec![Opnd::Sgpr(0)], vec![Opnd::Sgpr(0)]));
    while ops.len() < len {
        ops.push(nop());
    }
    for (i, op) in ops.iter_mut().enumerate() {
        op.offset = i * 4;
    }
    ops
}

fn branch_target(op: &Insn) -> Option<i64> {
    (op.enc == Enc::Sopp && op.op == 2).then(|| op.offset as i64 + 4 + 4 * op.aux.imm)
}

#[test]
fn long_jumps_with_a_carried_high_word_become_branches() {
    // GETPC at 0x0 returns 0x4; +0x20 lands on dword 9.
    let mut ops = long_jump(0, Opnd::Literal(0x20), 0, 12);
    lower_local_pc_transfers(&mut ops).unwrap();
    assert_eq!(branch_target(&ops[3]), Some(0x24));
    // Backward: GETPC at 0x20 returns 0x24; -0x24 (high word -1) lands on dword 0.
    let mut ops = long_jump(8, Opnd::Literal(0u32.wrapping_sub(0x24)), -1, 14);
    lower_local_pc_transfers(&mut ops).unwrap();
    assert_eq!(branch_target(&ops[11]), Some(0));
}

#[test]
fn unproven_long_jumps_stay_indirect() {
    // A target outside the program, or a high word that is not a sign or zero
    // extension, leaves the transfer untouched.
    for (low, high) in [(Opnd::Literal(0x400), 0), (Opnd::Literal(0x20), 1)] {
        let mut ops = long_jump(0, low, high, 12);
        lower_local_pc_transfers(&mut ops).unwrap();
        assert_eq!(ops[3].enc, Enc::Sop1);
        assert_eq!(ops[3].mnemonic, "s_setpc_b64");
    }
}

/// The waterfall loop of a ProjectScarlett wave64 compute program: each lane
/// reads v5 at the lane its v8 names.
fn waterfall_words(after: &[u32]) -> Vec<Insn> {
    let mut words = vec![
        0xbe86_047e,              // s_mov_b64 s[6:7], exec
        0x7e20_0508,              // loop: v_readfirstlane_b32 s16, v8
        0x7da4_1010,              // v_cmpx_eq_u32 vcc, s16, v8
        0xd760_0010, 0x0000_2105, // v_readlane_b32 s16, v5, s16
        0x7e16_0210,              // v_mov_b32 v11, s16
        0xbe86_3a06,              // s_andn2_wrexec_b64 s[6:7], s[6:7]
        0xbf89_fff9,              // s_cbranch_execnz loop
        0xbefe_0404,              // s_mov_b64 exec, s[4:5]
    ];
    words.extend_from_slice(after);
    words.push(0xbf81_0000); // s_endpgm
    disassemble(&words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<_>>())
}

#[test]
fn waterfall_lane_reads_become_one_group_gather() {
    let mut ops = waterfall_words(&[]);
    assert_eq!(lower_waterfall_gathers(&mut ops), 1);
    assert_eq!(ops[1].mnemonic, GROUP_GATHER);
    assert_eq!(ops[1].src, [Opnd::Vgpr(5), Opnd::Vgpr(8)]);
    assert_eq!(ops[1].dst, [Opnd::Vgpr(11)]);
    assert!(ops[2..7].iter().all(|op| op.mnemonic == "v_nop"), "{:?}", ops.iter().map(|o| &o.mnemonic).collect::<Vec<_>>());
    assert_eq!(ops[7].mnemonic, "s_mov_b64");
}

#[test]
fn waterfall_scalar_leftovers_must_be_dead() {
    // s_add_u32 s1, s16, 1 reads the last index the loop left in s16.
    let mut ops = waterfall_words(&[0x8001_8110]);
    assert_eq!(lower_waterfall_gathers(&mut ops), 0);
    // A write first makes it dead: s_mov_b32 s16, 0 then the same read.
    let mut ops = waterfall_words(&[0xbe90_0380, 0x8001_8110]);
    assert_eq!(lower_waterfall_gathers(&mut ops), 1);
}

/// A raster first-lane read: `s0` names the first active lane, `v43` is read
/// there, and the result is consumed before a branch. `after` follows the
/// branch, with `s_endpgm` last.
fn first_lane_words(after: &[u32]) -> Vec<Insn> {
    let mut words = vec![
        0xbe80_047e,              // s_mov_b64 s0, exec
        0xbe80_1400,              // s_ff1_i32_b64 s0, s0
        0xd760_0012, 0x0000_012b, // v_readlane_b32 s18, v43, s0
        0x8012_8112,              // s_add_u32 s18, s18, 1
        0xbf85_0002,              // s_cbranch_scc1 over two nops
        0xbf80_0000,              // s_nop
        0xbf80_0000,              // s_nop
    ];
    words.extend_from_slice(after);
    words.push(0xbf81_0000); // s_endpgm
    disassemble(&words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<_>>())
}

#[test]
fn first_lane_read_survives_a_branch_once_its_result_is_consumed() {
    let mut ops = first_lane_words(&[]);
    lower_first_lane_reads(&mut ops);
    assert_eq!(ops[1].mnemonic, "s_mov_b32");
    assert_eq!(ops[1].src, [Opnd::ConstInt(0)]);
    assert_eq!(ops[2].mnemonic, "v_readfirstlane_b32");
    assert_eq!(ops[2].dst, [Opnd::Sgpr(18)]);
    assert_eq!(ops[2].src, [Opnd::Vgpr(43)]);
}

#[test]
fn first_lane_read_stays_put_while_the_index_is_still_live() {
    // s_add_u32 s0, s0, 1 after the branch reads the elected lane.
    let mut ops = first_lane_words(&[0x8000_8100]);
    lower_first_lane_reads(&mut ops);
    assert_eq!(ops[1].mnemonic, "s_ff1_i32_b64");
    assert_eq!(ops[2].mnemonic, "v_readlane_b32");
}

#[test]
fn first_lane_read_stays_put_when_a_branch_reaches_a_use_of_the_index() {
    // The branch skips `s_mov_b32 s0, 0`; its target adds 1 to the elected index.
    let words = [0xbe80_047eu32, 0xbe80_1400, 0xd760_0012, 0x0000_012b, 0xbf85_0001,
        0xbe80_0380, 0x8001_8100, 0xbf81_0000];
    let mut skipped = disassemble(&words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<_>>());
    lower_first_lane_reads(&mut skipped);
    assert_eq!(skipped[2].mnemonic, "v_readlane_b32");
    // With the use gone, both paths drop the index and the read is lowered.
    let words = [0xbe80_047eu32, 0xbe80_1400, 0xd760_0012, 0x0000_012b, 0xbf85_0001,
        0xbe80_0380, 0xbf80_0000, 0xbf81_0000];
    let mut ops = disassemble(&words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<_>>());
    lower_first_lane_reads(&mut ops);
    assert_eq!(ops[2].mnemonic, "v_readfirstlane_b32");
}

#[test]
fn first_lane_read_stays_put_across_a_saveexec() {
    // s_and_saveexec_b64 s[2:3], vcc narrows EXEC after the election.
    let words = [0xbe80_047eu32, 0xbe80_1400, 0xbe82_246a, 0xd760_0012, 0x0000_012b, 0xbf81_0000];
    let mut ops = disassemble(&words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<_>>());
    lower_first_lane_reads(&mut ops);
    assert_eq!(ops[3].mnemonic, "v_readlane_b32");
}
