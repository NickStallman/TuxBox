//! Per-key lane counts added to a buffer by one lane for each key.
//!
//! Ray binning counts the active lanes of a wave by a key each lane holds and
//! adds every count to a typed buffer. The console compiler serves one key per
//! pass (`W` is `b32` or `b64` with the wave width):
//!
//! ```text
//!         v_mov_b32 vF, 1
//!   head: v_cmp_ne_u32 vcc, 0, vF            ; the lanes still to serve
//!         v_mov_b32 vF, 0
//!         s_and_saveexec_W sOuter, vcc
//!         s_cbranch_execz exit
//!         v_readfirstlane_b32 sKey, vKey     ; the first one's key
//!         [v_cmp_* vcc, <constant>, vKey]    ; lanes that count at all, or sValid
//!         v_cmp_eq_u32 sM, sKey, vKey
//!         s_and_W sA, vcc|sValid, sM
//!         s_and_saveexec_W sPend, sA         ; EXEC: counting lanes with that key
//!         s_cbranch_execz tail
//!         ...                                ; 64-bit sum over the wave of 1 per EXEC lane
//!         v_readlane_b32 sC, vA, 0           ; the count
//!         ...                                ; EXEC: the first of those lanes
//!         v_mov_b32 vCnt, sC
//!         v_mov_b32 vIdx, sKey
//!         buffer_atomic_add vCnt, vIdx, sDesc
//!   tail: s_mov_W exec, sPend
//!         v_cndmask_b32 vF, 0, 1, sM
//!         v_xor_b32 vF, 1, vF                ; lanes with another key remain
//!   exit: s_mov_W exec, sOuter
//!         v_cmp_ne_u32 vcc, 0, vF
//!         s_bcnt1_i32_W sAny, vcc
//!         s_cbranch_scc1 head
//! ```
//!
//! Each pass removes exactly the lanes that hold the served key, so every key
//! among the entry lanes is served once, with the number of counting lanes that
//! hold it. The buffer therefore ends as if each counting lane had added 1 at
//! its own key, and EXEC leaves as it entered. The loop is rewritten to that,
//! in console instructions:
//!
//! ```text
//!         v_mov_b32 vF, 1 ... s_and_saveexec_W sOuter, vcc    ; unchanged
//!         [v_cmp_* vcc, <constant>, vKey]
//!         s_and_W sA, vcc|sValid, vcc|sValid
//!         s_and_saveexec_W sPend, sA
//!         v_mov_b32 vCnt, 1
//!         v_mov_b32 vIdx, vKey
//!         buffer_atomic_add vCnt, vIdx, sDesc
//!         s_mov_W exec, sPend
//!         s_mov_W exec, sOuter
//! ```
//!
//! No lane reads another, so the result does not depend on how the host groups
//! threads into waves. Two things differ from the console and are checked: the
//! registers the loop writes hold other values afterwards, so each must be
//! written again before it is read; and a count arrives in steps of 1, so
//! nothing else in the program may use that buffer (`counters_are_private`).
use super::*;
use crate::rdna2::clobbers_scalar;
use std::cell::Cell;

thread_local!(static REWRITING: Cell<bool> = const { Cell::new(false) });

/// Whether this translation takes the rewrite. It is a last resort: a program
/// the translator handles without it keeps that translation, and only one it
/// refuses is translated again with the loops rewritten (`with_key_counts`).
pub(in crate::hlsl) fn rewriting() -> bool { REWRITING.with(Cell::get) }

/// Run `translate` again with the rewrite, when the program has a loop it takes.
pub(in crate::hlsl) fn with_key_counts<T>(insns: &[Insn], translate: impl FnOnce() -> Result<T, String>)
    -> Option<Result<T, String>> {
    if rewriting() || lower_key_counts(&mut insns.to_vec()).is_empty() {
        return None;
    }
    REWRITING.with(|c| c.set(true));
    let result = translate();
    REWRITING.with(|c| c.set(false));
    Some(result)
}

const VCC: u8 = 106;
const SCC: u8 = 108;

