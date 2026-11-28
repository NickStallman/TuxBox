//! Lane masks read as numbers.
//!
//! One invocation holds only its own bit of a lane mask. Counting a mask's lanes, finding
//! the first one or counting those before this lane reads all of them, often after EXEC has
//! been narrowed to one elected lane. Take the ballot where the mask is computed, while
//! every lane that can be in it is there, and let each lane keep it.
use super::{Emitter, Enc, Insn, Opnd};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

thread_local!(static OFF: Cell<bool> = const { Cell::new(false) });

/// Whether this translation leaves lane masks as each lane's own bit.
pub(super) fn off() -> bool { OFF.with(Cell::get) }

/// Translate without whole-mask ballots, as before they were taken.
pub(super) fn without<T>(run: impl FnOnce() -> T) -> T {
    OFF.with(|c| c.set(true));
    let result = run();
    OFF.with(|c| c.set(false));
    result
}

const VCC: u8 = 106;

/// The scalar register an operand names. VCC follows the ordinary registers.
pub(super) fn register(o: &Opnd) -> Option<u8> {
    match *o {
        Opnd::Sgpr(n) => Some(n),
        Opnd::VccLo => Some(VCC),
        Opnd::VccHi => Some(VCC + 1),
        _ => None,
    }
}

pub(super) fn name(register: u8) -> String { format!("tb_bal{register}") }

/// The scalar registers written: the first of each group and how many.
fn written(ins: &Insn, wide: bool) -> Vec<(u8, u8)> {
    let m = ins.mnemonic.as_str();
    let mask = if wide { 2 } else { 1 };
    let Some(d) = ins.dst.first() else { return Vec::new() };
    let one = |count: u8| register(d).map(|n| (n, count)).into_iter().collect::<Vec<_>>();
    if m.starts_with("s_load_") || m.starts_with("s_buffer_load_") {
        return one(["x16", "x8", "x4", "x2"].iter().zip([16, 8, 4, 2]).find(|(s, _)| m.ends_with(*s)).map_or(1, |(_, w)| w));
    }
    if m.starts_with("s_") {
        if m.starts_with("s_cmp") || m.starts_with("s_bitcmp") { return Vec::new(); }
        let single = ["s_bcnt", "s_ff", "s_flbit"].iter().any(|p| m.starts_with(p));
        let pair = m.ends_with("64") || matches!(m, "s_bitreplicate_b64_b32" | "s_memtime" | "s_memrealtime");
        return one(if pair && !single { 2 } else { 1 });
    }
    if matches!(m, "v_readlane_b32" | "v_readfirstlane_b32") {
        // The VOP3 encoding keeps the scalar destination in the vector field.
        return match *d { Opnd::Vgpr(n) => vec![(n, 1)], _ => one(1) };
    }
    if m.starts_with("v_cmpx") { return Vec::new(); }
    if m.starts_with("v_cmp") { return one(mask); }
    if m.starts_with("v_") {
        let carry = m.contains("_co_");
        return match ins.enc {
            Enc::Vop3 if carry || matches!(m, "v_mad_u64_u32" | "v_mad_i64_i32") || m.starts_with("v_div_scale") =>
                vec![(((ins.raw[0] >> 8) & 127) as u8, mask)],
            Enc::Vop2 if carry => vec![(VCC, mask)],
            _ => ins.dst.iter().filter_map(register).map(|n| (n, mask)).collect(),
        };
    }
    one(if m.starts_with("tb_") && m.ends_with("64") { 2 } else { 1 })
}

const BITWISE: [&str; 11] = ["s_mov_b", "s_and_b", "s_or_b", "s_xor_b", "s_andn2_b", "s_orn2_b", "s_nand_b", "s_nor_b",
    "s_xnor_b", "s_not_b", "s_cselect_b"];

enum Kind { Mask, Of(Vec<u8>), AllLanes, Number }

