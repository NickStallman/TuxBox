//! With `TUXBOX_LOG`, name the placed textures whose GPU writes are read back
//! into title memory, about every 5 s: how much each costs, why it was
//! counted as written, and whether its publication changed any guest bytes.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D12::*;

const WINDOW: Duration = Duration::from_secs(5);
const TOP: usize = 8;

/// Why a batch counts a placed texture as written.
#[derive(Clone, Copy)]
pub(crate) enum Source {
    /// Bound as a render target of a draw.
    RenderTarget = 1,
    /// Cleared as a render target.
    Clear = 2,
    /// The destination of a copy or a UAV clear.
    Copy = 4,
    /// A writable view in a descriptor heap a draw or dispatch can index.
    HeapView = 8,
}

const SOURCE_NAMES: [(u8, &str); 4] = [(1, "render-target"), (2, "clear"), (4, "copy"), (8, "heap-uav")];

pub(super) fn enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(crate::diagnostics::logging);
    *ENABLED
}

/// Remember why a texture is written until its next readback is recorded.
pub(super) fn note(sources: &AtomicU8, source: Source) {
    if enabled() {
        sources.fetch_or(source as u8, Ordering::Relaxed);
    }
}

/// What publishing one mip did to title memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Outcome {
    /// The guest bytes changed.
    Changed,
    /// The GPU's bytes were written, and title memory ended as the last
    /// upload or publication left it.
    Rewritten,
    /// The GPU's bytes were already in title memory, so none were written.
    Skipped,
    /// No guest pages back the mip.
    Unmapped,
}

#[derive(Default, Clone)]
pub(super) struct Entry {
    pub(super) desc: D3D12_RESOURCE_DESC,
    pub(super) address: u64,
    pub(super) subresources: usize,
    pub(super) sources: u8,
    /// Bit per `D3D12_COMMAND_LIST_TYPE` that recorded a readback.
    pub(super) queues: u8,
    pub(super) readbacks: u64,
    /// Footprint bytes copied from the GPU image.
    pub(super) copied: u64,
    pub(super) mips: u64,
    /// Mips whose publication left title memory as it was.
    pub(super) unchanged: u64,
    pub(super) unchanged_bytes: u64,
    pub(super) unmapped: u64,
}

#[derive(Default)]
pub(super) struct Window {
    start: Option<Instant>,
    pub(super) batches: u64,
    pub(super) copied: u64,
    pub(super) largest: (u64, usize),
    pub(super) created: (u64, u64),
    pub(super) reused: u64,
    pub(super) outcomes: [(u64, u64); 4],
    pub(super) textures: HashMap<usize, Entry>,
}

static STATS: LazyLock<Mutex<Window>> = LazyLock::new(Mutex::default);

/// One texture in a recorded readback.
pub(super) struct Recorded<'a> {
    pub(super) key: usize,
    pub(super) desc: &'a D3D12_RESOURCE_DESC,
    pub(super) address: u64,
    pub(super) subresources: usize,
    pub(super) sources: u8,
    pub(super) copied: u64,
}