/// Rewrites every loop of the shape above whose leftovers are dead. Returns the
/// indices of the remaining atomic adds.
pub(in crate::hlsl) fn lower_key_counts(insns: &mut [Insn]) -> Vec<usize> {
    // A computed jump could enter a loop past its head.
    if insns.iter().any(|i| matches!(i.mnemonic.as_str(), "s_setpc_b64" | "s_swappc_b64" | "s_call_b64" | "s_rfe_b64")
        || i.mnemonic.contains("movrel") || i.enc == Enc::Unknown || !i.is_complete())
    {
        return Vec::new();
    }
    let index: HashMap<i64, usize> = insns.iter().enumerate().map(|(i, x)| (x.offset as i64, i)).collect();
    let mut found: Vec<Loop> = (0..insns.len()).filter_map(|first| key_count_loop(insns, first, &index)).collect();
    found.retain(|l| !insns.iter().enumerate().any(|(i, b)| {
        !(l.first..=l.last).contains(&i) && target(b).and_then(|t| index.get(&t)).is_some_and(|&t| t > l.first && t <= l.last)
    }));
    // A loop is judged against the rewritten form of the others, so one
    // refusal can expose a read in another: settle before changing anything.
    while let Some(live) = (0..found.len()).find(|&k| !dead_after(insns, &index, &found[k], &found)) {
        found.remove(live);
    }
    for l in &found {
        for slot in l.first..=l.last {
            if !l.keep.contains(&slot) { nop(&mut insns[slot]); }
        }
        let mask = l.valid;
        let and = &mut insns[l.and];
        and.src = vec![mask, mask];
        let code = match mask { Opnd::Sgpr(n) => u32::from(n), _ => u32::from(VCC) };
        and.raw[0] = (and.raw[0] & !0xffff) | (code << 8) | code;
        let count = &mut insns[l.count];
        count.src = vec![Opnd::ConstInt(1)];
        count.raw[0] = (count.raw[0] & !0x1ff) | 129;
        let key = &mut insns[l.index];
        key.src = vec![Opnd::Vgpr(l.key)];
        key.raw[0] = (key.raw[0] & !0x1ff) | 256 | u32::from(l.key);
    }
    found.iter().map(|l| l.atomic).collect()
}

/// A count added one lane at a time passes through values the console's
/// buffer never holds. Nothing else in the program may see them: the buffer
/// must resolve to one typed UAV that only these adds use.
pub(in crate::hlsl) fn counters_are_private(em: &Emitter<'_>, adds: &[usize]) -> Result<(), String> {
    for &add in adds {
        let at = em.insns[add].offset;
        let Some(&[Res::TBuf(slot, true)]) = em.res.get(&add).map(Vec::as_slice) else {
            return Err(format!("key count at {at:#x}: counters are not one typed UAV"));
        };
        let shared = |res: &Res| matches!(*res, Res::TBuf(s, true) | Res::Buf(s, true) | Res::Raw(s, true) | Res::Uav(s, _) if s == slot);
        let other = em.res.iter().filter(|(i, _)| !adds.contains(i)).any(|(_, res)| res.iter().any(shared))
            || em.bsel.iter().any(|(i, (_, list))| adds.contains(i) || list.iter().any(|(_, res)| shared(res)));
        if other {
            return Err(format!("key count at {at:#x}: counters in u{slot} are used elsewhere"));
        }
    }
    Ok(())
}

struct Loop {
    first: usize,
    last: usize,
    /// Instructions that stay; `and`, `count` and `index` change a source.
    keep: Vec<usize>,
    and: usize,
    count: usize,
    index: usize,
    atomic: usize,
    key: u8,
    valid: Opnd,
    descriptor: u8,
    wide: bool,
    /// VGPRs the console loop leaves with other values, in every lane.
    vectors: Vec<u8>,
    /// SGPRs likewise, with VCC as 106 and 107.
    scalars: u128,
}

fn target(op: &Insn) -> Option<i64> {
    (op.enc == Enc::Sopp && Emitter::branch_cond(op.op).is_some()).then(|| op.offset as i64 + 4 + 4 * op.aux.imm)
}

fn padding(op: &Insn) -> bool {
    op.mnemonic == "s_nop" || op.mnemonic == "v_nop" && op.raw == [0x7e00_0000]
}

