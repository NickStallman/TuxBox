//! Loops whose body meets the whole thread group at a barrier.
//!
//! Each host thread leaves a translated loop by its own copy of the guest
//! condition. A guest condition is uniform for its 64-lane wave at most, and a
//! group can hold several guest waves: a 128-thread mesh group holds two, and
//! a loop over distinct values can run a different number of times in each.
//! When the body holds a group rendezvous (a `tb_page_*` vote or lane read),
//! the wave that leaves first stops reaching barriers the other still waits
//! on, and the barriers of one wave pair with different barriers of the other.
//!
//! Such a loop instead runs until no thread of the group wants another
//! iteration. A thread that is done keeps its skip target aside and skips
//! everything up to the end of the loop: it changes no state but still meets
//! every barrier. After the loop it takes its skip target back.
use super::{not_skipping, Emitter};
use std::collections::BTreeSet;

/// A skip target past every instruction. Every guard of a group loop's body
/// tests it, so a thread that has left the loop runs none of it.
pub(super) const SKIP_ALL: usize = 0xffff_ffff;

/// Whether any thread of the group votes true. Every thread must call it.
pub(super) const GROUP_ANY: &str = r#"
groupshared uint tb_group_vote;
bool tb_group_any(bool vote) {
    tb_group_vote = 0u;
    GroupMemoryBarrierWithGroupSync();
    if (vote) InterlockedOr(tb_group_vote, 1u);
    GroupMemoryBarrierWithGroupSync();
    bool any = tb_group_vote != 0u;
    GroupMemoryBarrierWithGroupSync();
    return any;
}
"#;

/// The back edges of the loops that must iterate as a group: every loop whose
/// instructions hold a rendezvous. An enclosing loop holds the same
/// instruction, so it is chosen too and every thread enters the inner loop.
pub(super) fn select(loops: &[(usize, usize)], rendezvous: impl Fn(usize) -> bool) -> BTreeSet<usize> {
    loops.iter().filter(|&&(head, back)| (head..=back).any(&rendezvous)).map(|&(_, back)| back).collect()
}

/// Per-thread state of each group loop.
pub(super) fn declare(em: &mut Emitter<'_>, back: usize) {
    em.line(&format!("bool tb_live{back} = true; uint tb_rest{back} = 0u, tb_round{back} = 0u;"));
}

/// Before the loop's `do`: every thread starts the loop live.
pub(super) fn enter(em: &mut Emitter<'_>, back: usize) {
    em.line(&format!("tb_live{back} = true; tb_round{back} = 0u;"));
}

/// The end of an iteration. `again` is the thread's own condition for another
/// one, evaluated outside every guard.
pub(super) fn close(em: &mut Emitter<'_>, back: usize, again: &str) {
    em.line(&format!(
        "if (tb_live{back} && !({again})) {{ tb_live{back} = false; tb_rest{back} = tb_skip; tb_skip = {SKIP_ALL}u; }}"
    ));
    // The vote and the count are the same in every thread, so all of them leave together.
    em.line(&format!("}} while (tb_group_any(tb_live{back}) && ++tb_round{back} < 65536u);"));
    em.line(&format!("if (!tb_live{back}) tb_skip = tb_rest{back};"));
    em.group_votes = true;
}

/// The vote of a thread at a rendezvous hoisted out of the guard that `active`
/// opens: false for a thread that guard skips.
pub(super) fn guarded(active: &[usize], vote: &str) -> String {
    match active.iter().min() {
        Some(&nearest) => format!("{} && {vote}", not_skipping(nearest)),
        None => vote.to_string(),
    }
}

#[cfg(test)]
#[path = "../tests/hlsl/group_loops.rs"]
mod tests;
