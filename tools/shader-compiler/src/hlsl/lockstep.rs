//! Wave-wide branches for programs whose wave operations sit in control flow
//! the targeted guards cannot verify.
//!
//! The console branches once per wave. The generic emitter branches once per
//! invocation and keeps only the lane's own bit of EXEC and other lane masks.
//! Here every invocation of a wave follows the same path instead: a branch on a
//! mask votes across the wave, and so does the condition code a mask operation
//! sets. That is only faithful while every scalar register holds either a value
//! all lanes agree on or a lane mask, so a program that treats a mask as a
//! number, or a number as a mask, is refused. Counting the lanes of a mask is
//! the exception: the emitter counts them across the wave.
//!
//! A program whose lane reads sit in a loop that lanes would leave at different
//! times is translated a second time this way ([`retry`]). The loop is then one
//! loop for the wave, so a read reaches its lane in every iteration.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use super::{r, ray_resolve, Emitter, Enc, Insn, Opnd, COUNT_LANES};

thread_local!(static LANE_READS: Cell<bool> = const { Cell::new(false) });

/// Whether this translation keeps the wave together for its lane reads.
pub(super) fn for_lane_reads() -> bool { LANE_READS.with(Cell::get) }

pub(super) fn retry<T>(run: impl FnOnce() -> T) -> T {
    LANE_READS.with(|c| c.set(true));
    let result = run();
    LANE_READS.with(|c| c.set(false));
    result
}

/// Make every lane read of a 32-lane program a read by the whole wave. The
/// encoding names the destination SGPR in its vector destination field.
pub(super) fn lower_lane_reads(group: [u32; 3], insns: &mut [Insn]) -> Result<(), String> {
    let threads = group.into_iter().try_fold(1u32, u32::checked_mul).unwrap_or(0);
    let wide = |i: &Insn| i.mnemonic.ends_with("_b64") && (i.mnemonic.contains("saveexec") || i.mnemonic.contains("wrexec"));
    let narrow = |i: &Insn| i.mnemonic.ends_with("_b32") && (i.mnemonic.contains("saveexec") || i.mnemonic.contains("wrexec"));
    if threads == 0 || threads % 32 != 0 || insns.iter().any(wide) || !insns.iter().any(narrow) {
        return Err("whole-wave lane reads need a program of complete 32-lane waves".into());
    }
    if insns.iter().any(|i| i.mnemonic == "ds_swizzle_b32") {
        return Err("whole-wave lane reads do not cover compute swizzles".into());
    }
    for op in insns.iter_mut().filter(|i| i.mnemonic == "v_readlane_b32") {
        let dst = match op.dst.first() {
            Some(&Opnd::Vgpr(n)) if n <= 105 => n as u8,
            Some(&Opnd::Sgpr(n)) if n <= 105 => n,
            _ => return Err(format!("v_readlane_b32 at {:#x}: unsupported destination", op.offset)),
        };
        if !matches!(op.src[..], [Opnd::Vgpr(_), Opnd::ConstInt(0..=31) | Opnd::Sgpr(0..=105), ..]) {
            return Err(format!("v_readlane_b32 at {:#x}: unsupported source or lane", op.offset));
        }
        op.enc = Enc::Sop1;
        op.dst = vec![Opnd::Sgpr(dst)];
        op.src.truncate(2);
        op.mnemonic = super::indexed_lane::WAVE32_READ_LANE.into();
    }
    Ok(())
}

/// The block dispatcher runs each invocation through its own blocks, so a vote
/// that a reduction's guard needs may not find the whole wave there.
pub(super) const DISPATCHED_VOTE: &str = "wave reduction EXECZ vote in a block-dispatched program";

thread_local!(static INSTEAD_OF_GUARDS: Cell<bool> = const { Cell::new(false) });

/// Whether this translation branches with the whole wave where a guard would vote.
pub(super) fn instead_of_guards() -> bool { INSTEAD_OF_GUARDS.with(Cell::get) }