fn nop(op: &mut Insn) {
    op.mnemonic = "v_nop".into();
    op.enc = Enc::Vop1;
    op.src.clear();
    op.dst = vec![Opnd::Vgpr(0)];
}

/// No modifier the operand lists do not show.
fn plain(op: &Insn) -> bool {
    op.aux.sdwa.is_none() && op.aux.imm == 0 && op.aux.imm2 == 0 && op.aux.mask == 0
        && match op.enc {
            Enc::Vop3 => op.raw.len() == 2 && op.raw[0] & 0xff00 == 0 && op.raw[1] >> 27 == 0,
            // A literal operand follows in a word of its own.
            Enc::Vop1 | Enc::Vop2 | Enc::Vopc | Enc::Sop1 | Enc::Sop2 => {
                op.raw.len() == 1 + op.src.iter().filter(|s| matches!(s, Opnd::Literal(_))).count()
            }
            _ => false,
        }
}

/// Whether `op` writes SGPR `r`. A comparison writes one mask word in a
/// 32-lane wave and a pair in a 64-lane one.
fn writes(op: &Insn, r: u8, wide: bool) -> bool {
    if !wide && op.mnemonic.starts_with("v_cmp") { return op.dst == [Opnd::Sgpr(r)]; }
    clobbers_scalar(op, r)
}

fn sgpr(op: Option<&Opnd>) -> Option<u8> {
    match op { Some(&Opnd::Sgpr(n @ 0..=105)) => Some(n), _ => None }
}

/// The destination of a lane read, which decodes as a VGPR.
fn scalar_dst(op: &Insn) -> Option<u8> {
    match op.dst[..] { [Opnd::Sgpr(n @ 0..=105) | Opnd::Vgpr(n @ 0..=105)] => Some(n), _ => None }
}

struct Cursor<'a> {
    insns: &'a [Insn],
    at: usize,
}

impl<'a> Cursor<'a> {
    /// The next instruction that is not padding, and its index.
    fn next(&mut self) -> Option<(usize, &'a Insn)> {
        while padding(self.insns.get(self.at)?) { self.at += 1; }
        self.at += 1;
        Some((self.at - 1, &self.insns[self.at - 1]))
    }

    fn peek(&self) -> Option<&'a Insn> {
        self.insns[self.at.min(self.insns.len())..].iter().find(|op| !padding(op))
    }

    /// `v_mov_b32 vD, <value>` with a full-width destination.
    fn mov(&mut self, value: impl Fn(&Opnd) -> bool) -> Option<(usize, u8)> {
        let (i, op) = self.next()?;
        match (&op.dst[..], &op.src[..]) {
            ([Opnd::Vgpr(d)], [s]) if op.mnemonic == "v_mov_b32" && op.enc == Enc::Vop1 && plain(op) && value(s) => Some((i, *d)),
            _ => None,
        }
    }

    fn exact(&mut self, mnemonic: &str, enc: Enc, dst: &[Opnd], src: &[Opnd]) -> Option<usize> {
        let (i, op) = self.next()?;
        // Two-source VOP3 forms decode with an unused third field.
        let sources = if enc == Enc::Vop3 && src.len() == 2 { &op.src[..op.src.len().min(2)] } else { &op.src[..] };
        (op.mnemonic == mnemonic && op.enc == enc && plain(op) && op.dst == dst && sources == src).then_some(i)
    }

    fn raw(&mut self, words: &[u32]) -> Option<usize> {
        let (i, op) = self.next()?;
        (op.raw == words).then_some(i)
    }

    fn branch(&mut self, op: u32) -> Option<(usize, i64)> {
        let (i, ins) = self.next()?;
        (ins.enc == Enc::Sopp && ins.op == op).then(|| target(ins)).flatten().map(|t| (i, t))
    }
}

/// Whether a branch to `to` lands on `at` or on padding that falls into it.
fn lands(insns: &[Insn], index: &HashMap<i64, usize>, to: i64, at: usize) -> bool {
    index.get(&to).is_some_and(|&t| t <= at && insns[t..at].iter().all(padding))
}

