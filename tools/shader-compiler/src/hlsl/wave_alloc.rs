//! Wave-size-independent forms of the 64-lane integer sum and exclusive prefix
//! sum that compilers emit around an elected atomic append. The exact DPP
//! emulation in `wave_sum` needs host waves of 64 lanes, which only compute
//! shaders can request. When the row partials, cross-row values and half-wave
//! scalars die unread, the sequence is only a whole-wave total or an exclusive
//! prefix of values that are zero in inactive lanes. `WaveActiveSum` and
//! `WavePrefixSum` compute exactly that on every host wave width.
use super::{Emitter, Enc, Insn, Opnd, Stage, wave_sum};
use std::collections::BTreeSet;

pub(super) const TOTAL: &str = "tb_wave_total_u32";
pub(super) const PREFIX: &str = "tb_wave_exclusive_prefix_u32";
// Opcode for the rewritten instructions, so passes matching READLANE or VOP2
// opcodes ignore them. Deliberately distinct from `wave_sum::DPP_SCAN` (0x7ff3)
// and `DPP_REDUCE` (0x7ff1): a rewritten prefix must not be mistaken for a
// scan region and validated as one, which the whole-wave prefix does not need
// and would reject.
pub(super) const PSEUDO_OP: u32 = 0x7ff4;

#[derive(Clone, Copy, PartialEq)]
enum Reg { V(u8), S(u8) }

fn memory_width(m: &str) -> Option<u16> {
    let wide = |m: &str| if m.contains("x16") { 16 } else if m.contains("x8") { 8 } else if m.contains("x4") || m.contains("xyzw") { 4 }
        else if m.contains("x3") || m.contains("xyz") { 3 } else if m.contains("x2") || m.contains("xy") { 2 } else { 1 };
    ["buffer_", "tbuffer_", "image_", "global_", "ds_", "exp", "s_load", "s_buffer_load"].iter()
        .any(|p| m.starts_with(p)).then(|| wide(m).max(4))
}

fn covers(o: &Opnd, r: Reg, width: u16) -> bool {
    let (k, n) = match (*o, r) {
        (Opnd::Vgpr(k), Reg::V(n)) | (Opnd::Sgpr(k), Reg::S(n)) => (k, n),
        _ => return false,
    };
    u16::from(k) <= u16::from(n) && u16::from(n) < u16::from(k) + width
}

/// Whether `ins` reads `r`. `refined` takes each operand at its own width, as the
/// 64-lane scan needs. The other sequences keep the coarser reading they had.
fn reads(ins: &Insn, r: Reg, refined: bool) -> bool {
    let m = ins.mnemonic.as_str();
    if let Some(width) = memory_width(m) {
        // Store and atomic data are decoded as destinations. MUBUF VADDR is
        // ignored when neither OFFEN nor IDXEN is set.
        let unused_vaddr = ins.enc == Enc::Mubuf && ins.aux.mask & 3 == 0;
        let data = !m.starts_with("s_") && !m.contains("_load");
        // The address and data of a 32-bit LDS access are one register each.
        let width = if refined && ins.enc == Enc::Ds && m.ends_with("32") { 1 } else { width };
        // MUBUF VADDR holds an index, an offset, or one after the other.
        let address = if refined && ins.enc == Enc::Mubuf { (ins.aux.mask & 3).count_ones() as u16 } else { width };
        return ins.src.iter().enumerate().any(|(i, o)| !(unused_vaddr && i == 0) && covers(o, r, if i == 0 { address } else { width }))
            || (data || m.contains("atomic")) && ins.dst.iter().any(|o| covers(o, r, width));
    }
    match r {
        // Only a select or a carry reads its third source as the two words of a mask.
        Reg::S(n) if refined && m.starts_with("v_") && !m.contains("64") && !m.starts_with("v_cndmask") && !m.contains("_ci_")
            && !m.starts_with("v_div_fmas") => ins.src.contains(&Opnd::Sgpr(n)),
        Reg::S(n) => super::reads_sgpr(ins, n),
        Reg::V(_) => {
            let width = if m.contains("64") && m.starts_with("v_") { 2 } else { 1 };
            ins.src.iter().any(|o| covers(o, r, width))
        }
    }
}