/// Translate with every branch a decision of the whole wave, which the block
/// dispatcher keeps, instead of the votes of single guards, which it cannot.
pub(super) fn without_guards<T>(run: impl FnOnce() -> T) -> T {
    INSTEAD_OF_GUARDS.with(|c| c.set(true));
    let result = run();
    INSTEAD_OF_GUARDS.with(|c| c.set(false));
    result
}

/// What a scalar register holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// The same value in every lane.
    Uniform,
    /// A lane mask, of which each invocation holds its own bit.
    Mask,
    /// All zeros or all ones, which reads correctly either way.
    Constant,
    /// A uniform value on one path and a mask on another.
    Conflict,
}

impl Class {
    fn join(self, other: Self) -> Self {
        use Class::*;
        match (self, other) {
            (a, b) if a == b => a,
            (Constant, b) => b,
            (a, Constant) => a,
            _ => Conflict,
        }
    }

    fn is_value(self) -> bool {
        matches!(self, Class::Uniform | Class::Constant)
    }

    fn is_mask(self) -> bool {
        matches!(self, Class::Mask | Class::Constant)
    }
}

/// A condition code to recompute across the wave after one instruction.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Vote {
    pub mask: Opnd,
    /// The code is set when no lane's bit is.
    pub none: bool,
    /// The destination receives the number of lanes in the mask.
    pub count: bool,
}

/// What the emitter adds to a program whose branches are decisions of the whole wave.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Proof {
    /// The condition codes to vote after the instructions that set them.
    pub votes: BTreeMap<usize, Vote>,
    /// Instructions that count the lanes of a mask or find its first, and so
    /// read the whole wave's bits where each invocation holds one.
    pub ballots: BTreeSet<usize>,
}

const VCC: u16 = 106;
const EXEC: u16 = 126;
const TEMPS: u16 = 256;

fn register(operand: &Opnd) -> Option<u16> {
    Some(match *operand {
        Opnd::Sgpr(n) => u16::from(n),
        Opnd::VccLo => VCC,
        Opnd::VccHi => VCC + 1,
        Opnd::ExecLo => EXEC,
        Opnd::ExecHi => EXEC + 1,
        Opnd::Temp(n) => TEMPS.checked_add(n)?,
        _ => return None,
    })
}

#[derive(Clone, PartialEq)]
struct State(BTreeMap<u16, Class>);

impl State {
    fn entry() -> Self {
        State(BTreeMap::from([
            (VCC, Class::Constant),
            (VCC + 1, Class::Constant),
            (EXEC, Class::Mask),
            (EXEC + 1, Class::Mask),
        ]))
    }

    fn get(&self, register: u16) -> Class {
        // A register nothing has written holds what the wave started with.
        self.0.get(&register).copied().unwrap_or(Class::Uniform)
    }

    fn set(&mut self, register: u16, width: u16, class: Class) {
        for step in 0..width {
            self.0.insert(register + step, class);
        }
    }

    /// The class of `width` words of one source operand.
    fn read(&self, operand: &Opnd, width: u16) -> Class {
        match *operand {
            Opnd::ConstInt(0 | -1) | Opnd::Literal(u32::MAX) => Class::Constant,
            _ => match register(operand) {
                Some(first) => (1..width).fold(self.get(first), |class, step| class.join(self.get(first + step))),
                None => Class::Uniform,
            },
        }
    }

    fn join(&mut self, other: &State) -> bool {
        let before = self.clone();
        let keys: Vec<u16> = self.0.keys().chain(other.0.keys()).copied().collect();
        for key in keys {
            let class = self.get(key).join(other.get(key));
            self.0.insert(key, class);
        }
        *self != before
    }
}

fn wide(mnemonic: &str) -> u16 {
    if ["_b64", "_u64", "_i64"].iter().any(|suffix| mnemonic.ends_with(suffix)) { 2 } else { 1 }
}

