//! Heap descriptors a wave copies into vector registers and reads back.
//!
//! A program that takes a texture by a run-time heap index can hold the descriptor
//! in vector registers and serve it through a waterfall loop:
//!
//! ```text
//!         v_mov_b32 vK, sK                   ; one copy per descriptor word
//!   loop: v_readfirstlane_b32 sD, vK         ; the first lane's descriptor
//!         v_cmp_eq_u64 ...                   ; lanes holding the same one
//!         s_and_saveexec_b64 sM, sM
//!         image_sample ..., sD, ...
//!         s_xor_b64 exec, exec, sM           ; retire them
//!         s_cbranch_execnz loop
//! ```
//!
//! A copy writes only the lanes active at it, so the read returns the copied
//! scalar only if its first active lane was active at the copy and nothing wrote
//! the register since. That holds when, on every path back from the read:
//! - the first write of the register met is such a copy, and
//! - EXEC only narrows on the way: by `s_and_saveexec`, by `v_cmpx`, or by handing
//!   the lanes a preceding `s_and_saveexec` set aside back out of the mask it saved.
//!   Restoring a copy of EXEC taken on the way counts from where it was taken.
//!
//! The copies may also sit in both arms of an if/else whose end restores the EXEC its
//! `s_and_saveexec_b64` saved (see `joined`): each lane then took one arm, and that
//! arm copied the scalar for it last. An if/else that never writes the register
//! leaves it as it was (see `around`).
use super::{heap_offsets, Emitter, Enc, Insn, Opnd};
use std::collections::BTreeSet;

fn writes_exec(i: &Insn) -> bool {
    i.dst.iter().any(|o| matches!(o, Opnd::ExecLo | Opnd::ExecHi))
        || i.mnemonic.contains("saveexec") || i.mnemonic.contains("wrexec") || i.mnemonic.starts_with("v_cmpx")
}

fn writes_vgpr(i: &Insn, v: u8) -> bool {
    i.mnemonic != "v_nop" && i.dst.iter().any(|o| matches!(*o, Opnd::Vgpr(n)
        if u16::from(n) <= u16::from(v) && u16::from(v) < u16::from(n) + heap_offsets::result_words(i).max(1)))
}

/// Whether `i` may write `s` or `s + 1`: a load fills a register per word, a scalar
/// operation without a 64-bit operand and a lane read one register, any other
/// instruction at most a 64-bit pair.
fn writes_sgpr_pair(i: &Insn, s: u8) -> bool {
    let words = if i.enc == Enc::Smem {
        i.mnemonic.split_once("dwordx").and_then(|(_, n)| n.parse::<u16>().ok()).unwrap_or(1)
    } else if matches!(i.enc, Enc::Sop1 | Enc::Sop2 | Enc::Sopk) && !i.mnemonic.contains("64")
        || matches!(i.mnemonic.as_str(), "v_readfirstlane_b32" | "v_readlane_b32") {
        1
    } else { 2 };
    i.dst.iter().any(|o| matches!(*o, Opnd::Sgpr(n) if u16::from(n) <= u16::from(s) + 1 && u16::from(s) < u16::from(n) + words))
}

/// `exec = saved ^ exec` or `exec = saved & ~exec`, as `Some(saved)`.
fn hands_back(i: &Insn) -> Option<u8> {
    if i.dst != [Opnd::ExecLo] { return None; }
    match (i.mnemonic.as_str(), i.src.as_slice()) {
        ("s_xor_b64", [Opnd::ExecLo, Opnd::Sgpr(m)] | [Opnd::Sgpr(m), Opnd::ExecLo]) => Some(*m),
        ("s_andn2_b64", [Opnd::Sgpr(m), Opnd::ExecLo]) => Some(*m),
        _ => None,
    }
}

/// Whether the mask in `saved` at `at` is the EXEC a preceding `s_and_saveexec_b64`
/// replaced, with EXEC only narrowed and `v` unwritten since.
fn set_aside(insns: &[Insn], targets: &BTreeSet<i64>, at: usize, saved: u8, v: u8) -> bool {
    for k in (0..at).rev() {
        if targets.contains(&(insns[k + 1].offset as i64)) { return false; }
        let i = &insns[k];
        if i.mnemonic == "s_and_saveexec_b64" && i.dst == [Opnd::Sgpr(saved)] { return true; }
        let narrows = i.mnemonic == "s_and_saveexec_b64" || i.mnemonic.starts_with("v_cmpx");
        if writes_sgpr_pair(i, saved) || writes_vgpr(i, v) || (writes_exec(i) && !narrows) { return false; }
    }
    false
}