/// A write that defines `r` in every lane that can later read it.
fn kills(ins: &Insn, r: Reg, whole_wave: bool) -> bool {
    let m = ins.mnemonic.as_str();
    match r {
        Reg::S(n) => {
            if matches!(m, "v_readlane_b32" | "v_readfirstlane_b32") {
                return matches!(ins.dst.first(), Some(Opnd::Vgpr(k) | Opnd::Sgpr(k)) if *k == n);
            }
            let width = if m.starts_with("s_load") || m.starts_with("s_buffer_load") { memory_width(m).unwrap_or(1) }
                else if m.starts_with("s_") && m.ends_with("64") { 2 } else { 1 };
            (m.starts_with("s_") || m.starts_with("v_cmp")) && ins.dst.iter().any(|o| covers(o, r, width))
        }
        // Inactive lanes keep the old value; only whole-wave vector writes kill.
        Reg::V(_) => whole_wave && m.starts_with("v_") && !m.starts_with("v_cmp") && memory_width(m).is_none()
            && !matches!(m, "v_readlane_b32" | "v_readfirstlane_b32") && ins.dst == [match r { Reg::V(n) => Opnd::Vgpr(n), Reg::S(n) => Opnd::Sgpr(n) }],
    }
}

/// Every path from `start` overwrites `r` before reading it.
fn dead_from(insns: &[Insn], start: usize, r: Reg, refined: bool) -> bool {
    let index_of = |offset: i64| insns.iter().position(|i| i.offset as i64 == offset);
    // `fresh`: a vector write under the EXEC still in force defined `r` in every
    // lane that an instruction under that EXEC reads, as when a scan in a loop
    // comes round to its own cross-row temporary. Lane reads, permutes and DPP
    // reach other lanes.
    let mut stack = vec![(start, false, false)];
    let mut seen = BTreeSet::new();
    while let Some((mut at, mut whole, mut fresh)) = stack.pop() {
        loop {
            if !seen.insert((at, whole, fresh)) { break; }
            let Some(ins) = insns.get(at) else { break };
            let own_lane = matches!(ins.enc, Enc::Vop1 | Enc::Vop2 | Enc::Vop3 | Enc::Vopc)
                && !ins.mnemonic.contains("lane") && !matches!(ins.src.first(), Some(Opnd::Raw(_)));
            if !(fresh && own_lane) && reads(ins, r, refined) {
                if std::env::var_os("TB_DFDEBUG").is_some() { eprintln!("wave_alloc: live at {:#x} {}", ins.offset, ins.mnemonic); }
                return false;
            }
            if kills(ins, r, whole) { break; }
            fresh |= refined && matches!(r, Reg::V(_)) && kills(ins, r, true);
            let m = ins.mnemonic.as_str();
            if ins.dst.contains(&Opnd::ExecLo) || ins.dst.contains(&Opnd::ExecHi) || m.contains("saveexec") || m.contains("wrexec")
                || m.starts_with("v_cmpx")
            {
                whole = m == "s_or_saveexec_b64" && ins.src == [Opnd::ConstInt(-1)]
                    || m == "s_mov_b64" && ins.dst == [Opnd::ExecLo] && ins.src == [Opnd::ConstInt(-1)];
                fresh = false;
            }
            if matches!(m, "s_setpc_b64" | "s_swappc_b64") { return false; }
            if m == "s_endpgm" { break; }
            if ins.enc == Enc::Sopp {
                let target = ins.offset as i64 + 4 + 4 * ins.aux.imm;
                if ins.op == 2 {
                    let Some(t) = index_of(target) else { return false };
                    at = t;
                    continue;
                }
                if Emitter::branch_cond(ins.op).is_some() {
                    let Some(t) = index_of(target) else { return false };
                    stack.push((t, whole, fresh));
                }
            }
            at += 1;
        }
    }
    true
}

