//! The group base of a compute dispatch split into several host dispatches.
//!
//! A console dispatch can have more groups in a dimension than the 65,535 a
//! desktop `Dispatch` allows. The runtime records it as several dispatches of
//! at most that many groups each. Before each one it sets the group ID the
//! dispatch starts at through three root constants it appends to the
//! pipeline's root (`b0, space32766`). A program translated against a root
//! that ends with them adds them to its group ID, so every group sees the ID
//! it has on the console. The group count a program reads comes from the
//! title's own constants, which keep the console values.

use crate::rootsig::{RootParamKind, RootSig};

/// Register and space of the group base constants.
pub const REGISTER: u32 = 0;
pub const SPACE: u32 = 32766;
/// X, Y and Z.
pub const WORDS: u32 = 3;

/// Whether a root parameter is the group base the runtime appends.
pub fn is_group_base(kind: &RootParamKind) -> bool {
    matches!(*kind, RootParamKind::Constants { register: REGISTER, space: SPACE, num32: WORDS })
}

/// Remove the group base from the end of `root`, where the runtime appends
/// it. The console program was compiled without it, so the rest of the
/// translation must not see it. Returns whether it was there.
pub fn take(root: &mut RootSig) -> bool {
    if root.params.last().is_some_and(|param| is_group_base(&param.kind)) {
        root.params.pop();
        true
    } else {
        false
    }
}

/// One host dispatch of a split: the console group ID of its first group
/// and its group counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub base: [u32; 3],
    pub groups: [u32; 3],
}

/// The dispatches that cover `groups`, each at most `limit` groups in every
/// dimension. Only a dimension over the limit is divided. `None` when more
/// than `max_parts` dispatches would be needed or `limit` is zero.
pub fn split(groups: [u32; 3], limit: u32, max_parts: usize) -> Option<Vec<Part>> {
    if limit == 0 {
        return None;
    }
    let counts = groups.map(|n| n.div_ceil(limit).max(1));
    let total = counts.iter().try_fold(1usize, |total, &n| total.checked_mul(n as usize))?;
    if total > max_parts {
        return None;
    }
    let span = |dim: usize, index: u32| {
        let base = index * limit;
        (base, (groups[dim] - base).min(limit))
    };
    let mut parts = Vec::with_capacity(total);
    for z in 0..counts[2] {
        for y in 0..counts[1] {
            for x in 0..counts[0] {
                let [(bx, nx), (by, ny), (bz, nz)] = [span(0, x), span(1, y), span(2, z)];
                parts.push(Part { base: [bx, by, bz], groups: [nx, ny, nz] });
            }
        }
    }
    Some(parts)
}

#[cfg(test)]
#[path = "tests/group_base.rs"]
mod tests;