fn logic(mnemonic: &str) -> bool {
    ["s_and_b", "s_or_b", "s_xor_b", "s_andn2_b", "s_orn2_b", "s_nand_b", "s_nor_b", "s_xnor_b", "s_not_b"]
        .iter()
        .any(|prefix| mnemonic.starts_with(prefix))
}

/// How many of a vector instruction's decoded sources it reads. The long
/// encoding has three source fields whether or not the operation uses them.
fn operands(op: &Insn) -> usize {
    let name = op.mnemonic.as_str();
    if name == "v_nop" {
        return 0;
    }
    const THREE: [&str; 22] = ["mad", "fma", "bfe", "bfi", "alignbit", "alignbyte", "min3", "max3", "med3", "lerp",
        "sad", "cube", "div_fixup", "div_fmas", "div_scale", "xor3", "add3", "or3", "and_or", "lshl_or", "lshl_add",
        "perm"];
    let three = THREE.iter().any(|key| name.contains(key)) || name.starts_with("v_cndmask") || name.contains("_ci_")
        || name.starts_with("v_add_lshl") || name.starts_with("v_xad");
    // The accumulating forms add to their destination.
    let three = three && !name.starts_with("v_mac_") && !name.starts_with("v_fmac_");
    const ONE: [&str; 15] = ["v_mov_", "v_cvt_", "v_rcp_", "v_rsq_", "v_sqrt_", "v_rndne_", "v_floor_", "v_ceil_",
        "v_trunc_", "v_fract_", "v_exp_", "v_log_", "v_sin_", "v_cos_", "v_frexp_"];
    let used = if op.enc != Enc::Vop3 { 3 } else if ONE.iter().any(|prefix| name.starts_with(prefix))
        || matches!(name, "v_not_b32" | "v_bfrev_b32") { 1 } else if three { 3 } else { 2 };
    used.min(op.src.len())
}