fn dpp(ins: &Insn, acc: u8, selector: u16, ctrl: u32) -> bool {
    ins.mnemonic == "v_add_nc_u32" && ins.dst == [Opnd::Vgpr(acc)]
        && ins.src == [Opnd::Raw(selector), Opnd::Vgpr(acc)] && ins.raw.get(1) == Some(&(ctrl << 8 | u32::from(acc)))
}

fn readlane(ins: &Insn, acc: u8, lane: i64) -> Option<u8> {
    match (ins.mnemonic.as_str(), ins.dst.first(), ins.src.get(..2)) {
        ("v_readlane_b32", Some(&Opnd::Vgpr(d)), Some([src, Opnd::ConstInt(l)])) if *src == Opnd::Vgpr(acc) && *l == lane => Some(d),
        _ => None,
    }
}

fn is(ins: &Insn, m: &str, dst: &[Opnd], src: &[Opnd]) -> bool {
    ins.mnemonic == m && ins.dst == dst && ins.src.get(..src.len()) == Some(src)
}

/// Find the cndmask that follows the saveexec at index `a`, skipping over an
/// optional `s_waitcnt` and an optional `s_and_b32` (which intersects the saved
/// EXEC with another scalar mask). Returns the cndmask index and the mask register
/// the cndmask must use.
fn cndmask_at(insns: &[Insn], a: usize, saved: u8) -> Option<(usize, Opnd)> {
    let mut c = a + 1;
    // Skip an optional s_waitcnt between the saveexec and the cndmask.
    if let Some(ins) = insns.get(c) {
        if ins.mnemonic == "s_waitcnt" {
            c += 1;
        }
    }
    // Optional s_and_b32 that intersects the saved EXEC with another scalar mask.
    let mut mask = Opnd::Sgpr(saved);
    if let Some(and) = insns.get(c) {
        if and.mnemonic == "s_and_b32" && and.dst.len() == 1 && and.src.len() == 2
            && and.src.iter().any(|s| *s == Opnd::Sgpr(saved))
        {
            let Opnd::Sgpr(m) = and.dst[0] else { return None };
            mask = Opnd::Sgpr(m);
            c += 1;
        }
    }
    let Some(cnd) = insns.get(c) else { return None };
    if cnd.mnemonic != "v_cndmask_b32" {
        return None;
    }
    Some((c, mask))
}

fn nop(ins: &mut Insn) {
    ins.mnemonic = "s_nop".into();
    ins.enc = Enc::Sopp;
    ins.op = 0;
    ins.dst.clear();
    ins.src.clear();
}

