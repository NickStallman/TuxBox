use super::*;
use crate::{hlsl::translate, ShaderExtract, Stage};

mod fixture {
    include!("../../../examples/support/wave_guard_fixture.rs");
}

/// The translation of a wave32 program given as native words.
fn translated(words: &[u32]) -> Result<String, String> {
    let (mut shader, _, _): (ShaderExtract, _, _) = fixture::shader(32);
    assert!(matches!(shader.stage, Stage::Compute));
    shader.isa = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    shader.isa_data = shader.isa.clone();
    translate(&shader).map(|shader| shader.hlsl)
}

/// The OR-saveexec, DPP and lane-read sum of `v0` into s7, restoring EXEC from s0.
const REDUCTION: [u32; 17] = [0xbe803dc1, 0xd5010002, 0x00020080, 0x4a0404e9, 0xde54c102, 0x4a0404e9, 0x93601202,
    0x4a0404e9, 0x00092402, 0x4a0404fa, 0xff094002, 0xd7780003, 0x02010102, 0x4a040503, 0xd7600007, 0x00010102,
    0xbefe0300];

/// `body` between a save and a restore of EXEC, then the reduction behind a
/// branch on VCC, which only a program that branches with the whole wave keeps.
fn behind_a_mask_branch(body: &[u32]) -> Vec<u32> {
    // s_mov_b32 s12, exec_lo ... s_mov_b32 exec_lo, s12; v_cmp_eq_u32 vcc_lo, v1, v1; s_cbranch_vccz end
    [&[0xbe8c037e], body, &[0xbefe030c, 0x7d840301, 0xbf860011], &REDUCTION[..], &[0xbf810000]].concat()
}

fn insn(mnemonic: &str, enc: Enc, op: u32, dst: Vec<Opnd>, src: Vec<Opnd>) -> Insn {
    Insn { offset: 0, dwords: 1, enc, op, raw: vec![0, 125 << 25], mnemonic: mnemonic.into(), dst, src,
        aux: Default::default() }
}

fn program(mut ops: Vec<Insn>) -> Vec<Insn> {
    ops.push(insn("s_endpgm", Enc::Sopp, 1, vec![], vec![]));
    for (i, op) in ops.iter_mut().enumerate() {
        op.offset = i * 4;
    }
    ops
}

fn jump(op: u32, over: i64) -> Insn {
    let mut branch = insn("s_cbranch", Enc::Sopp, op, vec![], vec![]);
    branch.aux.imm = over;
    branch
}

fn compare() -> Insn {
    insn("v_cmp_gt_u32", Enc::Vopc, 0, vec![Opnd::VccLo], vec![Opnd::Vgpr(0), Opnd::Vgpr(1)])
}

fn scalar(mnemonic: &str, dst: u8, src: Vec<Opnd>) -> Insn {
    let enc = if src.len() == 1 { Enc::Sop1 } else { Enc::Sop2 };
    insn(mnemonic, enc, 0, vec![Opnd::Sgpr(dst)], src)
}

#[test]
fn mask_operations_vote_on_their_condition_code() {
    let ops = program(vec![
        compare(),
        scalar("s_and_b32", 4, vec![Opnd::VccLo, Opnd::ExecLo]),
        jump(4, 1),
        scalar("s_and_saveexec_b32", 5, vec![Opnd::VccLo]),
        insn("s_cmp_eq_u32", Enc::Sopc, 0, vec![Opnd::Scc], vec![Opnd::Sgpr(4), Opnd::ConstInt(0)]),
    ]);
    let votes = prove(&ops, 32).unwrap().votes;
    assert_eq!(votes[&1], Vote { mask: Opnd::Sgpr(4), none: false, count: false });
    assert_eq!(votes[&3], Vote { mask: Opnd::ExecLo, none: false, count: false });
    assert_eq!(votes[&4], Vote { mask: Opnd::Sgpr(4), none: true, count: false });
}