/// The instructions that can run just before `at`: the one above it, unless that
/// one never falls through, and every branch to it.
fn previous(insns: &[Insn], at: usize) -> Vec<usize> {
    let above = at.checked_sub(1).filter(|&k| !(insns[k].enc == Enc::Sopp && matches!(insns[k].op, 1 | 2)));
    let jumps = insns.iter().enumerate().filter(|(_, i)| {
        i.enc == Enc::Sopp && Emitter::branch_cond(i.op).is_some() && i.offset as i64 + 4 + 4 * i.aux.imm == insns[at].offset as i64
    }).map(|(k, _)| k);
    above.into_iter().chain(jumps).collect()
}

/// For `s_mov_b64 exec, saved` at `at`: the `s_mov_b64 saved, exec` copies it restores,
/// if every path back meets one before anything writes `saved` or `v` (when given). EXEC is then
/// what it was at the copy, whatever happened to it in between.
fn restored(insns: &[Insn], at: usize, saved: u8, v: Option<u8>) -> Option<Vec<usize>> {
    let (mut saves, mut seen, mut work) = (Vec::new(), BTreeSet::new(), vec![at]);
    while let Some(at) = work.pop() {
        if at == 0 { return None; }
        for k in previous(insns, at) {
            if !seen.insert(k) { continue; }
            let i = &insns[k];
            if i.mnemonic == "s_mov_b64" && i.dst == [Opnd::Sgpr(saved)] && i.src == [Opnd::ExecLo] {
                saves.push(k);
            } else if writes_sgpr_pair(i, saved) || v.is_some_and(|v| writes_vgpr(i, v)) {
                return None;
            } else {
                work.push(k);
            }
        }
    }
    Some(saves)
}

/// Whether `i` narrows EXEC: every lane it leaves on was on before.
fn narrows_exec(i: &Insn) -> bool {
    let m = i.mnemonic.as_str();
    m.starts_with("s_and_saveexec_b") || m.starts_with("v_cmpx")
        || (i.dst == [Opnd::ExecLo] && (m == "s_and_b64" && i.src.contains(&Opnd::ExecLo)
            || m == "s_andn2_b64" && i.src.first() == Some(&Opnd::ExecLo)))
}

/// Whether the mask in `m` at `at` holds only lanes of the arm entered at `entry`: on
/// every path back into the arm it was last set from EXEC, or narrowed from itself.
/// EXEC never leaves the arm's lanes there (see `arm_lanes`), so neither does the mask.
fn arm_mask(insns: &[Insn], at: usize, entry: usize, m: u8) -> bool {
    let (mut seen, mut work) = (BTreeSet::new(), vec![at]);
    while let Some(at) = work.pop() {
        for k in previous(insns, at) {
            if k <= entry { return false; }
            if !seen.insert(k) { continue; }
            let i = &insns[k];
            if !writes_sgpr_pair(i, m) {
                work.push(k);
                continue;
            }
            let name = i.mnemonic.as_str();
            let own = i.dst == [Opnd::Sgpr(m)];
            if own && (name == "s_mov_b64" && i.src == [Opnd::ExecLo] || name.ends_with("saveexec_b64")) { continue; }
            let narrowed = own && match (name, i.src.as_slice()) {
                ("s_and_b64", [a, b]) => [a, b].contains(&&Opnd::Sgpr(m)) || [a, b].contains(&&Opnd::ExecLo),
                ("s_andn2_b64", [a, _]) => *a == Opnd::Sgpr(m) || *a == Opnd::ExecLo,
                _ => false,
            };
            if !narrowed { return false; }
            // Narrowed from itself: what it held before still decides.
            if i.src.contains(&Opnd::Sgpr(m)) { work.push(k); }
        }
    }
    true
}