// DPP8 pairs, quads and octets, then DPP16 row_mirror (see `wave_sum`).
fn total(insns: &mut [Insn], a: usize) -> Option<()> {
    let saved = match insns.get(a) {
        Some(ins) if ins.mnemonic == "s_or_saveexec_b64" && ins.dst.len() == 1 && ins.src == [Opnd::ConstInt(-1)] => {
            let Opnd::Sgpr(s) = ins.dst[0] else { return None };
            s
        }
        _ => return None,
    };
    let (c, mask) = cndmask_at(insns, a, saved)?;
    if insns.len() < c + 11 {
        return None;
    }
    let cnd = &insns[c];
    let Opnd::Vgpr(acc) = *cnd.dst.first()? else { return None };
    let Opnd::Vgpr(tmp) = *insns[c + 5].dst.first()? else { return None };
    let (v, s) = (Opnd::Vgpr(acc), Opnd::Sgpr(saved));
    if cnd.src.len() != 3 || cnd.src[0] != Opnd::ConstInt(0) || cnd.src[2] != mask
        || ![0xde54c1, 0x936012, 0x000924, 0xff0940].iter().enumerate().all(|(i, &ctl)| dpp(&insns[c + 1 + i], acc, if i == 3 { 250 } else { 233 }, ctl))
        || !is(&insns[c + 5], "v_permlanex16_b32", &[Opnd::Vgpr(tmp)], &[v, Opnd::ConstInt(0), Opnd::ConstInt(0)])
        || !is(&insns[c + 6], "v_add_nc_u32", &[v], &[Opnd::Vgpr(tmp), v]) || insns[c + 5].raw[0] >> 11 & 0xf != 0 {
        return None;
    }
    let high = readlane(&insns[c + 7], acc, 32)?;
    let sum = readlane(&insns[c + 9], acc, 0)?;
    if !is(&insns[c + 8], "v_add_nc_u32", &[v], &[Opnd::Sgpr(high), v]) || !is(&insns[c + 10], "s_mov_b64", &[Opnd::ExecLo], &[s]) {
        return None;
    }
    // The permlanex temporary, the cross-half readlane result and the accumulator
    // are scratch: nopping the pattern's writes to them is only safe when every
    // later instruction overwrites them before reading them, so prove they are
    // dead before the rewrite.
    let end = c + 11;
    if !(dead_from(insns, end, Reg::V(acc), false) && dead_from(insns, end, Reg::V(tmp), false)
        && (high == sum || dead_from(insns, end, Reg::S(high), false))) {
        return None;
    }
    insns[c + 1..c + 9].iter_mut().for_each(nop);
    let r = &mut insns[c + 9];
    r.mnemonic = TOTAL.into();
    (r.enc, r.op) = (Enc::Sop1, PSEUDO_OP);
    r.dst = vec![Opnd::Sgpr(sum)];
    r.src = vec![v];
    Some(())
}

// The 32-lane whole-wave total: the same DPP row partials and permlanex combine as
// the 64-lane form, but a 32-lane wave has a single half, so there is no cross-half
// readlane/carry; the sum is read from lane 0 and the exec is restored with a
// 32-bit move.
fn total32(insns: &mut [Insn], a: usize) -> Option<()> {
    let saved = match insns.get(a) {
        Some(ins) if ins.mnemonic == "s_or_saveexec_b32" && ins.dst.len() == 1 && ins.src == [Opnd::ConstInt(-1)] => {
            let Opnd::Sgpr(s) = ins.dst[0] else { return None };
            s
        }
        _ => return None,
    };
    let (c, mask) = cndmask_at(insns, a, saved)?;
    if insns.len() < c + 9 {
        return None;
    }
    let cnd = &insns[c];
    let Opnd::Vgpr(acc) = *cnd.dst.first()? else { return None };
    let Opnd::Vgpr(tmp) = *insns[c + 5].dst.first()? else { return None };
    let (v, s) = (Opnd::Vgpr(acc), Opnd::Sgpr(saved));
    if cnd.src.len() != 3 || cnd.src[0] != Opnd::ConstInt(0) || cnd.src[2] != mask
        || ![0xde54c1, 0x936012, 0x000924, 0xff0940].iter().enumerate().all(|(i, &ctl)| dpp(&insns[c + 1 + i], acc, if i == 3 { 250 } else { 233 }, ctl))
        || !is(&insns[c + 5], "v_permlanex16_b32", &[Opnd::Vgpr(tmp)], &[v, Opnd::ConstInt(0), Opnd::ConstInt(0)])
        || !is(&insns[c + 6], "v_add_nc_u32", &[v], &[Opnd::Vgpr(tmp), v]) || insns[c + 5].raw[0] >> 11 & 0xf != 0 {
        return None;
    }
    let sum = readlane(&insns[c + 7], acc, 0)?;
    if !is(&insns[c + 8], "s_mov_b32", &[Opnd::ExecLo], &[s]) {
        return None;
    }
    // The permlanex temporary and the accumulator are scratch: nopping the
    // pattern's writes to them is only safe when every later instruction
    // overwrites them before reading them, so prove they are dead before the
    // rewrite.
    let end = c + 9;
    if !(dead_from(insns, end, Reg::V(acc), false) && dead_from(insns, end, Reg::V(tmp), false)) {
        return None;
    }
    insns[c + 1..c + 7].iter_mut().for_each(nop);
    let r = &mut insns[c + 7];
    r.mnemonic = TOTAL.into();
    (r.enc, r.op) = (Enc::Sop1, PSEUDO_OP);
    r.dst = vec![Opnd::Sgpr(sum)];
    r.src = vec![v];
    Some(())
}