/// The 64-bit sum of vB:vA over every lane of the wave, as the compiler
/// writes it. Returns the register that receives its low word.
fn wave_sum(c: &mut Cursor<'_>, wide: bool, a: u8, b: u8) -> Option<u8> {
    let mut ahead = c.insns[c.at..].iter().filter(|op| !padding(op)).map(|op| &op.dst[..]);
    let (Some(&[Opnd::Vgpr(t)]), Some(&[Opnd::Vgpr(u)])) = (ahead.next(), ahead.next()) else { return None };
    if [a, b, t, u].iter().enumerate().any(|(i, r)| [a, b, t, u][..i].contains(r)) { return None; }
    let (a, b, t, u) = (u32::from(a), u32::from(b), u32::from(t), u32::from(u));
    let add = |c: &mut Cursor<'_>, low: u32, high: u32| {
        // v_add_co_u32 vA, vcc, low, vA; v_add_co_ci_u32 vB, vcc, high, vB, vcc
        c.raw(&[0xd70f_6a00 | a, (256 | a) << 9 | low])?;
        c.raw(&[0x5000_0000 | b << 17 | b << 9 | high])
    };
    // Neighbours, pairs and quads within each octet.
    for select in [0xde54c1, 0x936012, 0x000924] {
        c.raw(&[0x7e00_02e9 | t << 17, select << 8 | a])?;
        c.raw(&[0x7e00_02e9 | u << 17, select << 8 | b])?;
        add(c, 256 | t, 256 | u)?;
    }
    // The mirrored row, then the other row of sixteen.
    c.raw(&[0x7e00_02fa | t << 17, 0xff09_4000 | a])?;
    c.raw(&[0x7e00_02fa | u << 17, 0xff09_4000 | b])?;
    add(c, 256 | t, 256 | u)?;
    c.raw(&[0xd778_0000 | t, 0x0201_0100 | a])?;
    c.raw(&[0xd778_0000 | u, 0x0201_0100 | b])?;
    add(c, 256 | t, 256 | u)?;
    let read = |c: &mut Cursor<'_>, source: u32, lane: u32| {
        let (_, op) = c.next()?;
        scalar_dst(op).filter(|&d| op.raw == [0xd760_0000 | u32::from(d), (128 + lane) << 9 | 256 | source])
    };
    if wide {
        // The other half of a 64-lane wave, from its first lane.
        let low = read(c, a, 32)?;
        let high = read(c, b, 32)?;
        if low == high { return None; }
        add(c, u32::from(low), u32::from(high))?;
    }
    let count = read(c, a, 0)?;
    read(c, b, 0).filter(|&high| high != count)?;
    Some(count)
}