#[test]
fn a_counted_mask_is_a_number_for_the_whole_wave() {
    let ops = program(vec![
        compare(),
        scalar("s_bcnt1_i32_b32", 4, vec![Opnd::VccLo]),
        scalar("s_add_u32", 5, vec![Opnd::Sgpr(4), Opnd::ConstInt(1)]),
    ]);
    let votes = prove(&ops, 32).unwrap().votes;
    assert_eq!(votes[&1], Vote { mask: Opnd::VccLo, none: false, count: true });
    assert_eq!(votes.len(), 1);
}

#[test]
fn wrexec_leaves_a_mask_and_votes() {
    let ops = program(vec![
        scalar("s_mov_b32", 4, vec![Opnd::ExecLo]),
        scalar("s_andn2_wrexec_b32", 4, vec![Opnd::Sgpr(4)]),
        scalar("s_mov_b32", 5, vec![Opnd::Sgpr(4)]),
    ]);
    assert_eq!(prove(&ops, 32).unwrap().votes[&1], Vote { mask: Opnd::ExecLo, none: false, count: false });
    let number = program(vec![scalar("s_andn2_wrexec_b32", 4, vec![Opnd::Sgpr(8)])]);
    assert_eq!(prove(&number, 32), None);
}

#[test]
fn lane_reads_name_their_scalar_destination() {
    let read = insn("v_readlane_b32", Enc::Vop3, 0, vec![Opnd::Vgpr(2)],
        vec![Opnd::Vgpr(1), Opnd::ConstInt(0), Opnd::Sgpr(0)]);
    let save = scalar("s_and_saveexec_b32", 8, vec![Opnd::VccLo]);
    let mut ops = program(vec![save.clone(), read.clone()]);
    lower_lane_reads([32, 1, 1], &mut ops).unwrap();
    assert_eq!(ops[1].dst, [Opnd::Sgpr(2)]);
    assert_eq!(ops[1].src, [Opnd::Vgpr(1), Opnd::ConstInt(0)]);
    // Partial waves, 64-lane masks and shared swizzles are not covered.
    assert!(lower_lane_reads([48, 1, 1], &mut program(vec![save.clone(), read.clone()])).is_err());
    let wide = scalar("s_and_saveexec_b64", 8, vec![Opnd::VccLo]);
    assert!(lower_lane_reads([64, 1, 1], &mut program(vec![wide, read.clone()])).is_err());
    let swizzle = insn("ds_swizzle_b32", Enc::Ds, 0, vec![Opnd::Vgpr(3)], vec![Opnd::Vgpr(1)]);
    assert!(lower_lane_reads([32, 1, 1], &mut program(vec![save, swizzle, read])).is_err());
}

#[test]
fn ordinary_scalar_work_needs_no_vote() {
    let ops = program(vec![
        scalar("s_add_u32", 4, vec![Opnd::Sgpr(0), Opnd::ConstInt(1)]),
        scalar("s_and_b32", 5, vec![Opnd::Sgpr(4), Opnd::ConstInt(-1)]),
        jump(5, 0),
    ]);
    assert_eq!(prove(&ops, 32).unwrap(), Proof::default());
}

#[test]
fn masks_used_as_numbers_are_refused() {
    let mask = || scalar("s_mov_b32", 4, vec![Opnd::ExecLo]);
    for misuse in [
        scalar("s_add_u32", 5, vec![Opnd::Sgpr(4), Opnd::ConstInt(1)]),
        // A 64-bit count of a 32-bit mask.
        scalar("s_bcnt1_i32_b64", 6, vec![Opnd::Sgpr(4)]),
        scalar("s_lshl_b32", 5, vec![Opnd::Sgpr(4), Opnd::ConstInt(1)]),
        scalar("s_and_b32", 5, vec![Opnd::Sgpr(4), Opnd::Sgpr(0)]),
        insn("v_mov_b32", Enc::Vop1, 0, vec![Opnd::Vgpr(2)], vec![Opnd::Sgpr(4)]),
        insn("s_cmp_gt_u32", Enc::Sopc, 0, vec![Opnd::Scc], vec![Opnd::Sgpr(4), Opnd::ConstInt(0)]),
    ] {
        let name = misuse.mnemonic.clone();
        assert_eq!(prove(&program(vec![mask(), misuse]), 32), None, "{name}");
    }
}