// Inclusive DPP16 row_shr scan, cross-row and cross-half carries, minus the input.
fn prefix(insns: &mut [Insn], a: usize) -> Option<()> {
    let saved = match insns.get(a) {
        Some(ins) if ins.mnemonic == "s_or_saveexec_b64" && ins.dst.len() == 1 && ins.src == [Opnd::ConstInt(-1)] => {
            let Opnd::Sgpr(s) = ins.dst[0] else { return None };
            s
        }
        _ => return None,
    };
    let (c, mask) = cndmask_at(insns, a, saved)?;
    if insns.len() < c + 18 {
        return None;
    }
    let cnd = &insns[c];
    let Opnd::Vgpr(x) = *cnd.dst.first()? else { return None };
    let Opnd::Vgpr(acc) = *insns[c + 1].dst.first()? else { return None };
    let Opnd::Vgpr(tmp) = *insns[c + 8].dst.first()? else { return None };
    let (v, s, ones) = (Opnd::Vgpr(acc), Opnd::Sgpr(saved), Opnd::ConstInt(-1));
    let upper_rows = Opnd::Literal(0xffff0000);
    if cnd.src.len() != 3 || cnd.src[0] != Opnd::ConstInt(0) || cnd.src[2] != mask
        || !is(&insns[c + 1], "v_mov_b32", &[v], &[Opnd::Vgpr(x)]) || acc == x
        || ![0xff0911, 0xff0112, 0xff0114, 0xff0118].iter().enumerate().all(|(i, &ctl)| dpp(&insns[c + 2 + i], acc, 250, ctl))
        || !is(&insns[c + 6], "s_mov_b32", &[Opnd::ExecLo], &[upper_rows]) || !is(&insns[c + 7], "s_mov_b32", &[Opnd::ExecHi], &[upper_rows])
        || !is(&insns[c + 8], "v_permlanex16_b32", &[Opnd::Vgpr(tmp)], &[v, ones, ones])
        || !is(&insns[c + 9], "v_add_nc_u32", &[v], &[Opnd::Vgpr(tmp), v])
        || !is(&insns[c + 11], "s_mov_b32", &[Opnd::ExecLo], &[Opnd::ConstInt(0)]) || !is(&insns[c + 12], "s_mov_b32", &[Opnd::ExecHi], &[ones])
        || !is(&insns[c + 14], "s_mov_b32", &[Opnd::ExecLo], &[ones]) || !is(&insns[c + 15], "s_mov_b32", &[Opnd::ExecHi], &[ones])
        || !is(&insns[c + 16], "v_sub_nc_u32", &[v], &[v, Opnd::Vgpr(x)]) || !is(&insns[c + 17], "s_mov_b64", &[Opnd::ExecLo], &[s]) {
        return None;
    }
    let half = readlane(&insns[c + 10], acc, 31)?;
    if !is(&insns[c + 13], "v_add_nc_u32", &[v], &[Opnd::Sgpr(half), v]) {
        return None;
    }
    // The permlanex temporary and the cross-half readlane result are scratch:
    // nopping the pattern's writes to them is only safe when every later
    // instruction overwrites them before reading them, so prove they are dead
    // before the rewrite. The accumulator holds the prefix in both the original
    // and the lowered form, so it needs no liveness check.
    let end = c + 18;
    if !(dead_from(insns, end, Reg::V(tmp), true) && dead_from(insns, end, Reg::S(half), true)) {
        return None;
    }
    insns[c + 1..c + 16].iter_mut().for_each(nop);
    let r = &mut insns[c + 16];
    r.mnemonic = PREFIX.into();
    (r.enc, r.op) = (Enc::Sop1, PSEUDO_OP);
    r.src = vec![Opnd::Vgpr(x)];
    Some(())
}