/// The loop starting at `first`, by shape and by the lifetimes its proof needs.
fn key_count_loop(insns: &[Insn], first: usize, index: &HashMap<i64, usize>) -> Option<Loop> {
    use Opnd::{ConstInt as K, ExecLo, Sgpr as S, VccLo, Vgpr as V};
    if padding(&insns[first]) { return None; }
    let mut c = Cursor { insns, at: first };
    let one = |o: &Opnd| *o == K(1);
    let zero = |o: &Opnd| *o == K(0);
    let (_, flag) = c.mov(one)?;
    let head = c.exact("v_cmp_ne_u32", Enc::Vopc, &[VccLo], &[K(0), V(flag)])?;
    c.mov(zero).filter(|&(_, d)| d == flag)?;
    let (save, op) = c.next()?;
    let wide = match op.mnemonic.as_str() {
        "s_and_saveexec_b32" => false,
        "s_and_saveexec_b64" => true,
        _ => return None,
    };
    let w = if wide { "b64" } else { "b32" };
    let outer = sgpr(op.dst.first()).filter(|_| plain(op) && op.src == [VccLo])?;
    let (_, exit) = c.branch(8)?;
    let (read, op) = c.next()?;
    let (key, vkey) = match (&op.dst[..], &op.src[..]) {
        ([S(k @ 0..=105)], [V(v)]) if op.mnemonic == "v_readfirstlane_b32" && op.enc == Enc::Vop1 && plain(op) => (*k, *v),
        _ => return None,
    };
    // A comparison of the key with a constant may limit the lanes that count.
    let filter = c.peek().filter(|op| {
        op.enc == Enc::Vopc && op.mnemonic.starts_with("v_cmp_") && plain(op) && op.dst == [VccLo]
            && matches!(op.src[..], [K(_) | Opnd::Literal(_), V(v)] if v == vkey)
    }).and_then(|_| c.next()).map(|(i, _)| i);
    let (same, op) = c.next()?;
    let matched = sgpr(op.dst.first()).filter(|_| {
        op.mnemonic == "v_cmp_eq_u32" && op.enc == Enc::Vop3 && plain(op) && op.src.get(..2) == Some(&[S(key), V(vkey)])
    })?;
    let (and, op) = c.next()?;
    let valid = match op.src[..] {
        [x, m] | [m, x] if m == S(matched) && x != m => x,
        _ => return None,
    };
    let active = sgpr(op.dst.first()).filter(|_| op.mnemonic == format!("s_and_{w}") && op.enc == Enc::Sop2 && plain(op))?;
    match (filter, valid) {
        (Some(_), VccLo) | (None, S(0..=105)) => {}
        _ => return None,
    }
    let (narrow, op) = c.next()?;
    let pending = sgpr(op.dst.first()).filter(|_| op.mnemonic == format!("s_and_saveexec_{w}") && plain(op) && op.src == [S(active)])?;
    let (_, tail) = c.branch(8)?;
    let (_, ones) = c.mov(one)?;
    let (_, zeros) = c.mov(zero)?;
    let (widen, op) = c.next()?;
    let all = sgpr(op.dst.first()).filter(|_| op.mnemonic == format!("s_or_saveexec_{w}") && plain(op) && op.src == [K(-1)])?;
    let (_, low) = c.mov(zero)?;
    let (_, high) = c.mov(zero)?;
    c.exact("v_cndmask_b32", Enc::Vop3, &[V(low)], &[V(low), V(ones), S(all)])?;
    c.exact("v_cndmask_b32", Enc::Vop3, &[V(high)], &[V(high), V(zeros), S(all)])?;
    if [low, high].contains(&ones) || [low, high].contains(&zeros) || ones == zeros { return None; }
    let sum_at = c.at;
    let count = wave_sum(&mut c, wide, low, high)?;
    let temporaries: Vec<u8> = insns[sum_at..c.at].iter().filter_map(|op| match op.dst[..] {
        [V(n)] if op.mnemonic != "v_readlane_b32" => Some(n),
        _ => None,
    }).collect();
    let restore = c.exact(&format!("s_mov_{w}"), Enc::Sop1, &[ExecLo], &[S(all)])?;
    let (_, op) = c.next()?;
    let lane = match op.dst[..] {
        [V(l)] if op.mnemonic == "v_mbcnt_lo_u32_b32" && plain(op) && op.src.get(..2) == Some(&[K(-1), K(0)]) => l,
        _ => return None,
    };
    // The first EXEC lane, a copy of EXEC nothing reads, and the other half of the lane number.
    let (mut elected, mut upper) = (None, !wide);
    loop {
        let op = c.peek()?;
        let first_lane = op.mnemonic == format!("s_ff1_i32_{w}") && elected.is_none() && plain(op) && op.src == [ExecLo];
        if op.mnemonic == "v_mbcnt_hi_u32_b32" && !upper && plain(op) && op.dst == [V(lane)] && op.src.get(..2) == Some(&[K(-1), V(lane)]) {
            upper = true;
        } else if !first_lane && !(op.mnemonic == format!("s_mov_{w}") && plain(op) && op.src == [ExecLo] && sgpr(op.dst.first()).is_some()) {
            break;
        }
        let (at, _) = c.next()?;
        if first_lane { elected = Some((sgpr(op.dst.first())?, at)); }
    }
    let (elected, election) = elected.filter(|_| upper)?;
    let (elect, op) = c.next()?;
    if !matches!(op.mnemonic.as_str(), "v_cmpx_eq_i32" | "v_cmpx_eq_u32") || op.enc != Enc::Vopc || !plain(op) || op.src != [S(elected), V(lane)] {
        return None;
    }
    let (_, none) = c.branch(8)?;
    let mut moves = [c.next()?, c.next()?];
    if moves[0].1.src == [S(key)] { moves.swap(0, 1); }
    let [(count_at, set_count), (index_at, set_index)] = moves;
    let vector_move = |op: &Insn, source: u8| match op.dst[..] {
        [V(d)] if op.mnemonic == "v_mov_b32" && op.enc == Enc::Vop1 && plain(op) && op.src == [S(source)] => Some(d),
        _ => None,
    };
    let (data, address) = (vector_move(set_count, count)?, vector_move(set_index, key)?);
    let mut waits = Vec::new();
    while c.peek()?.mnemonic == "s_waitcnt" { waits.push(c.next()?.0); }
    let (atomic, op) = c.next()?;
    // IDXEN alone: the key is the element, and the old value is not returned.
    let descriptor = match (&op.dst[..], &op.src[..]) {
        ([V(d)], [V(i), S(desc @ 0..=102), K(0)]) if op.mnemonic == "buffer_atomic_add" && op.enc == Enc::Mubuf
            && op.aux.mask == 2 && op.aux.imm == 0 && *d == data && *i == address && data != address => *desc,
        _ => return None,
    };
    let rejoin = c.exact(&format!("s_mov_{w}"), Enc::Sop1, &[ExecLo], &[S(pending)])?;
    let remain = c.exact("v_cndmask_b32", Enc::Vop3, &[V(flag)], &[K(0), K(1), S(matched)])?;
    c.exact("v_xor_b32", Enc::Vop2, &[V(flag)], &[K(1), V(flag)])?;
    let leave = c.exact(&format!("s_mov_{w}"), Enc::Sop1, &[ExecLo], &[S(outer)])?;
    c.exact("v_cmp_ne_u32", Enc::Vopc, &[VccLo], &[K(0), V(flag)])?;
    let (_, op) = c.next()?;
    if op.mnemonic != format!("s_bcnt1_i32_{w}") || !plain(op) || op.src != [VccLo] || sgpr(op.dst.first()).is_none() { return None; }
    let (last, again) = c.branch(5)?;
    if !lands(insns, index, exit, leave) || !lands(insns, index, tail, rejoin) || !lands(insns, index, none, rejoin)
        || !lands(insns, index, again, head)
    {
        return None;
    }
    // The flag is rewritten for every pending lane only if the whole-wave sum leaves it alone.
    let vectors: Vec<u8> = [flag, ones, zeros, low, high, lane, data, address].into_iter().chain(temporaries.iter().copied()).collect();
    if temporaries.contains(&flag) || [low, high].contains(&flag) || vectors.contains(&vkey) { return None; }
    let pair = |r: u8| if wide { vec![r, r + 1] } else { vec![r] };
    let written = |range: std::ops::Range<usize>, regs: &[u8]| insns[range].iter().any(|op| regs.iter().any(|&r| writes(op, r, wide)));
    // Each value must reach its last reader, on every pass.
    if written(save + 1..leave, &pair(outer)) || written(narrow + 1..rejoin, &pair(pending))
        || written(same + 1..remain, &pair(matched)) || written(read + 1..atomic, &[key])
        || written(widen + 1..restore, &pair(all)) || written(restore..atomic, &[count])
        || written(election + 1..elect, &[elected])
        || written(first..last + 1, &[descriptor, descriptor + 1, descriptor + 2, descriptor + 3])
        || matches!(valid, S(v) if written(first..last + 1, &pair(v)))
        || wide && [outer, pending, matched, active, all].iter().chain(sgpr(Some(&valid)).iter()).any(|&r| r == 105)
    {
        return None;
    }
    let mut scalars = 0b11u128 << VCC | 1 << SCC;
    for op in &insns[first..=last] {
        for r in 0..106 {
            if writes(op, r, wide) { scalars |= 1 << r; }
        }
    }
    let mut keep: Vec<usize> = insns[first..=save].iter().enumerate().map(|(i, _)| first + i).collect();
    keep.extend(filter);
    keep.extend([and, narrow, count_at, index_at, atomic, rejoin, leave]);
    keep.extend(waits);
    Some(Loop { first, last, keep, and, count: count_at, index: index_at, atomic, key: vkey, valid, descriptor, wide, vectors, scalars })
}