#[test]
fn numbers_used_as_masks_are_refused() {
    // A literal written to EXEC selects lanes by position.
    let write = scalar("s_mov_b32", 126, vec![Opnd::Literal(0xffff)]);
    let mut write = write;
    write.dst = vec![Opnd::ExecLo];
    assert_eq!(prove(&program(vec![write, jump(8, 0)]), 32), None);
    // A register that is a mask on only one path cannot be read after them.
    let after = |read: Vec<Insn>| {
        prove(&program([vec![jump(5, 1), scalar("s_mov_b32", 4, vec![Opnd::ExecLo])], read].concat()), 32)
    };
    assert_eq!(after(vec![scalar("s_and_b32", 5, vec![Opnd::Sgpr(4), Opnd::ExecLo])]), None);
    assert_eq!(after(vec![scalar("s_add_u32", 5, vec![Opnd::Sgpr(4), Opnd::ConstInt(1)])]), None);
    // A copy is no more than its source, and is refused where it is read in turn.
    let copy = || scalar("s_mov_b32", 5, vec![Opnd::Sgpr(4)]);
    assert!(after(vec![copy()]).is_some());
    assert_eq!(after(vec![copy(), scalar("s_and_b32", 6, vec![Opnd::Sgpr(5), Opnd::ExecLo])]), None);
}

#[test]
fn masks_survive_loops_and_saved_exec() {
    let mut restore = scalar("s_mov_b32", 0, vec![Opnd::Sgpr(10)]);
    restore.dst = vec![Opnd::ExecLo];
    let ops = program(vec![
        scalar("s_and_saveexec_b32", 10, vec![Opnd::ExecLo]),
        compare(),
        scalar("s_and_saveexec_b32", 11, vec![Opnd::VccLo]),
        jump(9, -3),
        restore,
    ]);
    assert_eq!(prove(&ops, 32).unwrap().votes.len(), 2);
}

#[test]
fn unused_source_fields_are_not_reads() {
    // The long encoding's third field of a two-operand instruction names s0.
    let spare = |mnemonic: &str, enc, dst: Vec<Opnd>| {
        insn(mnemonic, enc, 0, dst, vec![Opnd::Vgpr(1), Opnd::Vgpr(2), Opnd::Sgpr(0)])
    };
    let mask = || scalar("s_mov_b32", 0, vec![Opnd::ExecLo]);
    for op in [
        spare("v_cmp_eq_u32", Enc::Vop3, vec![Opnd::Sgpr(4)]),
        spare("v_fmac_f32", Enc::Vop3, vec![Opnd::Vgpr(3)]),
        spare("image_load", Enc::Mimg, vec![Opnd::Vgpr(3)]),
    ] {
        let name = op.mnemonic.clone();
        assert!(prove(&program(vec![mask(), op]), 32).is_some(), "{name}");
    }
    for op in [
        spare("v_fma_f32", Enc::Vop3, vec![Opnd::Vgpr(3)]),
        spare("image_sample", Enc::Mimg, vec![Opnd::Vgpr(3)]),
    ] {
        let name = op.mnemonic.clone();
        assert_eq!(prove(&program(vec![mask(), op]), 32), None, "{name}");
    }
}

#[test]
fn branch_conditions_cover_both_masks_and_polarities() {
    assert_eq!(branch_condition(8).unwrap(), "!WaveActiveAnyTrue((exec_lo & 1u) != 0u)");
    assert_eq!(branch_condition(7).unwrap(), "WaveActiveAnyTrue((vcc_lo & 1u) != 0u)");
    assert_eq!(branch_condition(4), None);
}