/// Whether EXEC stays within the lanes the arm `(entry, end)` began with: each write
/// of EXEC in it narrows, or takes a mask of the arm's own (`arm_mask`).
fn arm_lanes(insns: &[Insn], entry: usize, end: usize) -> bool {
    (entry + 1..end).all(|k| {
        let i = &insns[k];
        if !writes_exec(i) || narrows_exec(i) { return true; }
        let masks: Vec<u8> = match (i.mnemonic.as_str(), i.src.as_slice()) {
            ("s_mov_b64", [Opnd::Sgpr(m)]) if i.dst == [Opnd::ExecLo] => vec![*m],
            ("s_and_b64", [a, b]) if i.dst == [Opnd::ExecLo] => [a, b].into_iter()
                .filter_map(|o| if let Opnd::Sgpr(m) = o { Some(*m) } else { None }).collect(),
            _ => hands_back(i).into_iter().collect(),
        };
        masks.into_iter().any(|m| arm_mask(insns, k, entry, m))
    })
}

/// Whether EXEC at `at` is still the EXEC just after `entry`: on every path back, the
/// first write of EXEC met, if any, restores a copy kept where this held.
fn entry_exec(insns: &[Insn], at: usize, entry: usize, depth: u8) -> bool {
    if depth > 8 { return false; }
    let (mut seen, mut work) = (BTreeSet::new(), vec![at]);
    while let Some(at) = work.pop() {
        if at == entry + 1 { continue; }
        for k in previous(insns, at) {
            if k <= entry { return false; }
            if !seen.insert(k) { continue; }
            let i = &insns[k];
            if let ("s_mov_b64", [Opnd::ExecLo], [Opnd::Sgpr(saved)]) = (i.mnemonic.as_str(), i.dst.as_slice(), i.src.as_slice()) {
                match restored(insns, k, *saved, None) {
                    Some(saves) if !saves.is_empty() && saves.iter().all(|&save| entry_exec(insns, save, entry, depth + 1)) => continue,
                    _ => return false,
                }
            }
            if writes_exec(i) { return false; }
            work.push(k);
        }
    }
    true
}

/// The copies of `v` that give it to every lane of an if/else whose end, the
/// `s_mov_b64 exec, saved` at `join`, restores the EXEC its head saved:
///
/// ```text
///         s_and_saveexec_b64 saved, cond     ; head: the then lanes
///         s_cbranch_execz else
///         ... v_mov_b32 v, sA ...            ; then arm
///   else: s_andn2_b64 exec, saved, exec      ; the rest of the saved lanes
///         s_cbranch_execz join
///         ... v_mov_b32 v, sB ...            ; else arm
///   join: s_mov_b64 exec, saved
/// ```
///
/// Each arm is entered only from its start, keeps EXEC within the lanes it began with,
/// and on every path to its end writes `v` last by a copy made with all those lanes on.
/// A lane the then arm turns off goes through the else arm. The `s_cbranch_execz`
/// right after an arm's start skips it only when it has no lanes. Whether both copies
/// hold the same word is for the descriptor dataflow to decide.
fn joined(insns: &[Insn], join: usize, saved: u8, v: u8) -> Option<Vec<usize>> {
    let head = (0..join).rev().find(|&k| writes_sgpr_pair(&insns[k], saved))?;
    if insns[head].mnemonic != "s_and_saveexec_b64" || insns[head].dst != [Opnd::Sgpr(saved)] { return None; }
    let mut splits = (head + 1..join).filter(|&k| hands_back(&insns[k]) == Some(saved));
    let split = splits.next()?;
    if splits.next().is_some() { return None; }
    let mut found = Vec::new();
    for (entry, end) in [(head, split), (split, join)] {
        if !closed(insns, entry, end) || !arm_lanes(insns, entry, end) { return None; }
        let skip = |k: usize| k == entry + 1 && insns[k].enc == Enc::Sopp && insns[k].op == 8;
        let (mut seen, mut work) = (BTreeSet::new(), vec![end]);
        while let Some(at) = work.pop() {
            for k in previous(insns, at) {
                if skip(k) && at != k + 1 { continue; }
                // A path from before the arm wrote nothing here.
                if k <= entry { return None; }
                if !seen.insert(k) { continue; }
                let i = &insns[k];
                if is_copy(i, v) {
                    if !entry_exec(insns, k, entry, 0) { return None; }
                    found.push(k);
                    continue;
                }
                if writes_vgpr(i, v) { return None; }
                work.push(k);
            }
        }
    }
    Some(found)
}

