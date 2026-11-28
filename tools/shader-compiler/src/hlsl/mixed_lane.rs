//! Lower mixed vector data and scalar lane storage for one complete guest Wave32.
//!
//! The lowering maps guest VGPRs to per-invocation values and lane reads to scalar
//! storage. Physical lane writes select an invocation by its lane index, regardless
//! of guest EXEC.
//!
//! # Boundaries
//!
//! This module accepts only complete 32-thread compute groups and the verified
//! instruction subset checked by [`prepare`]. Resource resolution and whole-wave
//! scalar dispatch remain prerequisites supplied by the translation pipeline.
use super::{compute_group, ray_resolve, Enc, Insn, Opnd, ShaderExtract, Stage};
use std::collections::BTreeSet;

/// Physical lane write, independent of guest EXEC, with a constant selector.
pub(super) const WRITE_LANE: &str = "tb_mixed_writelane";

/// Validate the single-wave contract and all lane operations before rewriting.
/// Returns false outside mixed SGPR-selected storage; selected unsupported encodings fail.
pub(super) fn prepare(ex: &ShaderExtract, insns: &mut [Insn]) -> Result<bool, String> {
    let count = compute_group(ex)
        .into_iter()
        .try_fold(1u32, u32::checked_mul);
    if ex.stage != Stage::Compute || count != Some(32) {
        return Ok(false);
    }
    let written: BTreeSet<_> = insns
        .iter()
        .filter(|op| op.mnemonic == "v_writelane_b32")
        .filter_map(|op| match op.dst.first() {
            Some(Opnd::Vgpr(v)) => Some(*v),
            _ => None,
        })
        .collect();
    if !insns.iter().any(|op| op.mnemonic == "v_readlane_b32"
        && matches!(op.src.as_slice(), [Opnd::Vgpr(v), Opnd::Sgpr(_), ..] if written.contains(v)))
        || !insns.iter().any(|op| {
            // Wave64 compares require a pair-aligned scalar destination.
            op.enc == Enc::Vop3
                && op.mnemonic.starts_with("v_cmp_")
                && matches!(op.dst.as_slice(), [Opnd::Sgpr(n @ 0..=105)] if n & 1 != 0)
        })
    {
        return Ok(false);
    }
    for op in insns.iter() {
        let m = op.mnemonic.as_str();
        // Group memory is addressed per lane, and DPP reads lanes of the 32-lane host wave
        // the resolve_wave32 mode pins to the guest's. Lane exchanges through group memory,
        // the global data share and barriers have no verified contract here.
        let lane_exchange = op.enc == Enc::Ds
            && (matches!(m, "ds_swizzle_b32" | "ds_bpermute_b32" | "ds_permute_b32") || op.raw[0] & (1 << 17) != 0);
        let wide_mask = (m.ends_with("_b64") || m.ends_with("_i64") || m.ends_with("_u64"))
            && op
                .src
                .iter()
                .chain(&op.dst)
                .any(|v| matches!(v, Opnd::ExecLo | Opnd::VccLo));
        if lane_exchange
            || wide_mask
            || op
                .src
                .iter()
                .chain(&op.dst)
                .any(|v| matches!(v, Opnd::ExecHi | Opnd::VccHi))
            || m.ends_with("saveexec_b64")
            || m.ends_with("wrexec_b64")
            || matches!(
                m,
                "s_barrier"
                    | "s_swappc_b64"
                    | "s_call_b64"
                    | "v_mbcnt_hi_u32_b32"
                    | "v_readlane_regrd_b32"
                    | "v_writelane_regwr_b32"
            )
        {
            return Err(format!(
                "mixed lane Wave32 cannot lower {m} at {:#x}",
                op.offset
            ));
        }
        if !matches!(m, "v_readlane_b32" | "v_writelane_b32") {
            continue;
        }
        let read = m == "v_readlane_b32";
        let scalar = |v: &Opnd| {
            matches!(
                v,
                Opnd::Sgpr(0..=105) | Opnd::ConstInt(_) | Opnd::ConstF32(_) | Opnd::Literal(_)
            )
        };
        let lane = |v: &Opnd| {
            matches!(v, Opnd::ConstInt(_) | Opnd::Literal(_))
                || read && matches!(v, Opnd::Sgpr(0..=105))
        };
        // SRC2 is unused by both instructions, including assembler-filled bits.
        // Reject VOP3 modifiers and retain the decoder's literal-length checks.
        let encoding = op.enc == Enc::Vop3
            && op.op == (if read { 0x360 } else { 0x361 })
            && op.is_complete()
            && op.raw[0] & 0xffffff00 == (if read { 0xd7600000 } else { 0xd7610000 })
            && op.raw[1] >> 27 == 0
            && op.aux.sdwa.is_none();
        let operands = match (op.dst.as_slice(), op.src.as_slice()) {
            ([Opnd::Vgpr(d)], [value, selector, _]) => {
                (!read || *d <= 105)
                    && lane(selector)
                    && if read {
                        matches!(value, Opnd::Vgpr(_))
                    } else {
                        scalar(value)
                    }
            }
            _ => false,
        };
        if !encoding || !operands {
            return Err(format!(
                "mixed lane Wave32 unsupported {m} encoding at {:#x}",
                op.offset
            ));
        }
    }
    // Only the storage `v_writelane_b32` fills is read through scalar storage. Other lane
    // reads, such as a wave reduction's last, stay for the full-mask emitter.
    for op in insns.iter_mut() {
        match op.mnemonic.as_str() {
            "v_readlane_b32" if matches!(op.src.first(), Some(Opnd::Vgpr(v)) if written.contains(v)) => {
                let Opnd::Vgpr(dst) = op.dst[0] else {
                    unreachable!()
                };
                op.dst[0] = Opnd::Sgpr(dst);
                op.enc = Enc::Sop1;
                op.mnemonic = ray_resolve::READ_LANE.into();
                op.src.truncate(2);
            }
            "v_writelane_b32" => {
                op.mnemonic = WRITE_LANE.into();
                op.src.truncate(2);
            }
            _ => {}
        }
    }
    Ok(true)
}