#[test]
fn counts_of_a_mask_read_the_whole_wave() {
    let mask = || scalar("s_mov_b32", 4, vec![Opnd::ExecLo]);
    let below = |name: &str, source| insn(name, Enc::Vop3, 0, vec![Opnd::Vgpr(2)], vec![source, Opnd::ConstInt(0), Opnd::Sgpr(0)]);
    for (count, width) in [
        (scalar("s_ff1_i32_b32", 5, vec![Opnd::Sgpr(4)]), 32),
        // A count into its own mask reads the mask before it replaces it.
        (scalar("s_bcnt1_i32_b32", 4, vec![Opnd::Sgpr(4)]), 32),
        (scalar("s_bcnt1_i32_b64", 5, vec![Opnd::Sgpr(4)]), 64),
        (scalar("s_ff1_i32_b64", 5, vec![Opnd::Sgpr(4)]), 64),
        // EXEC names its own half in a wave of either width.
        (scalar("s_bcnt1_i32_b32", 5, vec![Opnd::ExecLo]), 64),
        (scalar("s_bcnt1_i32_b64", 5, vec![Opnd::ExecLo]), 32),
        (below("v_mbcnt_lo_u32_b32", Opnd::Sgpr(4)), 32),
    ] {
        let name = count.mnemonic.clone();
        let wide = width == 64 && name.ends_with("_b64");
        let save = scalar(if wide { "s_mov_b64" } else { "s_mov_b32" }, 4, vec![Opnd::ExecLo]);
        // The count is a number every lane agrees on, which a mask is not.
        let ops = program(vec![save, count, scalar("s_add_u32", 6, vec![Opnd::Sgpr(5), Opnd::ConstInt(1)])]);
        let proof = prove(&ops, width).unwrap_or_else(|| panic!("{name} in {width} lanes"));
        assert_eq!(proof.ballots, BTreeSet::from([1]), "{name}");
        assert!(proof.votes.is_empty(), "{name}");
    }
    // The emitter takes the ballot of EXEC below a lane itself.
    for name in ["v_mbcnt_lo_u32_b32", "v_mbcnt_hi_u32_b32"] {
        let proof = prove(&program(vec![below(name, Opnd::ExecLo)]), 64).unwrap();
        assert!(proof.ballots.is_empty(), "{name}");
    }
    // A register holds one half of a wide mask, and which half is not known.
    for (count, width) in [
        (scalar("s_bcnt1_i32_b32", 5, vec![Opnd::Sgpr(4)]), 64),
        (scalar("s_ff1_i32_b64", 5, vec![Opnd::Sgpr(4)]), 32),
        (below("v_mbcnt_lo_u32_b32", Opnd::Sgpr(4)), 64),
        (below("v_mbcnt_hi_u32_b32", Opnd::Sgpr(4)), 32),
    ] {
        let name = count.mnemonic.clone();
        assert_eq!(prove(&program(vec![mask(), count]), width), None, "{name} in {width} lanes");
    }
    // A number is counted as it is.
    let ops = program(vec![scalar("s_bcnt1_i32_b32", 5, vec![Opnd::Sgpr(0)])]);
    assert_eq!(prove(&ops, 32).unwrap(), Proof::default());
}

#[test]
fn retiring_lanes_through_exec_votes_like_saving_it() {
    let retire = |source| scalar("s_andn2_wrexec_b32", 5, vec![source]);
    let ops = program(vec![
        scalar("s_mov_b32", 4, vec![Opnd::ExecLo]),
        compare(),
        scalar("s_and_saveexec_b32", 6, vec![Opnd::VccLo]),
        retire(Opnd::Sgpr(4)),
        jump(4, 0),
        // The destination is the new EXEC, a mask.
        scalar("s_and_b32", 7, vec![Opnd::Sgpr(5), Opnd::ExecLo]),
    ]);
    let proof = prove(&ops, 32).unwrap();
    assert_eq!(proof.votes[&3], Vote { mask: Opnd::ExecLo, none: false, count: false });
    assert_eq!(proof.votes[&5], Vote { mask: Opnd::Sgpr(7), none: false, count: false });
    // A number selects lanes by position.
    let ops = program(vec![scalar("s_add_u32", 4, vec![Opnd::Sgpr(0), Opnd::ConstInt(1)]), retire(Opnd::Sgpr(4))]);
    assert_eq!(prove(&ops, 32), None);
}