/// What an instruction reads, generously: descriptors and wide operands span
/// several registers, and stores name their data as a destination.
fn reads(op: &Insn) -> (u128, Vec<u8>) {
    let m = op.mnemonic.as_str();
    let vop = matches!(op.enc, Enc::Vop1 | Enc::Vop2 | Enc::Vop3 | Enc::Vopc);
    let scalar = matches!(op.enc, Enc::Sop1 | Enc::Sop2 | Enc::Sopk | Enc::Sopc | Enc::Sopp | Enc::Smem);
    let span = match op.enc {
        Enc::Mimg => 8,
        Enc::Smem | Enc::Mubuf | Enc::Mtbuf => 4,
        // A lane mask is a pair in a 64-lane wave.
        _ if vop || m.contains("64") => 2,
        _ => 1,
    };
    // Further registers an address or a value can occupy: (as a source, as stored data).
    let (address, stored) = match op.enc {
        _ if vop => (u8::from(m.contains("64")), 0),
        Enc::Mubuf | Enc::Mtbuf => (1, 3),
        Enc::Ds => (1, 1),
        _ => (7, 7),
    };
    let fills = m.contains("load") || m.contains("sample") || m.contains("gather") || m.contains("resinfo") || m.contains("bvh") || m.starts_with("ds_read");
    let data = op.dst.iter().filter(|_| !vop && !scalar && !fills).map(|d| (d, stored));
    let mut scalars = 0u128;
    let mut vectors = Vec::new();
    for (operand, further) in active_srcs(op).iter().map(|s| (s, address)).chain(data) {
        match *operand {
            Opnd::Sgpr(n @ 0..=105) => (n..=105.min(n + span - 1)).for_each(|r| scalars |= 1 << r),
            Opnd::VccLo | Opnd::VccHi => scalars |= 0b11 << VCC,
            Opnd::Scc => scalars |= 1 << SCC,
            Opnd::Vgpr(n) => vectors.extend(n..=n.saturating_add(further)),
            // A DPP source names its register in the control word.
            Opnd::Raw(_) => vectors.extend(op.raw.get(1).map(|w| *w as u8)),
            _ => {}
        }
    }
    // Read-modify-write and conditional forms read their destination.
    if op.enc == Enc::Sopk && m != "s_movk_i32" || m.starts_with("s_cmov") || m.starts_with("s_bitset") {
        op.dst.iter().filter_map(|d| sgpr(Some(d))).for_each(|r| scalars |= 0b11 << r);
    }
    if op.enc == Enc::Smem {
        if let Some(r) = op.raw.get(1).map(|w| w >> 25).filter(|r| *r <= 105) { scalars |= 1 << r; }
    }
    if op.enc == Enc::Sopp && matches!(op.op, 4 | 5) || ["s_cselect", "s_cmov", "s_addc", "s_subb"].iter().any(|p| m.starts_with(p)) {
        scalars |= 1 << SCC;
    }
    // VOP2 selects and carries read VCC without naming it.
    if op.enc == Enc::Sopp && matches!(op.op, 6 | 7)
        || op.enc == Enc::Vop2 && (m.starts_with("v_cndmask") || m.contains("_co_ci") || m.contains("_subb") || m.contains("_addc"))
    {
        scalars |= 0b11 << VCC;
    }
    (scalars, vectors)
}