/// Whether branches in `(entry, end)` stay in it and only they reach into it.
fn closed(insns: &[Insn], entry: usize, end: usize) -> bool {
    let inner = insns[entry + 1].offset as i64..=insns[end].offset as i64;
    insns.iter().enumerate().all(|(k, i)| {
        if i.enc != Enc::Sopp || Emitter::branch_cond(i.op).is_none() { return true; }
        let to = i.offset as i64 + 4 + 4 * i.aux.imm;
        (entry < k && k < end) == inner.contains(&to)
    })
}

/// For a `s_mov_b64 exec, saved` at `end` that closes code which never writes `v`:
/// the `s_and_saveexec_b64 saved` that opened it, the only way in. EXEC is again what
/// it was there, and so is `v` in every lane.
fn around(insns: &[Insn], end: usize, saved: u8, v: u8) -> Option<usize> {
    let head = (0..end).rev().find(|&k| writes_sgpr_pair(&insns[k], saved))?;
    let opens = insns[head].mnemonic == "s_and_saveexec_b64" && insns[head].dst == [Opnd::Sgpr(saved)];
    (opens && closed(insns, head, end) && !(head + 1..end).any(|k| writes_vgpr(&insns[k], v))).then_some(head)
}

fn is_copy(i: &Insn, v: u8) -> bool {
    i.mnemonic == "v_mov_b32" && i.enc == Enc::Vop1 && i.is_complete() && i.aux.sdwa.is_none()
        && i.dst == [Opnd::Vgpr(v)] && matches!(i.src.as_slice(), [Opnd::Sgpr(0..=105)])
}

/// The copies (`v_mov_b32 v, s`) and first-lane reads proven to return the copied scalar.
pub(super) fn find(insns: &[Insn]) -> (BTreeSet<usize>, BTreeSet<usize>) {
    let (mut copies, mut reads) = (BTreeSet::new(), BTreeSet::new());
    if insns.iter().any(|i| matches!(i.mnemonic.as_str(), "s_setpc_b64" | "s_swappc_b64" | "s_call_b64")) {
        return (copies, reads);
    }
    let targets: BTreeSet<i64> = insns.iter()
        .filter(|i| i.enc == Enc::Sopp && Emitter::branch_cond(i.op).is_some())
        .map(|i| i.offset as i64 + 4 + 4 * i.aux.imm).collect();
    'reads: for (read, ins) in insns.iter().enumerate() {
        let (true, [Opnd::Vgpr(v)]) = (ins.mnemonic == "v_readfirstlane_b32", ins.src.as_slice()) else { continue };
        let (mut found, mut seen, mut work) = (BTreeSet::new(), BTreeSet::new(), vec![read]);
        while let Some(at) = work.pop() {
            // The program entry: no copy on this path.
            if at == 0 { continue 'reads; }
            for k in previous(insns, at) {
                if !seen.insert(k) { continue; }
                let i = &insns[k];
                if is_copy(i, *v) {
                    found.insert(k);
                    continue;
                }
                if writes_vgpr(i, *v) { continue 'reads; }
                if let ("s_mov_b64", [Opnd::ExecLo], [Opnd::Sgpr(saved)]) = (i.mnemonic.as_str(), i.dst.as_slice(), i.src.as_slice()) {
                    // Carry on from where EXEC was saved.
                    if let Some(arms) = joined(insns, k, *saved, *v) {
                        found.extend(arms);
                        continue;
                    }
                    if let Some(head) = around(insns, k, *saved, *v) {
                        if seen.insert(head) { work.push(head); }
                        continue;
                    }
                    let Some(saves) = restored(insns, k, *saved, Some(*v)) else { continue 'reads };
                    work.extend(saves.into_iter().filter(|&save| seen.insert(save)));
                    continue;
                }
                let narrows = i.mnemonic.starts_with("s_and_saveexec_b") || i.mnemonic.starts_with("v_cmpx")
                    || hands_back(i).is_some_and(|saved| set_aside(insns, &targets, k, saved, *v));
                if writes_exec(i) && !narrows { continue 'reads; }
                work.push(k);
            }
        }
        if !found.is_empty() {
            copies.extend(found);
            reads.insert(read);
        }
    }
    (copies, reads)
}

#[cfg(test)]
#[path = "../tests/hlsl/descriptor_broadcast.rs"]
mod tests;