#[test]
fn a_stale_restore_refuses_only_what_runs_under_it() {
    let restore = |from| {
        let mut op = scalar("s_mov_b32", 0, vec![Opnd::Sgpr(from)]);
        op.dst = vec![Opnd::ExecLo];
        op
    };
    // s4 is a mask on one path and a number on the other.
    let stale = |after: Vec<Insn>| {
        let mut ops = vec![
            scalar("s_mov_b32", 10, vec![Opnd::ExecLo]),
            scalar("s_mov_b32", 4, vec![Opnd::ExecLo]),
            jump(5, 1),
            scalar("s_add_u32", 4, vec![Opnd::Sgpr(0), Opnd::ConstInt(1)]),
            restore(4),
        ];
        ops.extend(after);
        ops.push(restore(10));
        ops.push(insn("v_mov_b32", Enc::Vop1, 0, vec![Opnd::Vgpr(2)], vec![Opnd::Vgpr(1)]));
        prove(&program(ops), 32)
    };
    assert!(stale(vec![]).is_some());
    assert!(stale(vec![scalar("s_add_u32", 5, vec![Opnd::Sgpr(0), Opnd::ConstInt(1)])]).is_some());
    for under in [
        insn("v_mov_b32", Enc::Vop1, 0, vec![Opnd::Vgpr(2)], vec![Opnd::Vgpr(1)]),
        insn("buffer_store_dword", Enc::Mubuf, 0, vec![Opnd::Vgpr(2)], vec![Opnd::Vgpr(1), Opnd::Sgpr(20), Opnd::ConstInt(0)]),
        jump(8, 0),
        scalar("s_mov_b32", 5, vec![Opnd::ExecLo]),
        scalar("s_and_saveexec_b32", 5, vec![Opnd::VccLo]),
        scalar("s_bcnt1_i32_b32", 5, vec![Opnd::ExecLo]),
    ] {
        let name = under.mnemonic.clone();
        // The copy made under it is itself refused where it is read.
        let reads = name == "s_mov_b32";
        let after = if reads { vec![under, scalar("s_add_u32", 6, vec![Opnd::Sgpr(5), Opnd::ConstInt(1)])] } else { vec![under] };
        assert_eq!(stale(after), None, "{name}");
    }
}

#[test]
fn a_buffer_offset_is_one_register() {
    let load = |offset| insn("buffer_load_dword", Enc::Mubuf, 0, vec![Opnd::Vgpr(2)],
        vec![Opnd::Vgpr(1), Opnd::Sgpr(20), offset]);
    // s9 is a mask, beside the offset in s8 and after the descriptor in s[20:23].
    let mask = |register| scalar("s_mov_b32", register, vec![Opnd::ExecLo]);
    assert!(prove(&program(vec![mask(9), load(Opnd::Sgpr(8))]), 32).is_some());
    assert!(prove(&program(vec![mask(24), load(Opnd::Sgpr(8))]), 32).is_some());
    assert_eq!(prove(&program(vec![mask(8), load(Opnd::Sgpr(8))]), 32), None);
    assert_eq!(prove(&program(vec![mask(23), load(Opnd::Sgpr(8))]), 32), None);
}