impl Window {
    pub(super) fn recorded<'a>(
        &mut self,
        kind: D3D12_COMMAND_LIST_TYPE,
        textures: impl IntoIterator<Item = Recorded<'a>>,
        created: Option<u64>,
    ) {
        let mut batch = (0, 0);
        for texture in textures {
            let entry = self.textures.entry(texture.key).or_insert_with(|| Entry {
                desc: *texture.desc,
                address: texture.address,
                subresources: texture.subresources,
                ..Entry::default()
            });
            entry.sources |= texture.sources;
            entry.queues |= 1 << (kind.0 & 7);
            entry.readbacks += 1;
            entry.copied += texture.copied;
            batch = (batch.0 + texture.copied, batch.1 + 1);
        }
        self.batches += 1;
        self.copied += batch.0;
        self.largest = self.largest.max(batch);
        match created {
            Some(bytes) => self.created = (self.created.0 + 1, self.created.1 + bytes),
            None => self.reused += 1,
        }
    }

    pub(super) fn published(&mut self, key: usize, bytes: u64, outcome: Outcome) {
        let slot = &mut self.outcomes[outcome as usize];
        *slot = (slot.0 + 1, slot.1 + bytes);
        if let Some(entry) = self.textures.get_mut(&key) {
            entry.mips += 1;
            match outcome {
                Outcome::Changed => {}
                Outcome::Rewritten | Outcome::Skipped => {
                    entry.unchanged += 1;
                    entry.unchanged_bytes += bytes;
                }
                Outcome::Unmapped => entry.unmapped += 1,
            }
        }
    }

    /// The summary line and one line per costliest texture.
    pub(super) fn report(&self, seconds: f64) -> Vec<String> {
        const MIB: f64 = (1 << 20) as f64;
        let mib = |bytes: u64| bytes as f64 / MIB;
        let [changed, rewritten, skipped, unmapped] = self.outcomes;
        let mut lines = vec![format!(
            "placed texture readback {seconds:.1}s: batches={} textures={} copied={:.1}MiB largest={:.1}MiB/{} textures \
             | mips changed={} ({:.1}MiB) unchanged={} ({:.1}MiB, {} written again) unmapped={} \
             | buffers created={} ({:.1}MiB) reused={}",
            self.batches, self.textures.len(), mib(self.copied), mib(self.largest.0), self.largest.1,
            changed.0, mib(changed.1), rewritten.0 + skipped.0, mib(rewritten.1 + skipped.1), rewritten.0,
            unmapped.0, self.created.0, mib(self.created.1), self.reused,
        )];
        let mut top: Vec<_> = self.textures.values().collect();
        top.sort_by_key(|entry| std::cmp::Reverse(entry.copied));
        for (rank, entry) in top.iter().take(TOP).enumerate() {
            let desc = &entry.desc;
            let sources: Vec<_> = SOURCE_NAMES.iter().filter(|(bit, _)| entry.sources & bit != 0).map(|(_, name)| *name).collect();
            let queues: Vec<_> = [(0, "direct"), (2, "compute"), (3, "copy")]
                .iter()
                .filter(|(kind, _)| entry.queues & (1 << kind) != 0)
                .map(|(_, name)| *name)
                .collect();
            lines.push(format!(
                "placed texture readback top {}: {:#x} {}x{}x{} format={} layout={:#x} flags={:#x} mips={} subresources={} \
                 readbacks={} copied={:.1}MiB unchanged={}/{} mips ({:.1}MiB) unmapped={} sources={} queues={}",
                rank + 1, entry.address, desc.Width, desc.Height, desc.DepthOrArraySize, desc.Format.0, desc.Layout.0,
                desc.Flags.0, desc.MipLevels, entry.subresources, entry.readbacks, mib(entry.copied), entry.unchanged,
                entry.mips, mib(entry.unchanged_bytes), entry.unmapped,
                if sources.is_empty() { "unknown".into() } else { sources.join("+") }, queues.join("+"),
            ));
        }
        lines
    }

    /// Log and restart the window once it is old enough.
    fn roll(&mut self, now: Instant) {
        let start = *self.start.get_or_insert(now);
        let elapsed = now.saturating_duration_since(start);
        if elapsed < WINDOW {
            return;
        }
        if self.batches != 0 || self.outcomes.iter().any(|(count, _)| *count != 0) {
            for line in self.report(elapsed.as_secs_f64()) {
                crate::diagnostics::log(&line);
            }
        }
        *self = Self { start: Some(now), ..Self::default() };
    }
}

pub(super) fn recorded<'a>(
    kind: D3D12_COMMAND_LIST_TYPE,
    textures: impl IntoIterator<Item = Recorded<'a>>,
    created: Option<u64>,
) {
    let mut stats = super::lock(&STATS);
    stats.recorded(kind, textures, created);
    stats.roll(Instant::now());
}

pub(super) fn published(key: usize, bytes: u64, outcome: Outcome) {
    let mut stats = super::lock(&STATS);
    stats.published(key, bytes, outcome);
    stats.roll(Instant::now());
}

#[cfg(test)]
#[path = "../../tests/device/placement/readback_stats.rs"]
mod tests;
