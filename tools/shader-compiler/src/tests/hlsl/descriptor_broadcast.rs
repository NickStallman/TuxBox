use super::*;
use crate::rdna2::InsnAux;

fn op(name: &str, enc: Enc, dst: Vec<Opnd>, src: Vec<Opnd>) -> Insn {
    Insn { offset: 0, dwords: 1, enc, op: 0, raw: vec![0], mnemonic: name.into(), dst, src, aux: InsnAux::default() }
}

fn branch(code: u32, to: usize) -> (Insn, usize) {
    (Insn { op: code, ..op("s_cbranch", Enc::Sopp, vec![], vec![]) }, to)
}

/// Lay the instructions out four bytes apart and resolve each branch to its target index.
fn program(items: Vec<(Insn, usize)>) -> Vec<Insn> {
    let mut insns: Vec<Insn> = items.iter().map(|(i, _)| i.clone()).collect();
    for (at, (ins, (_, to))) in insns.iter_mut().zip(&items).enumerate() {
        ins.offset = 4 * at;
        if ins.enc == Enc::Sopp && ins.op != 1 { ins.aux.imm = *to as i64 - at as i64 - 1; }
    }
    insns
}

fn plain(i: Insn) -> (Insn, usize) { (i, 0) }
fn copy(v: u8, s: u8) -> (Insn, usize) { plain(op("v_mov_b32", Enc::Vop1, vec![Opnd::Vgpr(v)], vec![Opnd::Sgpr(s)])) }
fn read(s: u8, v: u8) -> (Insn, usize) { plain(op("v_readfirstlane_b32", Enc::Vop1, vec![Opnd::Sgpr(s)], vec![Opnd::Vgpr(v)])) }
fn save(s: u8) -> (Insn, usize) { plain(op("s_and_saveexec_b64", Enc::Sop1, vec![Opnd::Sgpr(s)], vec![Opnd::Sgpr(s)])) }
fn hand_back(s: u8) -> (Insn, usize) { plain(op("s_xor_b64", Enc::Sop2, vec![Opnd::ExecLo], vec![Opnd::ExecLo, Opnd::Sgpr(s)])) }
fn end() -> (Insn, usize) { plain(Insn { op: 1, ..op("s_endpgm", Enc::Sopp, vec![], vec![]) }) }

/// copy; loop { read; save; <body>; hand back; execnz loop }; end
fn waterfall(body: (Insn, usize)) -> Vec<Insn> {
    program(vec![copy(57, 8), read(12, 57), save(0), body, hand_back(0), branch(9, 1), end()])
}

#[test]
fn a_waterfall_reads_back_the_copied_scalar() {
    let sample = plain(op("image_sample_l", Enc::Mimg, vec![Opnd::Vgpr(66)], vec![Opnd::Vgpr(54), Opnd::Sgpr(12), Opnd::Sgpr(44)]));
    let (copies, reads) = find(&waterfall(sample));
    assert_eq!((copies, reads), (BTreeSet::from([0]), BTreeSet::from([1])));
}

#[test]
fn a_write_or_a_wider_exec_between_copy_and_read_is_not_proven() {
    // Another value in the register, for the lanes active at that write.
    let other = plain(op("v_mov_b32", Enc::Vop1, vec![Opnd::Vgpr(57)], vec![Opnd::Vgpr(3)]));
    // The second result word of a two-word load lands in v57.
    let wide = plain(Insn { aux: InsnAux { mask: 3, ..InsnAux::default() },
        ..op("image_load", Enc::Mimg, vec![Opnd::Vgpr(56)], vec![Opnd::Vgpr(54), Opnd::Sgpr(12), Opnd::Sgpr(44)]) });
    // Lanes that were not active at the copy join.
    let restore = plain(op("s_mov_b64", Enc::Sop1, vec![Opnd::ExecLo], vec![Opnd::Sgpr(20)]));
    let widen = plain(op("s_or_saveexec_b64", Enc::Sop1, vec![Opnd::Sgpr(20)], vec![Opnd::ConstInt(-1)]));
    // The mask handed back is no longer the one the loop saved.
    let clobber = plain(op("s_mov_b64", Enc::Sop1, vec![Opnd::Sgpr(0)], vec![Opnd::Sgpr(20)]));
    for body in [other, wide, restore, widen, clobber] {
        assert_eq!(find(&waterfall(body.clone())).1, BTreeSet::new(), "{}", body.0.mnemonic);
    }
}