// The 32-lane exclusive prefix: an inclusive DPP16 row_shr scan and cross-row
// carry, minus the input. Unlike the 64-lane form, a 32-lane wave has a single
// half, so there is no cross-half readlane/carry and the result is per-lane:
// the whole accumulator holds the exclusive prefix, so no lane read is needed.
// The compiler may intersect the saved EXEC with another scalar mask
// (s_and_b32) before the cndmask; that adds one instruction.
fn prefix32(insns: &mut [Insn], a: usize) -> Option<()> {
    let saved = match insns.get(a) {
        Some(ins) if ins.mnemonic == "s_or_saveexec_b32" && ins.dst.len() == 1 && ins.src == [Opnd::ConstInt(-1)] => {
            let Opnd::Sgpr(s) = ins.dst[0] else { return None };
            s
        }
        _ => return None,
    };
    // The cndmask follows the saveexec, possibly after an s_waitcnt and/or an
    // s_and_b32 that intersects the saved EXEC with another scalar mask.
    let (c, mask) = cndmask_at(insns, a, saved)?;
    if insns.len() < c + 13 {
        return None;
    }
    let cnd = &insns[c];
    let Opnd::Vgpr(x) = *cnd.dst.first()? else { return None };
    if cnd.src.len() != 3 || cnd.src[0] != Opnd::ConstInt(0) || cnd.src[2] != mask
        || !matches!(cnd.src[1], Opnd::Vgpr(_)) {
        return None;
    }
    let Opnd::Vgpr(acc) = *insns[c + 1].dst.first()? else { return None };
    let Opnd::Vgpr(tmp) = *insns[c + 7].dst.first()? else { return None };
    let (v, ones) = (Opnd::Vgpr(acc), Opnd::ConstInt(-1));
    // The result is stored by a plain move of the accumulator; the destination may be
    // the original data register or a separate register the caller uses later.
    if !is(&insns[c + 1], "v_mov_b32", &[v], &[Opnd::Vgpr(x)]) || acc == x
        || ![0xff0911, 0xff0112, 0xff0114, 0xff0118].iter().enumerate().all(|(i, &ctl)| dpp(&insns[c + 2 + i], acc, 250, ctl))
        || !is(&insns[c + 6], "s_mov_b32", &[Opnd::ExecLo], &[Opnd::Literal(0xffff0000)])
        || !is(&insns[c + 7], "v_permlanex16_b32", &[Opnd::Vgpr(tmp)], &[v, ones, ones])
        || !is(&insns[c + 8], "v_add_nc_u32", &[v], &[Opnd::Vgpr(tmp), v])
        || !is(&insns[c + 9], "s_mov_b32", &[Opnd::ExecLo], &[Opnd::ConstInt(-1)])
        || !is(&insns[c + 10], "v_sub_nc_u32", &[v], &[v, Opnd::Vgpr(x)])
        || !is(&insns[c + 11], "s_mov_b32", &[Opnd::ExecLo], &[Opnd::Sgpr(saved)])
        || !(insns[c + 12].mnemonic == "v_mov_b32"
            && matches!(insns[c + 12].dst.first(), Some(Opnd::Vgpr(_)))
            && insns[c + 12].src == [v]) {
        return None;
    }
    // The accumulator holds the exclusive prefix; the final move copies it to the
    // original data register. Replace the sub with the pseudo-op and nop the body.
    // The permlanex temporary is scratch: nopping the pattern's write to it is only
    // safe when every later instruction overwrites it before reading it, so prove it
    // is dead before the rewrite.
    let end = c + 13;
    if !dead_from(insns, end, Reg::V(tmp), false) {
        return None;
    }
    insns[c + 1..c + 10].iter_mut().for_each(nop);
    let r = &mut insns[c + 10];
    r.mnemonic = PREFIX.into();
    (r.enc, r.op) = (Enc::Sop1, PSEUDO_OP);
    r.src = vec![Opnd::Vgpr(x)];
    Some(())
}