#[test]
fn lanes_read_through_the_group_are_numbers() {
    for name in ["tb_indexed_readlane", crate::hlsl::GROUP_READ] {
        let read = |lane| insn(name, Enc::Sop1, 0, vec![Opnd::Sgpr(5)], vec![Opnd::Vgpr(1), lane]);
        let ops = program(vec![
            scalar("s_mov_b32", 5, vec![Opnd::ExecLo]),
            read(Opnd::Sgpr(0)),
            scalar("s_add_u32", 6, vec![Opnd::Sgpr(5), Opnd::ConstInt(1)]),
        ]);
        assert_eq!(prove(&ops, 32).unwrap(), Proof::default(), "{name}");
        // Each lane would read the one its own bit names.
        let ops = program(vec![scalar("s_mov_b32", 4, vec![Opnd::ExecLo]), read(Opnd::Sgpr(4))]);
        assert_eq!(prove(&ops, 32), None, "{name}");
    }
}

#[test]
fn a_mask_filled_behind_a_branch_is_counted_across_the_wave() {
    // s_mov_b32 s11, 0; v_cmp_lt_u32 vcc_lo, 1000, v0; s_cbranch_vccz +1; s_and_b32 s11, vcc_lo, exec_lo;
    // s_bcnt1_i32_b32 s20, s11; s_ff1_i32_b32 s21, s11; v_mbcnt_lo_u32_b32 v10, s11, 0
    let hlsl = translated(&behind_a_mask_branch(&[0xbe8b0380, 0x7d8200ff, 0x000003e8, 0xbf860001, 0x870b7e6a,
        0xbe940f0b, 0xbe95130b, 0xd765000a, 0x0001000b])).unwrap();
    let ballot = "WaveActiveBallot((s11 & 1u) != 0u).x";
    assert!(hlsl.contains("s20 = WaveActiveCountBits((s11 & 1u) != 0u);"), "{hlsl}");
    assert!(hlsl.contains(&format!("s21 = firstbitlow({ballot});")), "{hlsl}");
    assert!(hlsl.contains(&format!("countbits({ballot} & ")), "{hlsl}");
    assert!(hlsl.contains("!WaveActiveAnyTrue((vcc_lo & 1u) != 0u)"), "{hlsl}");
}

#[test]
fn lanes_retired_through_exec_vote_on_the_rest() {
    // v_cmp_lt_u32 vcc_lo, 1000, v0; s_and_saveexec_b32 s10, vcc_lo; v_mov_b32 v10, 5; s_andn2_wrexec_b32 s11, s10;
    // s_cbranch_scc0 +2; s_mov_b32 s20, 9; v_mov_b32 v10, 6; s_bcnt1_i32_b32 s21, s11; s_mov_b32 exec_lo, s10
    let hlsl = translated(&behind_a_mask_branch(&[0x7d8200ff, 0x000003e8, 0xbe8a3c6a, 0x7e140285, 0xbe8b470a,
        0xbf840002, 0xbe940389, 0x7e140286, 0xbe950f0b, 0xbefe030a])).unwrap();
    let retire = hlsl.find("s_andn2_wrexec_b32").unwrap();
    let branch = hlsl.find("s_cbranch_scc0").unwrap();
    assert!(hlsl[retire..branch].contains("= s10 & ~exec_lo;"), "{hlsl}");
    assert!(hlsl[retire..branch].trim_end().ends_with("scc = uint(WaveActiveAnyTrue((exec_lo & 1u) != 0u));\n    // 0x0018"), "{hlsl}");
    assert!(hlsl.contains("s21 = WaveActiveCountBits((s11 & 1u) != 0u);"), "{hlsl}");
}

#[test]
fn a_restore_nothing_runs_under_does_not_refuse_the_program() {
    // s_mov_b32 s10, exec_lo; v_cmp_lt_u32 vcc_lo, 1000, v0; s_cbranch_vccz +1; s_mov_b32 s10, 5;
    // s_mov_b32 exec_lo, s10; s_mov_b32 exec_lo, s12; s_mov_b32 s20, 4; v_mov_b32 v10, s20
    let body = [0xbe8a037e, 0x7d8200ff, 0x000003e8, 0xbf860001, 0xbe8a0385, 0xbefe030a, 0xbefe030c, 0xbe940384,
        0x7e140214];
    let hlsl = translated(&behind_a_mask_branch(&body)).unwrap();
    assert!(hlsl.contains("!WaveActiveAnyTrue((vcc_lo & 1u) != 0u)"), "{hlsl}");
    // With a vector write under the stale EXEC lockstep can't prove the program, and the
    // whole wave still takes the branch.
    let mut under = body.to_vec();
    under.insert(6, 0x7e140285); // v_mov_b32 v10, 5
    let hlsl = translated(&behind_a_mask_branch(&under)).unwrap();
    assert!(!hlsl.contains("scc = uint(WaveActiveAnyTrue"), "{hlsl}");
    assert!(hlsl.contains("!WaveActiveAnyTrue((vcc_lo & 1u) != 0u)"), "{hlsl}");
}

