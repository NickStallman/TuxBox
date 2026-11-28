use super::*;
fn op(name: &str, enc: Enc, dst: Vec<Opnd>, src: Vec<Opnd>, offset: usize) -> Insn {
    Insn { offset, dwords: 1, enc, op: 0, raw: vec![0], mnemonic: name.into(), dst, src, aux: Default::default() }
}
#[test]
fn election_ignores_independent_single_word_moves() {
    let mut ops = vec![
        op("v_mbcnt_lo_u32_b32", Enc::Vop3, vec![Opnd::Vgpr(2)], vec![Opnd::ConstInt(-1), Opnd::ConstInt(0)], 0),
        op("s_ff1_i32_b32", Enc::Sop1, vec![Opnd::Sgpr(7)], vec![Opnd::ExecLo], 4),
        op("s_mov_b32", Enc::Sop1, vec![Opnd::Sgpr(8)], vec![Opnd::Sgpr(6)], 8),
        op("v_cmp_eq_u32", Enc::Vop3, vec![Opnd::VccLo], vec![Opnd::Vgpr(2), Opnd::Sgpr(7)], 12),
    ];
    assert!(verify_lane_election(&mut ops, 1, true).is_ok());
    ops[2].mnemonic = "s_mov_b64".into(); // s[6:7] reads the elected lane.
    assert!(verify_lane_election(&mut ops, 1, true).is_err());
    ops[2].mnemonic = "s_mov_b32".into();
    ops[2].dst = vec![Opnd::Sgpr(7)];
    assert!(verify_lane_election(&mut ops, 1, true).is_err());
}

#[test]
fn later_saveexec_uses_the_redefined_vcc() {
    let mut ops = vec![
        op("v_mbcnt_lo_u32_b32", Enc::Vop3, vec![Opnd::Vgpr(2)], vec![Opnd::ConstInt(-1), Opnd::ConstInt(0)], 0),
        op("s_ff1_i32_b32", Enc::Sop1, vec![Opnd::Sgpr(7)], vec![Opnd::ExecLo], 4),
        op("v_cmp_eq_u32", Enc::Vopc, vec![Opnd::VccLo], vec![Opnd::Sgpr(7), Opnd::Vgpr(2)], 8),
        op("v_cmp_eq_u32", Enc::Vopc, vec![Opnd::VccLo], vec![Opnd::ConstInt(0), Opnd::Vgpr(4)], 12),
        op("s_and_saveexec_b32", Enc::Sop1, vec![Opnd::Sgpr(20)], vec![Opnd::VccLo], 16),
    ];
    assert!(verify_lane_election(&mut ops, 1, true).is_ok());
    ops.remove(3);
    assert!(verify_lane_election(&mut ops, 1, true).is_err());
}

#[test]
fn symmetric_equality_still_requires_full_lane_count() {
    let mut ops = vec![
        op("v_mbcnt_lo_u32_b32", Enc::Vop3, vec![Opnd::Vgpr(2)], vec![Opnd::ConstInt(-1), Opnd::ConstInt(0)], 0),
        op("s_ff1_i32_b32", Enc::Sop1, vec![Opnd::Sgpr(7)], vec![Opnd::ExecLo], 4),
        op("v_cmp_eq_u32", Enc::Vop3, vec![Opnd::VccLo], vec![Opnd::Vgpr(2), Opnd::Sgpr(7)], 8),
    ];
    assert!(verify_lane_election(&mut ops, 1, true).is_ok());
    ops[2].src.swap(0, 1);
    assert!(verify_lane_election(&mut ops, 1, true).is_ok());
    ops[2].mnemonic = "v_cmp_ne_u32".into();
    assert!(verify_lane_election(&mut ops, 1, true).is_err());
    ops[2].mnemonic = "v_cmp_eq_u32".into();
    ops[0].src[0] = Opnd::ConstInt(0);
    assert!(verify_lane_election(&mut ops, 1, true).is_err());
}

#[test]
fn scalar_load_beside_the_elected_lane_is_not_a_use() {
    // No offset register: SOFFSET names the null register.
    let load = |name: &str, dst: u8, base: u8, offset: u32| Insn {
        raw: vec![0, offset << 25], ..op(name, Enc::Smem, vec![Opnd::Sgpr(dst)], vec![Opnd::Sgpr(base)], 8)
    };
    let program = |load: Insn| vec![
        op("v_mbcnt_lo_u32_b32", Enc::Vop3, vec![Opnd::Vgpr(2)], vec![Opnd::ConstInt(-1), Opnd::ConstInt(0)], 0),
        op("s_ff1_i32_b32", Enc::Sop1, vec![Opnd::Sgpr(7)], vec![Opnd::ExecLo], 4),
        load,
        op("v_cmpx_eq_i32", Enc::Vopc, vec![Opnd::VccLo], vec![Opnd::Sgpr(7), Opnd::Vgpr(2)], 12),
    ];
    // The descriptor s[8:11] and the result s75 leave the elected lane in s7 alone.
    assert!(verify_lane_election(&mut program(load("s_buffer_load_dword", 75, 8, 0x7d)), 1, false).is_ok());
    assert!(verify_lane_election(&mut program(load("s_load_dwordx4", 8, 5, 0x7d)), 1, false).is_ok());
    // A descriptor s[4:7], a table pointer s[6:7], an offset in s7 and a result
    // word in s7 all touch it.
    assert!(verify_lane_election(&mut program(load("s_buffer_load_dword", 75, 4, 0x7d)), 1, false).is_err());
    assert!(verify_lane_election(&mut program(load("s_load_dwordx4", 8, 6, 0x7d)), 1, false).is_err());
    assert!(verify_lane_election(&mut program(load("s_buffer_load_dword", 75, 8, 7)), 1, false).is_err());
    assert!(verify_lane_election(&mut program(load("s_buffer_load_dword", 7, 8, 0x7d)), 1, false).is_err());
    assert!(verify_lane_election(&mut program(load("s_load_dwordx4", 4, 12, 0x7d)), 1, false).is_err());
}

#[test]
fn lane_spill_padding_is_not_a_use_of_the_elected_lane() {
    // `v_writelane_b32 v86, s2, 0` decodes its unused third source field as s0.
    let program = |value: Opnd, lane: Opnd| vec![
        op("v_mbcnt_lo_u32_b32", Enc::Vop3, vec![Opnd::Vgpr(7)], vec![Opnd::ConstInt(-1), Opnd::ConstInt(0)], 0),
        op("s_ff1_i32_b32", Enc::Sop1, vec![Opnd::Sgpr(0)], vec![Opnd::ExecLo], 4),
        op("v_writelane_b32", Enc::Vop3, vec![Opnd::Vgpr(86)], vec![value, lane, Opnd::Sgpr(0)], 8),
        op("v_cmpx_eq_i32", Enc::Vopc, vec![Opnd::VccLo], vec![Opnd::Sgpr(0), Opnd::Vgpr(7)], 16),
    ];
    assert!(verify_lane_election(&mut program(Opnd::Sgpr(2), Opnd::ConstInt(0)), 1, false).is_ok());
    // Spilling the elected lane, or writing the lane it names, does use it.
    assert!(verify_lane_election(&mut program(Opnd::Sgpr(0), Opnd::ConstInt(0)), 1, false).is_err());
    assert!(verify_lane_election(&mut program(Opnd::Sgpr(2), Opnd::Sgpr(0)), 1, false).is_err());
}