/// What an instruction's scalar result is made of.
fn kind(ins: &Insn, wide: bool) -> Kind {
    let m = ins.mnemonic.as_str();
    if m.starts_with("v_cmp") || m.contains("_co_") || m.starts_with("v_div_scale") { return Kind::Mask; }
    let size = if wide { "64" } else { "32" };
    if m.contains("saveexec") { return if m.ends_with(size) { Kind::Mask } else { Kind::Number }; }
    // A mask is moved and combined as a whole.
    if !BITWISE.iter().any(|p| m.strip_prefix(p) == Some(size)) { return Kind::Number; }
    let (mut exec, mut from) = (false, Vec::new());
    for o in &ins.src {
        match o {
            Opnd::ExecLo => exec = true,
            Opnd::ConstInt(0 | -1) => (),
            _ => match register(o) { Some(n) => from.push(n), None => return Kind::Number },
        }
    }
    match (exec, from.is_empty()) {
        (_, false) => Kind::Of(from),
        (true, true) => Kind::Mask,
        (false, true) if ins.src.contains(&Opnd::ConstInt(-1)) => Kind::AllLanes,
        (false, true) => Kind::Of(Vec::new()),
    }
}

fn predecessors(insns: &[Insn]) -> Vec<Vec<usize>> {
    let at: BTreeMap<i64, usize> = insns.iter().enumerate().map(|(i, x)| (x.offset as i64, i)).collect();
    let mut from = vec![Vec::new(); insns.len()];
    for (i, ins) in insns.iter().enumerate() {
        let sopp = ins.enc == Enc::Sopp;
        if i + 1 < insns.len() && !(sopp && matches!(ins.op, 1 | 2)) { from[i + 1].push(i); }
        if sopp && Emitter::branch_cond(ins.op).is_some() {
            if let Some(&t) = at.get(&(ins.offset as i64 + 4 + 4 * ins.aux.imm)) { from[t].push(i); }
        }
    }
    from
}

/// Which lanes of a wave run an instruction together.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Together { All, Some }

/// Branches of the console take the whole wave. Those on EXEC or VCC take it here too (see
/// `Ballots::branches`); a branch on SCC that can come from a mask takes one lane, so lanes
/// part in what it jumps over or repeats, and after an early exit.
fn together(insns: &[Insn]) -> Vec<Together> {
    let branches = || insns.iter().enumerate().filter(|(_, x)| x.enc == Enc::Sopp && Emitter::branch_cond(x.op).is_some());
    let target = |x: &Insn| x.offset as i64 + 4 + 4 * x.aux.imm;
    let targets: BTreeSet<i64> = branches().map(|(_, x)| target(x)).collect();
    let end = |x: &Insn| x.enc == Enc::Sopp && x.op == 1;
    let exits = insns.iter().position(end).is_some_and(|first| insns[first..].iter().any(|x| !end(x) && x.enc != Enc::Sopp));
    let mut found = vec![Together::All; insns.len()];
    for (i, branch) in branches() {
        let (from, to) = (branch.offset as i64, target(branch));
        let lanes = match branch.op {
            2 => continue,
            4 | 5 if uniform_condition(insns, i, &targets) => continue,
            6..=9 => continue,
            _ => Together::Some,
        };
        for (k, x) in insns.iter().enumerate() {
            if (from.min(to)..from.max(to)).contains(&(x.offset as i64)) || exits && k > i { found[k] = found[k].max(lanes); }
        }
    }
    found
}

/// Whether the condition code a branch tests is that of numbers every lane agrees on.
fn uniform_condition(insns: &[Insn], branch: usize, targets: &BTreeSet<i64>) -> bool {
    // A lane mask, or something that could be one.
    let mask = |x: &Insn| {
        let m = x.mnemonic.as_str();
        let number = |o: &Opnd| matches!(o, Opnd::Literal(_)) || matches!(o, Opnd::ConstInt(n) if !matches!(n, 0 | -1));
        m.starts_with("v_") && !m.contains("lane") || m.contains("saveexec") || m.starts_with("s_bcnt") || m.starts_with("s_ff")
            || BITWISE.iter().any(|p| m.starts_with(p)) && !x.src.iter().any(number)
                && x.src.iter().any(|o| register(o).is_some() || matches!(o, Opnd::ExecLo | Opnd::ExecHi))
    };
    let number = |o: &Opnd| match register(o) {
        Some(n) => !insns.iter().any(|x| mask(x) && written(x, true).iter().any(|&(first, count)| (first..first + count).contains(&n))),
        None => matches!(o, Opnd::ConstInt(_) | Opnd::ConstF32(_) | Opnd::Literal(_)),
    };
    let mut at = branch;
    while at > 0 && !targets.contains(&(insns[at].offset as i64)) {
        at -= 1;
        let x = &insns[at];
        let m = x.mnemonic.as_str();
        if m.starts_with("s_cmp") || m.starts_with("s_bitcmp") {
            let compared = x.dst.iter().filter(|_| x.enc == Enc::Sopk);
            return x.src.iter().chain(compared).all(number);
        }
        // These leave the condition code alone.
        let leaves = ["s_mov", "s_cselect", "s_cmov", "s_ff1", "s_mul", "s_load", "s_buffer_load", "s_nop", "s_waitcnt", "s_inst_prefetch"];
        if m.starts_with("s_") && !leaves.iter().any(|p| m.starts_with(p)) { return false; }
    }
    false
}