#[test]
fn the_lane_a_mask_elects_is_read_from_the_wave() {
    // s_and_b32 s11, s12, exec_lo; s_ff1_i32_b32 s11, s11; v_readlane_b32 s21, v0, s11; s_mov_b32 s20, s11
    let hlsl = translated(&behind_a_mask_branch(&[0x870b7e0c, 0xbe8b130b, 0xd7600015, 0x00001700, 0xbe94030b])).unwrap();
    assert!(hlsl.contains("= WaveReadLaneAt(v0, s11 & 31u);"), "{hlsl}");
    assert!(!hlsl.contains("GroupMemoryBarrierWithGroupSync"), "{hlsl}");
    assert!(hlsl.contains("!WaveActiveAnyTrue((vcc_lo & 1u) != 0u)"), "{hlsl}");
}

#[test]
fn a_guarded_reduction_behind_interleaved_loops_branches_with_the_wave() {
    // Two loops that share their bodies, a restore of EXEC and an EXECZ guard before the reduction.
    let loops = [0xbe8c037e, 0x7d8200ff, 0x000003e8, 0x80148114, 0xbf860001, 0x80158115, 0xbe8a3c6a, 0xbf880001,
        0x4a141481, 0xbefe030a, 0x80168116, 0xbf0a8314, 0xbf85fff6, 0xbf0a8616, 0xbf85fff5, 0xbefe030c];
    let words = [&loops[..], &[0xbf880011], &REDUCTION[..], &[0xbf810000]].concat();
    let hlsl = translated(&words).unwrap();
    assert!(hlsl.contains("tb_pc"), "{hlsl}");
    assert!(hlsl.contains("(!WaveActiveAnyTrue((exec_lo & 1u) != 0u)) ? "), "{hlsl}");
    assert!(hlsl.contains("(!WaveActiveAnyTrue((vcc_lo & 1u) != 0u)) ? "), "{hlsl}");
    // Without the loops every branch on a mask is still the whole wave's.
    let straight = [&[0xbe8c037eu32, 0x7d8200ff, 0x000003e8, 0xbf860001, 0x80158115, 0xbf880011][..], &REDUCTION[..],
        &[0xbf810000]].concat();
    let hlsl = translated(&straight).unwrap();
    assert!(hlsl.contains("!WaveActiveAnyTrue((vcc_lo & 1u) != 0u)"), "{hlsl}");
    assert!(!hlsl.contains("(vcc_lo & 1u) == 0u"), "{hlsl}");
    assert!(hlsl.contains("!WaveActiveAnyTrue((exec_lo & 1u) != 0u)"), "{hlsl}");
}

#[test]
fn a_prefix_sum_over_the_wave_keeps_its_lanes_together() {
    let prefix = || insn(crate::hlsl::wave_alloc::PREFIX, Enc::Sop1, 0, vec![Opnd::Vgpr(2)], vec![Opnd::Vgpr(1)]);
    assert_eq!(prove(&program(vec![compare(), jump(7, 1), prefix()]), 64).unwrap(), Proof::default());
    // It is written under EXEC, which a number in EXEC cannot say.
    let mut number = scalar("s_mov_b32", 0, vec![Opnd::Literal(0xffff)]);
    number.dst = vec![Opnd::ExecLo];
    assert_eq!(prove(&program(vec![number, prefix()]), 64), None);
}