/// Apply `op` to `state`. `None` refuses the program. `ballot` is set when the
/// instruction reads its mask operand as the whole wave's.
fn step(op: &Insn, state: &mut State, width: u16, ballot: &mut bool) -> Option<Option<Vote>> {
    let name = op.mnemonic.as_str();
    if matches!(name, "s_setpc_b64" | "s_swappc_b64" | "s_call_b64" | "s_rfe_b64" | "s_trap") {
        return None;
    }
    match op.enc {
        Enc::Sopp => {
            let valid = match op.op {
                6 | 7 => state.get(VCC).is_mask(),
                8 | 9 => state.get(EXEC).is_mask(),
                _ => true,
            };
            valid.then_some(None)
        }
        Enc::Sop1 | Enc::Sop2 | Enc::Sopc | Enc::Sopk => {
            if name.starts_with("s_setreg") {
                return Some(None);
            }
            let size = wide(name);
            let destination = op.dst.first().and_then(register);
            let mut inputs = op.src.iter().fold(Class::Constant, |class, source| class.join(state.read(source, size)));
            // SOPK arithmetic and compares read the register they name.
            if op.enc == Enc::Sopk && !name.starts_with("s_movk") && !name.starts_with("s_getreg") {
                inputs = inputs.join(state.read(op.dst.first()?, 1));
            }
            // A copy of a register that is a mask on one path and a number on another
            // is as unusable as its source. Whatever reads the copy is refused, so a
            // restore of EXEC that nothing runs under does not refuse the program.
            if inputs == Class::Conflict && matches!(name, "s_mov_b32" | "s_mov_b64") {
                state.set(op.dst.first().and_then(register)?, size, Class::Conflict);
                return Some(None);
            }
            // A literal other than the two masks is an ordinary number.
            if inputs == Class::Conflict {
                return None;
            }
            if name == "s_andn2_wrexec_b32" {
                // EXEC and the destination: the source's lanes that EXEC does not hold.
                let combined = state.get(EXEC).join(state.read(op.src.first()?, 1));
                if !combined.is_mask() {
                    return None;
                }
                state.set(destination?, 1, combined);
                state.set(EXEC, 1, combined);
                return Some((combined == Class::Mask).then_some(Vote { mask: Opnd::ExecLo, none: false, count: false }));
            }
            // In a 32-lane wave a count the emitter takes after the instruction. It
            // replaces the destination, so a count into its own mask is left to the
            // ballot below, which reads the mask first.
            if name == "s_bcnt1_i32_b32" && inputs == Class::Mask && width == 1 && op.dst.first() != op.src.first() {
                state.set(destination?, 1, Class::Uniform);
                return Some(Some(Vote { mask: *op.src.first()?, none: false, count: true }));
            }
            // The wave arrives together, so the bits its lanes hold are the whole
            // mask. EXEC names its own half; another register does when the count
            // is as wide as the wave.
            if (name.starts_with("s_ff1_i32_b") || name.starts_with("s_bcnt1_i32_b")) && inputs == Class::Mask {
                if !matches!(op.src[..], [Opnd::ExecLo | Opnd::ExecHi]) && size != width {
                    return None;
                }
                *ballot = true;
                state.set(destination?, 1, Class::Uniform);
                return Some(None);
            }
            // The lane a group exchange reads is the same for the whole wave.
            if name == "tb_indexed_readlane" || name == super::GROUP_READ {
                if !inputs.is_value() {
                    return None;
                }
                state.set(destination?, 1, Class::Uniform);
                return Some(None);
            }
            if name == ray_resolve::READ_LANE
                || name == super::indexed_lane::WAVE32_READ_LANE
                || name == COUNT_LANES
                || name == "tb_first_active_lane"
                || (name.starts_with("s_ff1_i32") && op.src == [Opnd::ExecLo])
            {
                state.set(destination?, 1, Class::Uniform);
                return Some(None);
            }
            // A prefix sum over the wave, which is together here, written under EXEC.
            if name == super::wave_alloc::PREFIX {
                return state.get(EXEC).is_mask().then_some(None);
            }
            if name.starts_with("tb_") {
                return None;
            }
            if name.contains("saveexec") {
                let exec = state.get(EXEC);
                let combined = exec.join(state.read(op.src.first()?, size));
                if !combined.is_mask() {
                    return None;
                }
                state.set(destination?, size, exec);
                state.set(EXEC, size, combined);
                return Some((combined == Class::Mask).then_some(Vote { mask: Opnd::ExecLo, none: false, count: false }));
            }
            // The new EXEC is also what the destination receives.
            if name.contains("wrexec") {
                let combined = state.get(EXEC).join(state.read(op.src.first()?, size));
                if !combined.is_mask() {
                    return None;
                }
                state.set(destination?, size, combined);
                state.set(EXEC, size, combined);
                return Some((combined == Class::Mask).then_some(Vote { mask: Opnd::ExecLo, none: false, count: false }));
            }
            if op.enc == Enc::Sopc {
                if inputs.is_value() {
                    return Some(None);
                }
                // A whole mask compared with zero asks whether any lane is set.
                let none = if name.starts_with("s_cmp_eq_") { true }
                    else if name.starts_with("s_cmp_lg_") { false }
                    else { return None };
                return match op.src[..] {
                    [mask, Opnd::ConstInt(0)] | [Opnd::ConstInt(0), mask] => Some(Some(Vote { mask, none, count: false })),
                    _ => None,
                };
            }
            // The program counter takes no input, whatever the source field says.
            if name == "s_getpc_b64" {
                state.set(op.dst.first().and_then(register)?, 2, Class::Uniform);
                return Some(None);
            }
            if op.dst == [Opnd::Null] {
                return Some(None);
            }
            let destination = destination?;
            if matches!(name, "s_mov_b32" | "s_mov_b64") {
                state.set(destination, size, inputs);
                return Some(None);
            }
            if name.starts_with("s_cselect_b") {
                state.set(destination, size, inputs);
                return Some(None);
            }
            // The emitter votes the condition code of a whole-quad EXEC itself.
            if name.starts_with("s_wqm_b") && op.src == [Opnd::ExecLo] {
                state.set(destination, size, inputs);
                return Some(None);
            }
            if logic(name) {
                state.set(destination, size, inputs);
                return Some((inputs == Class::Mask).then_some(Vote { mask: op.dst[0], none: false, count: false }));
            }
            if !inputs.is_value() {
                return None;
            }
            state.set(destination, size, Class::Uniform);
            Some(None)
        }
        Enc::Smem => {
            if !name.contains("load_dword") {
                return None;
            }
            let base = if name.starts_with("s_buffer") { 4 } else { 2 };
            let offset = u16::try_from(op.raw.get(1)? >> 25).ok()?;
            if !op.src.iter().all(|source| state.read(source, base).is_value())
                || (offset < 106 && !state.get(offset).is_value())
            {
                return None;
            }
            let words = [("x16", 16), ("x8", 8), ("x4", 4), ("x2", 2)]
                .iter()
                .find(|(suffix, _)| name.ends_with(suffix))
                .map_or(1, |&(_, words)| words);
            state.set(register(op.dst.first()?)?, words, Class::Uniform);
            Some(None)
        }
        Enc::Vop1 | Enc::Vop2 | Enc::Vop3 | Enc::Vopc => {
            if !state.get(EXEC).is_mask() {
                return None;
            }
            let lane_read = matches!(name, "v_readlane_b32" | "v_readfirstlane_b32") || name == ray_resolve::READ_LANE;
            let carry = name.contains("_co_") || name.contains("_ci_") || name.starts_with("v_div_scale")
                || name.starts_with("v_mad_u64") || name.starts_with("v_mad_i64");
            let selects = name.starts_with("v_cndmask") || name.contains("_ci_") || name.starts_with("v_div_fmas");
            // The 32-bit encodings read and write VCC without naming it.
            if selects && op.src.len() < 3 && !state.get(VCC).is_mask() {
                return None;
            }
            for (position, source) in op.src.iter().enumerate().take(operands(op)) {
                if register(source).is_none() {
                    continue;
                }
                let class = state.read(source, wide(name));
                // The lanes of a mask below this one. The emitter takes the ballot of
                // EXEC itself; of another register only the low word of a narrow wave.
                if name.starts_with("v_mbcnt_") && position == 0 && class == Class::Mask {
                    if !matches!(source, Opnd::ExecLo | Opnd::ExecHi) {
                        (name == "v_mbcnt_lo_u32_b32" && width == 1).then_some(())?;
                        *ballot = true;
                    }
                    continue;
                }
                if selects && position == 2 { class.is_mask() } else { class.is_value() }.then_some(())?;
            }
            if name.starts_with("v_cmp") {
                for destination in op.dst.iter().filter_map(register) {
                    state.set(destination, width, Class::Mask);
                }
                if name.starts_with("v_cmpx") {
                    state.set(EXEC, width, Class::Mask);
                } else if op.dst.is_empty() {
                    state.set(VCC, width, Class::Mask);
                }
                return Some(None);
            }
            for destination in op.dst.iter().filter_map(register) {
                if !lane_read {
                    return None;
                }
                state.set(destination, 1, Class::Uniform);
            }
            if carry {
                if op.enc == Enc::Vop3 {
                    state.set(u16::try_from((op.raw.first()? >> 8) & 0x7f).ok()?, width, Class::Mask);
                } else {
                    state.set(VCC, width, Class::Mask);
                }
            }
            Some(None)
        }
        _ => {
            // Memory, interpolation and export instructions take descriptors and
            // offsets from scalar registers, and write none.
            // Only sampling reads the sampler an image instruction names.
            let used = if op.enc == Enc::Mimg && !["sample", "gather", "lod"].iter().any(|k| name.contains(k)) {
                2
            } else {
                op.src.len()
            };
            // A descriptor is four words. The offset register of a buffer access is one.
            let words = |position| if matches!(op.enc, Enc::Mubuf | Enc::Mtbuf) && position == 2 { 1 } else { 4 };
            // These run under EXEC as the vector instructions do.
            if !state.get(EXEC).is_mask()
                || op.dst.iter().any(|destination| register(destination).is_some())
                || !op.src.iter().enumerate().take(used).filter(|(_, source)| register(source).is_some())
                    .all(|(position, source)| state.read(source, words(position)).is_value())
            {
                return None;
            }
            Some(None)
        }
    }
}