/// Lower a whole-wave total that `wave_sum` lowered to `DPP_REDUCE` into a
/// `WaveActiveSum`. The reduction region spans `start` (save-EXEC) through
/// `end` (EXEC restore); the lane-0 read is at `end - 1`. Only integer totals
/// (`v_add_nc_u32`) are lowered; extrema keep their per-lane lowering.
fn total_to_wave_active_sum(insns: &mut [Insn], start: usize, end: usize, width: u32) -> Option<()> {
    let saved = match insns.get(start) {
        Some(ins) if ins.mnemonic == (if width == 32 { "s_or_saveexec_b32" } else { "s_or_saveexec_b64" })
            && ins.dst.len() == 1 && ins.src == [Opnd::ConstInt(-1)] =>
        {
            let Opnd::Sgpr(s) = ins.dst[0] else { return None };
            s
        }
        _ => return None,
    };
    let (c, _mask) = cndmask_at(insns, start, saved)?;
    let first = c + 1;
    let dpp = insns.get(first)?;
    if dpp.op != wave_sum::DPP_REDUCE || dpp.aux.imm != 0 || dpp.mnemonic != "v_add_nc_u32" {
        return None;
    }
    let cnd = &insns[c];
    let Opnd::Vgpr(acc) = *cnd.dst.first()? else { return None };
    let read = insns.get(end - 1)?;
    if read.op != wave_sum::READ_LANE {
        return None;
    }
    let Opnd::Sgpr(sum) = read.dst[0] else { return None };
    // The permlanex temporary and, for the 64-lane form, the cross-half readlane
    // result are scratch: nopping the body is only safe when every later
    // instruction overwrites them before reading them, so prove they are dead
    // before the rewrite. The accumulator keeps the per-lane values selected by
    // the cndmask, which is exactly what WaveActiveSum consumes.
    let tmp = match insns.get(c + 5) {
        Some(ins) if ins.mnemonic == "v_permlanex16_b32" => {
            let Opnd::Vgpr(t) = ins.dst[0] else { return None };
            t
        }
        _ => return None,
    };
    let high = if width == 64 {
        let ins = insns.get(c + 7)?;
        if ins.op != wave_sum::READ_LANE || ins.mnemonic != "v_readlane_b32" {
            return None;
        }
        let Opnd::Sgpr(h) = ins.dst[0] else { return None };
        Some(h)
    } else {
        None
    };
    let after = end + 1;
    if !dead_from(insns, after, Reg::V(acc), false) || !dead_from(insns, after, Reg::V(tmp), false)
        || (high.is_some() && high.unwrap() != sum && !dead_from(insns, after, Reg::S(high.unwrap()), false)) {
        return None;
    }
    // Nop the body (c+1 .. end-1) so the accumulator keeps the per-lane values
    // selected by the cndmask, and replace the lane-0 read with the total.
    insns[c + 1..end - 1].iter_mut().for_each(nop);
    let r = &mut insns[end - 1];
    r.mnemonic = TOTAL.into();
    (r.enc, r.op) = (Enc::Sop1, PSEUDO_OP);
    r.dst = vec![Opnd::Sgpr(sum)];
    r.src = vec![Opnd::Vgpr(acc)];
    Some(())
}