/// Whether a scalar instruction sets SCC from its operands alone.
fn sets_scc(op: &Insn) -> bool {
    let m = op.mnemonic.as_str();
    match op.enc {
        Enc::Sopc => true,
        Enc::Sopk => m.starts_with("s_cmpk_"),
        Enc::Sop2 => ["s_add_", "s_sub_", "s_and_", "s_or_", "s_xor_", "s_lshl_", "s_lshr_", "s_min_", "s_max_"].iter().any(|p| m.starts_with(p)),
        Enc::Sop1 => m.starts_with("s_bcnt") || m.starts_with("s_not_") || m.contains("saveexec"),
        _ => false,
    }
}

/// Whether every register the console loop leaves different is written again
/// before anything reads it, on every path from the loop's end.
///
/// A vector write reaches only the lanes EXEC enables, so it clears a register
/// just until EXEC can widen or control flow joins. Another accepted loop
/// counts in its rewritten form: that reads its key, mask and descriptor, and
/// the registers it leaves different are checked from its own end.
fn dead_after(insns: &[Insn], index: &HashMap<i64, usize>, l: &Loop, accepted: &[Loop]) -> bool {
    #[derive(Clone, PartialEq)]
    struct State {
        scalars: u128,
        /// Vectors another loop answers for.
        passed: BTreeSet<u8>,
        /// Vectors rewritten under the EXEC of this straight run.
        fresh: Vec<u8>,
    }
    let joins: BTreeSet<usize> = insns.iter().filter_map(|op| target(op).and_then(|t| index.get(&t).copied())).collect();
    let mut seen: HashMap<usize, State> = HashMap::new();
    let mut work = vec![(l.last + 1, State { scalars: l.scalars, passed: BTreeSet::new(), fresh: Vec::new() })];
    while let Some((mut at, mut state)) = work.pop() {
        while let Some(op) = insns.get(at) {
            if joins.contains(&at) || at == l.last + 1 {
                state.fresh.clear();
                if let Some(old) = seen.get(&at) {
                    let merged = State {
                        scalars: old.scalars | state.scalars,
                        passed: old.passed.intersection(&state.passed).copied().collect(),
                        fresh: Vec::new(),
                    };
                    if merged == *old { break; }
                    state = merged;
                }
                seen.insert(at, state.clone());
            }
            let dirty = |state: &State, v: u8| l.vectors.contains(&v) && !state.passed.contains(&v) && !state.fresh.contains(&v);
            // Back at its own head, the loop leaves what this scan started with.
            if at == l.first { break; }
            if let Some(other) = accepted.iter().find(|o| o.first == at) {
                let mask = match other.valid {
                    Opnd::Sgpr(v) => (if other.wide { 0b11u128 } else { 1 }) << v,
                    _ => 0,
                };
                if state.scalars & (mask | 0b1111 << other.descriptor) != 0 || dirty(&state, other.key) { return false; }
                state.scalars &= !other.scalars;
                state.passed.extend(other.vectors.iter().copied());
                state.fresh.clear();
                at = other.last + 1;
                continue;
            }
            let m = op.mnemonic.as_str();
            if m == "v_nop" || padding(op) || m.starts_with("s_waitcnt") {
                at += 1;
                continue;
            }
            let (scalars, vectors) = reads(op);
            if scalars & state.scalars != 0 || vectors.iter().any(|&v| dirty(&state, v)) { return false; }
            if m == "s_endpgm" { break; }
            let vop = matches!(op.enc, Enc::Vop1 | Enc::Vop2 | Enc::Vop3 | Enc::Vopc);
            // EXEC only narrows through these two forms.
            let narrows = m.starts_with("s_and_saveexec_") || m.starts_with("v_cmpx_");
            let exec = op.dst.iter().any(|d| matches!(d, Opnd::ExecLo | Opnd::ExecHi)) || m.contains("exec") || m.starts_with("v_cmpx_");
            if exec && !narrows { state.fresh.clear(); }
            for r in 0..106 {
                if writes(op, r, l.wide) { state.scalars &= !(1 << r); }
            }
            // A CMPX form names VCC but writes only EXEC.
            if op.dst.contains(&Opnd::VccLo) && !m.starts_with("v_cmpx_") { state.scalars &= !(0b11 << VCC); }
            if sets_scc(op) { state.scalars &= !(1 << SCC); }
            if let [Opnd::Vgpr(d)] = op.dst[..] {
                let whole = vop && plain(op) && !m.contains("64") && !m.contains("lane")
                    && !op.src.iter().any(|s| matches!(s, Opnd::Raw(_)));
                if whole && !state.fresh.contains(&d) { state.fresh.push(d); }
            }
            if op.enc == Enc::Sopp {
                if let Some(to) = target(op) {
                    let Some(&to) = index.get(&to) else { return false };
                    work.push((to, State { fresh: Vec::new(), ..state.clone() }));
                    if op.op == 2 { break; }
                    state.fresh.clear();
                } else if !matches!(m, "s_nop" | "s_inst_prefetch" | "s_clause" | "s_code_end") {
                    return false;
                }
            }
            at += 1;
        }
    }
    true
}

#[cfg(test)]
#[path = "../../tests/hlsl/key_counts.rs"]
mod tests;
