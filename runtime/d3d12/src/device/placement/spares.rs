//! Keep finished readback recordings for the next publication on their queue.
//!
//! A new readback buffer costs about as much CPU time on the submitting thread
//! as the rest of the publication's recording. On an RTX 5090 (620.70, GE-Proton
//! 11-14) creating a 925 MiB one with its allocator and list took 52-67 ms, and
//! the title's largest publications are that size.
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long an unused spare is kept.
pub(super) const RETAIN: Duration = Duration::from_secs(2);
/// The most bytes kept in spares for one queue.
pub(super) const RETAIN_BYTES: u64 = 1 << 30;
/// Sizes are rounded up to an eighth of their power of two so that
/// publications of similar size share spares.
pub(super) fn rounded(size: u64) -> Option<u64> {
    let step = (size.max(1 << 16).checked_next_power_of_two()? / 8).max(1 << 16);
    size.max(1).checked_next_multiple_of(step)
}

/// Whether a spare of `spare` bytes may serve `size`. A small publication
/// does not take a much larger spare, which a large one may need next.
fn fits(spare: u64, size: u64) -> bool {
    spare >= size && spare <= size.saturating_mul(2).max(size.saturating_add(4 << 20))
}

pub(super) struct Spares<T>(Mutex<Vec<(T, u64, Instant)>>);

impl<T> Default for Spares<T> {
    fn default() -> Self {
        Self(Mutex::new(Vec::new()))
    }
}

impl<T> Spares<T> {
    /// The smallest spare that fits `size`, and its size.
    pub(super) fn take(&self, size: u64, now: Instant) -> Option<(T, u64)> {
        let (taken, expired) = {
            let mut spares = super::lock(&self.0);
            let expired = Self::expire(&mut spares, now);
            let best = spares
                .iter()
                .enumerate()
                .filter(|(_, (_, spare, _))| fits(*spare, size))
                .min_by_key(|(_, (_, spare, _))| *spare)
                .map(|(index, _)| index);
            (best.map(|index| spares.swap_remove(index)), expired)
        };
        // Released outside the lock: freeing a large buffer takes a while.
        drop(expired);
        taken.map(|(item, size, _)| (item, size))
    }

    pub(super) fn put(&self, item: T, size: u64, now: Instant) {
        let released = {
            let mut spares = super::lock(&self.0);
            let mut released = Self::expire(&mut spares, now);
            // Newest first: keep the newest spares within the budget.
            spares.insert(0, (item, size, now));
            let mut kept = 0u64;
            let mut index = 0;
            while index < spares.len() {
                let total = kept.saturating_add(spares[index].1);
                if total > RETAIN_BYTES {
                    released.push(spares.remove(index));
                } else {
                    kept = total;
                    index += 1;
                }
            }
            released
        };
        drop(released);
    }

    fn expire(spares: &mut Vec<(T, u64, Instant)>, now: Instant) -> Vec<(T, u64, Instant)> {
        spares
            .extract_if(.., |(_, _, at)| now.saturating_duration_since(*at) >= RETAIN)
            .collect()
    }

    #[cfg(test)]
    pub(super) fn sizes(&self) -> Vec<u64> {
        let mut sizes: Vec<_> = super::lock(&self.0).iter().map(|(_, size, _)| *size).collect();
        sizes.sort_unstable();
        sizes
    }
}

#[cfg(test)]
#[path = "../../tests/device/placement/spares.rs"]
mod tests;