/// Lower every whole-wave total that `wave_sum` would reject on control flow
/// (an early exit before it, or a divergent branch crossing it) to
/// `WaveActiveSum`, which is safe in divergent control flow. Clean totals are
/// left for `wave_sum`, which lowers them per-lane. Returns an error when a
/// non-total reduction is rejected, since there is no whole-wave form for extrema.
pub(super) fn lower_problematic_totals(insns: &mut [Insn], width: u32) -> Result<(), String> {
    let mut verified_reads = Vec::new();
    let regions: Vec<(usize, std::ops::RangeInclusive<usize>)> =
        wave_sum::reduction_regions(insns, width).collect();
    for (first, region) in regions {
        let start = insns[*region.start()].offset as i64;
        let end = insns[*region.end()].offset as i64;
        let last_read = *region.end() - 1;
        if wave_sum::check_region(insns, first, start, end, &verified_reads).is_err() {
            if insns[first].mnemonic != "v_add_nc_u32" {
                return Err("wave reduction in divergent control flow".into());
            }
            total_to_wave_active_sum(insns, *region.start(), *region.end(), width);
            continue;
        }
        verified_reads.push(last_read);
    }
    Ok(())
}

/// Rewrite every recognized sequence. Unrecognized ones stay for `wave_sum`.
/// Returns the guest wave width (32 or 64 lanes) of the recognized sequences, or
/// `None` when there are none; the host wave must match so that `WavePrefixSum`
/// and `WaveActiveSum` are correct.
///
/// The exclusive prefix — which `wave_sum` does not recognize — is always lowered
/// here. The integer total is lowered by `wave_sum`, which lowers it per-lane and
/// verifies its control flow; only the totals it rejects are lowered here to
/// `WaveActiveSum` (see [`lower_problematic_totals`]), and only when the lockstep
/// fallback cannot prove the program. For pixel shaders every total is lowered
/// unconditionally, because a pixel is a wave of its own and has no lockstep
/// fallback.
pub(super) fn prepare(insns: &mut [Insn], stage: Stage) -> Result<Option<u32>, String> {
    let mut width = None;
    for a in 0..insns.len() {
        if insns[a].mnemonic == "s_or_saveexec_b64" {
            let matched = if stage == Stage::Pixel {
                total(insns, a).is_some() || prefix(insns, a).is_some()
            } else {
                prefix(insns, a).is_some()
            };
            if matched {
                match width {
                    None => width = Some(64),
                    Some(ow) if ow != 64 => return Err("conflicting wave allocation widths".into()),
                    Some(_) => (),
                }
            }
        } else if insns[a].mnemonic == "s_or_saveexec_b32" {
            let matched = if stage == Stage::Pixel {
                total32(insns, a).is_some() || prefix32(insns, a).is_some()
            } else {
                prefix32(insns, a).is_some()
            };
            if matched {
                match width {
                    None => width = Some(32),
                    Some(ow) if ow != 32 => return Err("conflicting wave allocation widths".into()),
                    Some(_) => (),
                }
            }
        }
    }
    Ok(width)
}

pub(super) fn emit(em: &mut Emitter, ins: &Insn) -> Result<bool, String> {
    let m = ins.mnemonic.as_str();
    if m != TOTAL && m != PREFIX { return Ok(false); }
    let d = *ins.dst.first().ok_or("missing wave sum destination")?;
    let value = super::r(&ins.src[0])?;
    // Every host invocation in this region runs the guest's whole-wave mode;
    // inactive guest lanes contribute the zero selected before the sum.
    let t = em.tmp;
    em.tmp += 1;
    if m == TOTAL {
        em.line(&format!("uint t{t} = WaveActiveSum({value});"));
        em.ws(&d, &format!("t{t}"))?;
    } else {
        em.line(&format!("uint t{t} = WavePrefixSum({value});"));
        em.wv(&d, &format!("t{t}"))?;
    }
    Ok(true)
}

#[cfg(test)]
#[path = "../tests/hlsl/wave_alloc.rs"]
mod tests;