struct Reach<'a> { insns: &'a [Insn], from: Vec<Vec<usize>>, together: Vec<Together>, wide: bool }

impl Reach<'_> {
    /// The writes of `register` that reach `at`. None when its entry value does.
    fn defs(&self, at: usize, register: u8) -> Option<BTreeSet<usize>> {
        let (mut found, mut seen, mut todo) = (BTreeSet::new(), BTreeSet::new(), vec![at]);
        while let Some(i) = todo.pop() {
            if i == 0 { return None; }
            // Padding after an unconditional branch is never entered.
            for &p in &self.from[i] {
                if !seen.insert(p) { continue; }
                if self.writes(p, register).is_some() { found.insert(p); } else { todo.push(p); }
            }
        }
        Some(found)
    }

    /// The group of registers instruction `at` writes that `register` is part of.
    fn writes(&self, at: usize, register: u8) -> Option<(u8, u8)> {
        written(&self.insns[at], self.wide).into_iter().find(|&(first, count)| (first..first.saturating_add(count)).contains(&register))
    }

    /// The definitions of a mask read at `at` and its first register, if every value that
    /// reaches any of its words is a whole mask.
    fn mask(&self, at: usize, register: u8) -> Option<(u8, BTreeSet<usize>)> {
        let direct = self.defs(at, register)?;
        let words = if self.wide { 2 } else { 1 };
        let mut first = None;
        for &d in &direct {
            let (n, count) = self.writes(d, register)?;
            if count != words || first.is_some_and(|f| f != n) { return None; }
            first = Some(n);
        }
        let first = first?;
        // The words of a pair are read and written together.
        if (first..first + words).any(|n| self.defs(at, n).as_ref() != Some(&direct)) { return None; }
        let (mut seen, mut todo, mut rooted) = (BTreeSet::new(), direct.iter().copied().collect::<Vec<_>>(), false);
        while let Some(d) = todo.pop() {
            if !seen.insert(d) { continue; }
            let captured = direct.contains(&d);
            // Lanes that went another way would be missing from its ballot.
            if captured && self.together[d] == Together::Some { return None; }
            match kind(&self.insns[d], self.wide) {
                Kind::Number => return None,
                // Counted itself, it holds lanes that are not running.
                Kind::AllLanes if captured => return None,
                Kind::AllLanes => (),
                Kind::Mask => rooted = true,
                Kind::Of(regs) => for n in regs {
                    for word in n..n + words { todo.extend(self.defs(d, word)?); }
                },
            }
        }
        rooted.then_some((first, direct))
    }
}

fn counted(ins: &Insn) -> bool {
    matches!(ins.mnemonic.as_str(), "s_bcnt1_i32_b32" | "s_bcnt1_i32_b64" | "s_ff1_i32_b32" | "s_ff1_i32_b64"
        | "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32")
}

pub(super) struct Ballots {
    /// Registers to take a ballot of after an instruction.
    pub defs: BTreeMap<usize, BTreeSet<u8>>,
    /// Instructions that read ballots instead of their mask operand.
    pub uses: BTreeSet<usize>,
    /// Branches on an empty or non-empty EXEC or VCC, decided for the whole wave as the
    /// console does. Each lane holds only its own bit, so a lane deciding alone would skip
    /// scalar writes, barriers and mask bits the rest of its wave still runs.
    pub branches: BTreeSet<usize>,
    pub width: u32,
}

impl Ballots {
    /// The variables the ballots are kept in.
    pub(super) fn names(&self) -> Vec<String> {
        let words = u8::from(self.width == 64);
        self.defs.values().flatten().flat_map(|&n| [n, n + words]).map(name).collect()
    }
}

