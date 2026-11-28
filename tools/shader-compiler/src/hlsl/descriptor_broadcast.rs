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
/// if every path back meets one before anything writes `saved` or `v`. EXEC is then
/// what it was at the copy, whatever happened to it in between.
fn restored(insns: &[Insn], at: usize, saved: u8, v: u8) -> Option<Vec<usize>> {
    let (mut saves, mut seen, mut work) = (Vec::new(), BTreeSet::new(), vec![at]);
    while let Some(at) = work.pop() {
        if at == 0 { return None; }
        for k in previous(insns, at) {
            if !seen.insert(k) { continue; }
            let i = &insns[k];
            if i.mnemonic == "s_mov_b64" && i.dst == [Opnd::Sgpr(saved)] && i.src == [Opnd::ExecLo] {
                saves.push(k);
            } else if writes_sgpr_pair(i, saved) || writes_vgpr(i, v) {
                return None;
            } else {
                work.push(k);
            }
        }
    }
    Some(saves)
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
                let copy = i.mnemonic == "v_mov_b32" && i.enc == Enc::Vop1 && i.is_complete() && i.aux.sdwa.is_none()
                    && i.dst == [Opnd::Vgpr(*v)] && matches!(i.src.as_slice(), [Opnd::Sgpr(0..=105)]);
                if copy {
                    found.insert(k);
                    continue;
                }
                if writes_vgpr(i, *v) { continue 'reads; }
                if let ("s_mov_b64", [Opnd::ExecLo], [Opnd::Sgpr(saved)]) = (i.mnemonic.as_str(), i.dst.as_slice(), i.src.as_slice()) {
                    // Carry on from where EXEC was saved.
                    let Some(saves) = restored(insns, k, *saved, *v) else { continue 'reads };
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
