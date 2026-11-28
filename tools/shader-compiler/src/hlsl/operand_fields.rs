//! What an instruction reads, and what a scalar register is known to hold.
//!
//! Dataflows that follow descriptor words from their load to their use share
//! these: which source fields of an instruction it reads, whether it touches
//! memory, where the straight run before an instruction starts, and the
//! literal a register holds at an instruction.
use super::*;
use crate::rdna2::clobbers_scalar;

fn branches(ins: &Insn) -> bool {
    ins.enc == Enc::Sopp && (Emitter::branch_cond(ins.op).is_some() || ins.op == 1)
}

pub(super) fn memory(ins: &Insn) -> bool {
    matches!(ins.enc, Enc::Smem | Enc::Flat | Enc::Mubuf | Enc::Mtbuf | Enc::Mimg | Enc::Ds | Enc::Exp)
}

/// First instruction of the straight run that ends before `at`.
pub(super) fn run_start(insns: &[Insn], at: usize) -> usize {
    let targets: BTreeSet<usize> = insns.iter().filter(|ins| branches(ins) && ins.op != 1)
        .map(|ins| (ins.offset as i64 + 4 + ins.aux.imm * 4) as usize).collect();
    (0..at).rev().find(|&i| branches(&insns[i]) || targets.contains(&insns[i + 1].offset)).map_or(0, |i| i + 1)
}

/// The three-operand encoding has three source fields whatever the
/// instruction reads, and a field it does not read is zero, which decodes as
/// s0. Returns whether source `at` is such a field.
pub(super) fn unused_field(ins: &Insn, at: usize, register: u8) -> bool {
    if ins.enc != Enc::Vop3 || register != 0 {
        return false;
    }
    let read = match ins.op {
        // The one-operand instructions in this encoding.
        0x180..=0x1bf => 1,
        // Compares and the two-operand instructions. A select reads its mask
        // and an add or subtract with carry its carry as the third.
        0..=0x13f if ins.mnemonic == "v_cndmask_b32" || ins.mnemonic.contains("_co_ci_") => 3,
        0..=0x13f => 2,
        _ if two_operands(&ins.mnemonic) => 2,
        // Anything else is taken to read all three.
        _ => 3,
    };
    at >= read
}

/// Instructions that exist only in the three-operand encoding and read two.
fn two_operands(mnemonic: &str) -> bool {
    matches!(mnemonic,
        "v_mul_hi_u32" | "v_mul_lo_u32" | "v_mul_hi_i32" | "v_ldexp_f32" | "v_pack_b32_f16"
        | "v_add_f64" | "v_mul_f64" | "v_min_f64" | "v_max_f64" | "v_ldexp_f64"
        | "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" | "v_add_co_u32" | "v_sub_co_u32" | "v_subrev_co_u32"
        | "v_readlane_b32" | "v_writelane_b32"
        // Both coefficients of this interpolation come from the attribute named in the first field.
        | "v_interp_p1ll_f16")
        // Packed 16-bit arithmetic, except the multiply-adds.
        || (mnemonic.starts_with("v_pk_") && !mnemonic.contains("fma") && !mnemonic.contains("mad"))
}

/// The constant `register` holds at instruction `at`: its last write before
/// `at` in program order is a move of a literal, and every jump to an
/// instruction after the move, up to `at`, starts after the move as well. So
/// each way to `at` runs the move and nothing after it writes the register.
pub(super) fn constant_before(insns: &[Insn], at: usize, register: u8) -> Option<u32> {
    if insns.iter().any(|ins| ins.mnemonic.contains("setpc") || ins.mnemonic.contains("swappc")) {
        return None;
    }
    // A compare is counted as writing two registers unless the program is
    // known to have 32 lanes, where its mask is one register.
    let narrow = narrow_masks(insns);
    let writes = |ins: &Insn| if narrow && ins.mnemonic.starts_with("v_cmp") {
        ins.dst.contains(&Opnd::Sgpr(register))
    } else {
        clobbers_scalar(ins, register)
    };
    let writer = (0..at).rev().find(|&index| writes(&insns[index]))?;
    let ins = &insns[writer];
    let value = match (ins.mnemonic.as_str(), &ins.dst[..], ins.src.first()) {
        ("s_mov_b32", &[Opnd::Sgpr(n)], Some(Opnd::ConstInt(value))) if n == register => *value as u32,
        ("s_mov_b32", &[Opnd::Sgpr(n)], Some(Opnd::Literal(value))) if n == register => *value,
        _ => return None,
    };
    let (first, last) = (ins.offset as i64, insns[at].offset as i64);
    let enters = insns.iter().filter(|jump| branches(jump)).any(|jump| {
        let (from, to) = (jump.offset as i64, jump.offset as i64 + 4 + jump.aux.imm * 4);
        first < to && to <= last && !(first < from && from < last)
    });
    (!enters).then_some(value)
}

/// Whether the program is compiled for 64 lanes: it runs a 64-bit scalar
/// instruction on the execution or carry mask. A 32-lane program has only
/// the low halves and uses the 32-bit forms.
fn wide_masks(insns: &[Insn]) -> bool {
    insns.iter().any(|ins| ins.mnemonic.starts_with("s_") && ins.mnemonic.ends_with("_b64")
        // The save-and-combine forms take the execution mask without naming it.
        && (ins.mnemonic.contains("saveexec") || ins.src.iter().chain(&ins.dst)
            .any(|operand| matches!(operand, Opnd::ExecLo | Opnd::ExecHi | Opnd::VccLo | Opnd::VccHi))))
}

/// Whether every lane mask of the program is one register: some 32-bit scalar
/// instruction takes the execution or carry mask, and nothing handles a mask as
/// 64 bits or names its upper half. A compare then writes one register, and the
/// one after it is not part of its result.
pub(super) fn one_word_masks(insns: &[Insn]) -> bool {
    let mask = |operand: &Opnd| matches!(operand, Opnd::ExecLo | Opnd::VccLo);
    let upper = |operand: &Opnd| matches!(operand, Opnd::ExecHi | Opnd::VccHi);
    let narrow = insns.iter().any(|ins| ins.mnemonic.starts_with("s_") && ins.mnemonic.ends_with("_b32")
        && (ins.mnemonic.contains("saveexec") || ins.mnemonic.contains("wrexec") || ins.src.iter().chain(&ins.dst).any(mask)));
    narrow && !wide_masks(insns) && !insns.iter().any(|ins| ins.src.iter().chain(&ins.dst).any(upper))
}

/// Whether the program is compiled for 32 lanes: it saves and combines the
/// execution mask as 32 bits, and never handles a mask as 64.
fn narrow_masks(insns: &[Insn]) -> bool {
    insns.iter().any(|ins| ins.mnemonic.contains("saveexec_b32")) && !wide_masks(insns)
}

#[cfg(test)]
#[path = "../tests/hlsl/operand_fields.rs"]
mod tests;