/// Masks of a compute program that are read as numbers.
pub(super) fn find(insns: &[Insn]) -> Option<Ballots> {
    if insns.iter().any(|i| matches!(i.mnemonic.as_str(), "s_setpc_b64" | "s_swappc_b64" | "s_call_b64")
        || i.mnemonic.starts_with("s_movrel")) { return None; }
    let masks = |bits: &str| insns.iter().any(|i| i.mnemonic.starts_with("s_") && i.mnemonic.ends_with(bits)
        && (i.mnemonic.contains("saveexec") || i.src.contains(&Opnd::ExecLo) || i.dst.contains(&Opnd::ExecLo)));
    // Only a wave of 64 lanes has a second word.
    let second = insns.iter().any(|i| i.mnemonic == "v_mbcnt_hi_u32_b32" || i.src.iter().chain(&i.dst).any(|o| *o == Opnd::ExecHi));
    let wide = match (masks("_b32"), masks("_b64")) {
        (true, false) if !second => false,
        (_, true) if !masks("_b32") || second => true,
        _ => return None,
    };
    let reach = Reach { insns, from: predecessors(insns), together: together(insns), wide };
    let mut found = Ballots { defs: BTreeMap::new(), uses: BTreeSet::new(), branches: BTreeSet::new(), width: if wide { 64 } else { 32 } };
    found.branches = insns.iter().enumerate().filter(|(_, i)| i.enc == Enc::Sopp && (6..=9).contains(&i.op)).map(|(k, _)| k).collect();
    for (i, ins) in insns.iter().enumerate().filter(|(_, i)| counted(i)) {
        let Some(source) = ins.src.first() else { continue };
        // The emitter already takes the ballot of EXEC for the other two.
        if matches!(source, Opnd::ExecLo | Opnd::ExecHi) {
            if ins.mnemonic.starts_with("s_bcnt1") || ins.mnemonic == "s_ff1_i32_b32" { found.uses.insert(i); }
            continue;
        }
        let Some(register) = register(source) else { continue };
        let Some((first, defs)) = reach.mask(i, register) else { continue };
        // A 64-bit count starts at the mask's first word, and narrow waves have no second.
        if ins.mnemonic.ends_with("_b64") && (!wide || register != first) { continue; }
        for d in defs { found.defs.entry(d).or_default().insert(first); }
        found.uses.insert(i);
    }
    (!found.uses.is_empty() || !found.branches.is_empty()).then_some(found)
}

/// The condition of branch `at` when the whole wave decides it.
pub(super) fn branch(em: &Emitter<'_>, at: usize, op: u32) -> Option<String> {
    em.mask_ballots.as_ref().filter(|b| b.branches.contains(&at))?;
    let (mask, empty) = match op { 6 => ("vcc_lo", true), 7 => ("vcc_lo", false), 8 => ("exec_lo", true), 9 => ("exec_lo", false), _ => return None };
    Some(format!("{}WaveActiveAnyTrue(({mask} & 1u) != 0u)", if empty { "!" } else { "" }))
}

/// The text of a mask word read by instruction `at`.
pub(super) fn word(em: &Emitter<'_>, at: usize, o: &Opnd) -> Option<String> {
    let ballots = em.mask_ballots.as_ref().filter(|b| b.uses.contains(&at))?;
    match o {
        Opnd::ExecLo => Some("WaveActiveBallot((exec_lo & 1u) != 0u).x".into()),
        Opnd::ExecHi => Some(if ballots.width == 64 { "WaveActiveBallot((exec_lo & 1u) != 0u).y" } else { "0u" }.into()),
        _ => register(o).map(name),
    }
}

/// Statements that take the ballots due after instruction `at`.
pub(super) fn capture(em: &Emitter<'_>, at: usize) -> Vec<String> {
    let Some(ballots) = &em.mask_ballots else { return Vec::new() };
    ballots.defs.get(&at).into_iter().flatten().map(|&n| {
        let mask = if n == VCC { "vcc_lo".to_owned() } else { format!("s{n}") };
        if ballots.width == 64 {
            format!("{{ uint4 tb_b = WaveActiveBallot(({mask} & 1u) != 0u); {} = tb_b.x; {} = tb_b.y; }}", name(n), name(n + 1))
        } else {
            format!("{} = WaveActiveBallot(({mask} & 1u) != 0u).x;", name(n))
        }
    }).collect()
}

#[cfg(test)]
#[path = "../tests/hlsl/mask_ballots.rs"]
mod tests;
