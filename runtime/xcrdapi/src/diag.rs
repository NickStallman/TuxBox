//! Request-option diagnostics.
//!
//! A request whose options this provider does not implement (BCPack, swizzle,
//! physical-page sources) completes with `E_NOTIMPL` and leaves its
//! destination unwritten. The first such request for each distinct option word
//! is always logged. `TUXBOX_XCRD_LOG=1` also counts every request by option
//! word and logs the totals every five seconds and when a queue closes. A
//! request that fails while it runs is logged with the stage that failed.
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use windows_core::HRESULT;

/// Fields of `DSTORAGE_REQUEST_OPTIONS` (dstorage_xs.h), low bit first:
/// ZlibDecompress 1, BcpackMode 4, SwizzleMode 16, DestinationIsPhysicalPages 1,
/// SourceType 1, SourceIsPhysicalPages 1, then 40 reserved bits.
pub(crate) fn describe(options: u64) -> String {
    let mut text = format!("{options:#x}");
    let mut parts = Vec::new();
    if options & 1 != 0 {
        parts.push("zlib".to_owned());
    }
    let bcpack = (options >> 1) & 0xf;
    if bcpack != 0 {
        const NAMES: [&str; 8] = ["none", "BC1", "BC2", "BC3", "BC4", "BC5", "BC6H", "BC7"];
        parts.push(format!(
            "bcpack={}",
            NAMES.get(bcpack as usize).copied().unwrap_or("?")
        ));
    }
    let swizzle = (options >> 5) & 0xffff;
    if swizzle != 0 {
        parts.push(format!("swizzle={swizzle}"));
    }
    for (bit, name) in [(21, "to-pages"), (22, "from-memory"), (23, "from-pages")] {
        if options >> bit & 1 != 0 {
            parts.push(name.to_owned());
        }
    }
    if options >> 24 != 0 {
        parts.push(format!("reserved={:#x}", options >> 24));
    }
    if !parts.is_empty() {
        let _ = write!(text, " ({})", parts.join(" "));
    }
    text
}

#[derive(Default)]
struct Counts {
    requests: u64,
    refused: u64,
    failed: u64,
    cancelled: u64,
    /// Cancelled requests that would have failed.
    cancelled_failing: u64,
    source_bytes: u64,
    destination_bytes: u64,
    intermediate_bytes: u64,
    largest_destination: u32,
}

struct Log {
    by_options: BTreeMap<u64, Counts>,
    last_dump: Instant,
}

static COUNTING: LazyLock<bool> =
    LazyLock::new(|| std::env::var("TUXBOX_XCRD_LOG").is_ok_and(|v| !v.is_empty() && v != "0"));
static LOG: LazyLock<Mutex<Log>> = LazyLock::new(|| {
    Mutex::new(Log {
        by_options: BTreeMap::new(),
        last_dump: Instant::now(),
    })
});
const DUMP_EVERY: Duration = Duration::from_secs(5);
/// Bounds the always-on refusal lines if a title sends unexpected words.
const MAX_REFUSED_KINDS: usize = 64;

/// Records one enqueued request. `refused` is the capture error, if any.
pub(crate) fn request(
    options: u64,
    size: u32,
    destination_size: u32,
    intermediate_size: u32,
    refused: Option<HRESULT>,
) {
    if !*COUNTING && refused.is_none() {
        return;
    }
    let mut log = crate::lock(&LOG);
    let first_refusal = refused.is_some()
        && log.by_options.len() < MAX_REFUSED_KINDS
        && log.by_options.get(&options).is_none_or(|c| c.refused == 0);
    if !*COUNTING && !first_refusal {
        return;
    }
    let counts = log.by_options.entry(options).or_default();
    counts.requests += 1;
    counts.source_bytes += u64::from(size);
    counts.destination_bytes += u64::from(destination_size);
    counts.intermediate_bytes += u64::from(intermediate_size);
    counts.largest_destination = counts.largest_destination.max(destination_size);
    if let Some(hr) = refused {
        counts.refused += 1;
        if first_refusal {
            crate::queue::debug(&format!(
                "TuxBox xcrdapi: refused request options {} ({:#010x}, source {size} bytes, \
                 destination {destination_size}, intermediate {intermediate_size}); \
                 its destination is not written",
                describe(options),
                hr.0 as u32
            ));
        }
    }
    if *COUNTING && log.last_dump.elapsed() >= DUMP_EVERY {
        log.last_dump = Instant::now();
        dump(&log);
    }
}

/// Records a captured request that failed while it ran (read or decode error).
pub(crate) fn failed(options: u64) {
    if *COUNTING {
        crate::lock(&LOG)
            .by_options
            .entry(options)
            .or_default()
            .failed += 1;
    }
}

/// Records a request the title cancelled; `failing` if it failed anyway.
pub(crate) fn cancelled(options: u64, failing: bool) {
    if *COUNTING {
        let mut log = crate::lock(&LOG);
        let c = log.by_options.entry(options).or_default();
        c.cancelled += 1;
        c.cancelled_failing += u64::from(failing);
    }
}

/// Logs why a captured request failed: the first 64 failures, then every
/// 1024th, with or without `TUXBOX_XCRD_LOG`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn failure(
    options: u64,
    stage: &str,
    hr: HRESULT,
    source: &str,
    size: usize,
    destination: usize,
    intermediate: usize,
    pages: Option<(&[u64], usize)>,
    detail: &str,
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static FAILURES: AtomicU64 = AtomicU64::new(0);
    let n = FAILURES.fetch_add(1, Ordering::Relaxed);
    if n >= 64 && !n.is_multiple_of(1024) {
        return;
    }
    let pages = pages.map_or(String::new(), |(tokens, offset)| {
        let shown: Vec<String> = tokens.iter().take(4).map(|t| format!("{t:#x}")).collect();
        format!(
            ", {} pages from offset {offset:#x} [{}{}]",
            tokens.len(),
            shown.join(" "),
            if tokens.len() > 4 { " ..." } else { "" }
        )
    });
    crate::queue::debug(&format!(
        "TuxBox xcrdapi: request failed #{} at {stage} ({:#010x}{}{detail}): options {}, {source}, \
         source {size} bytes, destination {destination}, intermediate {intermediate}{pages}",
        n + 1,
        hr.0 as u32,
        if detail.is_empty() { "" } else { ", " },
        describe(options),
    ));
}

/// Logs the totals now, as a queue closes.
pub(crate) fn flush() {
    if *COUNTING {
        let mut log = crate::lock(&LOG);
        log.last_dump = Instant::now();
        dump(&log);
    }
}

fn dump(log: &Log) {
    for (options, c) in &log.by_options {
        crate::queue::debug(&format!(
            "TuxBox xcrdapi: options {}: requests {} refused {} failed {} cancelled {} \
             (failing {}) source bytes {} destination bytes {} intermediate bytes {} \
             largest destination {}",
            describe(*options),
            c.requests,
            c.refused,
            c.failed,
            c.cancelled,
            c.cancelled_failing,
            c.source_bytes,
            c.destination_bytes,
            c.intermediate_bytes,
            c.largest_destination
        ));
    }
}