#[test]
fn every_path_to_the_read_must_pass_a_copy() {
    // A branch around the copy reaches the read with whatever the register held.
    let skipped = program(vec![branch(5, 2), copy(57, 8), read(12, 57), end()]);
    assert_eq!(find(&skipped).1, BTreeSet::new());
    // Either of two copies is the last write.
    let either = program(vec![branch(5, 3), copy(57, 8), branch(2, 4), copy(57, 16), read(12, 57), end()]);
    assert_eq!(find(&either), (BTreeSet::from([1, 3]), BTreeSet::from([4])));
    // A branch into the loop body skips the save, so the mask handed back is unknown.
    let entered = program(vec![copy(57, 8), branch(5, 4), read(12, 57), save(0), hand_back(0), branch(9, 2), end()]);
    assert_eq!(find(&entered).1, BTreeSet::new());
    // An indirect jump can come from anywhere.
    let mut jump = waterfall(plain(op("s_nop", Enc::Sopp, vec![], vec![])));
    jump[3].mnemonic = "s_setpc_b64".into();
    assert_eq!(find(&jump).1, BTreeSet::new());
}

#[test]
fn a_restored_exec_counts_from_where_it_was_saved() {
    let keep = |s: u8| plain(op("s_mov_b64", Enc::Sop1, vec![Opnd::Sgpr(s)], vec![Opnd::ExecLo]));
    let restore = |s: u8| plain(op("s_mov_b64", Enc::Sop1, vec![Opnd::ExecLo], vec![Opnd::Sgpr(s)]));
    let widen = plain(op("s_or_saveexec_b64", Enc::Sop1, vec![Opnd::Sgpr(20)], vec![Opnd::ConstInt(-1)]));
    // copy; keep; <any EXEC>; restore; read
    let after = |between: (Insn, usize)| program(vec![copy(57, 8), keep(10), between, restore(10), read(12, 57), end()]);
    assert_eq!(find(&after(widen.clone())), (BTreeSet::from([0]), BTreeSet::from([4])));
    // The kept mask was overwritten, by a mask in s[9:10] or a four-word load into s[8:11].
    let mask = plain(op("v_cmp_eq_u64", Enc::Vop3, vec![Opnd::Sgpr(9)], vec![Opnd::Sgpr(12), Opnd::Vgpr(3)]));
    let load = plain(op("s_load_dwordx4", Enc::Smem, vec![Opnd::Sgpr(8)], vec![Opnd::Sgpr(4)]));
    for between in [mask, load] {
        assert_eq!(find(&after(between)).1, BTreeSet::new());
    }
    // A mask beside it, in s[8:9], leaves it alone.
    let beside = plain(op("v_cmp_eq_u64", Enc::Vop3, vec![Opnd::Sgpr(8)], vec![Opnd::Sgpr(12), Opnd::Vgpr(3)]));
    assert_eq!(find(&after(beside)).1, BTreeSet::from([4]));
    // EXEC kept before the copy may hold lanes the copy never wrote.
    let early = program(vec![keep(10), widen.clone(), copy(57, 8), restore(10), read(12, 57), end()]);
    assert_eq!(find(&early).1, BTreeSet::new());
    // A path that restores without having kept EXEC.
    let unkept = program(vec![copy(57, 8), branch(5, 3), keep(10), restore(10), read(12, 57), end()]);
    assert_eq!(find(&unkept).1, BTreeSet::new());
}

#[test]
fn a_32_bit_scalar_result_beside_a_kept_mask_leaves_it_alone() {
    let keep = |s: u8| plain(op("s_mov_b64", Enc::Sop1, vec![Opnd::Sgpr(s)], vec![Opnd::ExecLo]));
    let restore = |s: u8| plain(op("s_mov_b64", Enc::Sop1, vec![Opnd::ExecLo], vec![Opnd::Sgpr(s)]));
    let scalar = |name: &str, s: u8| plain(op(name, Enc::Sop2, vec![Opnd::Sgpr(s)], vec![Opnd::Sgpr(s), Opnd::ConstInt(0)]));
    let after = |between: (Insn, usize)| program(vec![copy(57, 8), keep(10), between, restore(10), read(12, 57), end()]);
    // The carry half of a 64-bit add writes s9 only, as does a lane read into s9.
    assert_eq!(find(&after(scalar("s_addc_u32", 9))).1, BTreeSet::from([4]));
    let lane = plain(op("v_readfirstlane_b32", Enc::Vop1, vec![Opnd::Sgpr(9)], vec![Opnd::Vgpr(3)]));
    assert_eq!(find(&after(lane)).1, BTreeSet::from([4]));
    // Writes into the kept pair still lose it, a 64-bit result from the word below too.
    for (name, s) in [("s_addc_u32", 10), ("s_add_u32", 11), ("s_lshl_b64", 9)] {
        assert_eq!(find(&after(scalar(name, s))).1, BTreeSet::new(), "{name} s{s}");
    }
}