/// The votes and ballots that make every branch of the program one decision per
/// wave of `width` lanes, or `None` when that cannot be shown.
pub(super) fn prove(insns: &[Insn], width: u32) -> Option<Proof> {
    let words = u16::try_from(width / 32).ok()?;
    let index: BTreeMap<i64, usize> = insns.iter().enumerate().map(|(i, op)| (op.offset as i64, i)).collect();
    let end = insns.last().map_or(0, |op| (op.offset + op.dwords * 4) as i64);
    let mut states: Vec<Option<State>> = vec![None; insns.len()];
    if let Some(first) = states.first_mut() {
        *first = Some(State::entry());
    }
    let mut pending = vec![0usize];
    let mut proof = Proof::default();
    while let Some(at) = pending.pop() {
        let Some(op) = insns.get(at) else { continue };
        let mut state = states[at].clone()?;
        // Classes only move towards `Conflict`, so a later visit may need a vote
        // or a ballot an earlier one did not, or refuse, but never the reverse.
        let mut ballot = false;
        if let Some(vote) = step(op, &mut state, words, &mut ballot)? {
            proof.votes.insert(at, vote);
        }
        if ballot {
            proof.ballots.insert(at);
        }
        let mut next = Vec::with_capacity(2);
        let branch = op.enc == Enc::Sopp && Emitter::branch_cond(op.op).is_some();
        if branch {
            let target = op.offset as i64 + 4 + 4 * op.aux.imm;
            if target < end {
                next.push(*index.get(&target)?);
            }
        }
        if !(op.enc == Enc::Sopp && (op.op == 1 || (branch && op.op == 2))) {
            next.push(at + 1);
        }
        for successor in next.into_iter().filter(|&successor| successor < insns.len()) {
            let changed = match &mut states[successor] {
                Some(existing) => existing.join(&state),
                empty => {
                    *empty = Some(state.clone());
                    true
                }
            };
            if changed {
                pending.push(successor);
            }
        }
    }
    Some(proof)
}

/// The whole wave's word of the mask an instruction counts, where each
/// invocation holds its own bit of it. `mask` names the low word.
pub(super) fn word(em: &Emitter<'_>, at: usize, mask: &Opnd, high: bool) -> Option<String> {
    em.lockstep.as_ref().filter(|proof| proof.ballots.contains(&at))?;
    let (mask, high) = match mask {
        Opnd::ExecHi => (&Opnd::ExecLo, true),
        other => (other, high),
    };
    Some(format!("WaveActiveBallot(({} & 1u) != 0u).{}", r(mask).ok()?, if high { "y" } else { "x" }))
}

/// The wave-wide form of a branch on a lane mask.
pub(super) fn branch_condition(op: u32) -> Option<String> {
    let (register, taken_when_none) = match op {
        6 => ("vcc_lo", true),
        7 => ("vcc_lo", false),
        8 => ("exec_lo", true),
        9 => ("exec_lo", false),
        _ => return None,
    };
    let not = if taken_when_none { "!" } else { "" };
    Some(format!("{not}WaveActiveAnyTrue(({register} & 1u) != 0u)"))
}

#[cfg(test)]
#[path = "../tests/hlsl/lockstep.rs"]
mod tests;
