//! Publish the GPU's texels to the original Xbox pages before the guest fence.
use super::{Digest, Texture, committed, lock};
use core::{mem::ManuallyDrop, ops::Range};
use std::sync::Arc;
use windows::Win32::Foundation::{
    E_ACCESSDENIED, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, HANDLE, NTSTATUS,
};
use windows::Win32::Graphics::{Direct3D12::*, Dxgi::Common::DXGI_SAMPLE_DESC};
use windows::Win32::System::{
    Diagnostics::Debug::WriteProcessMemory, Memory::*, Threading::GetCurrentProcess,
};
use windows::core::{Interface, Result};

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtWriteVirtualMemory(
        process: HANDLE,
        address: *mut core::ffi::c_void,
        buffer: *const core::ffi::c_void,
        size: usize,
        written: *mut usize,
    ) -> NTSTATUS;
}

/// A readback buffer with the allocator and list that copy into it, kept for
/// the queue's next publication once the GPU has finished with them.
pub(super) struct Parts {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    buffer: ID3D12Resource,
}

pub(super) type Spares = super::spares::Spares<Parts>;

pub(super) struct Readback {
    pub(super) list: ID3D12CommandList,
    parts: Parts,
    capacity: u64,
    spares: Arc<Spares>,
    size: usize,
    // One footprint offset per mip (including array slices) in the readback buffer.
    textures: Vec<(Arc<Texture>, Vec<u64>)>,
}

impl Parts {
    fn new(device: &ID3D12Device, kind: D3D12_COMMAND_LIST_TYPE, size: u64) -> Result<Self> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let buffer = committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
        )?;
        // SAFETY: the new allocator and command list belong to this device.
        let allocator = unsafe { device.CreateCommandAllocator(kind)? };
        let list = unsafe { device.CreateCommandList(0, kind, &allocator, None)? };
        Ok(Self { allocator, list, buffer })
    }

    /// Reopen a spare for recording. Its last copy has completed on the GPU.
    fn reopen(self) -> Option<Self> {
        unsafe { self.allocator.Reset() }.ok()?;
        unsafe { self.list.Reset(&self.allocator, None) }.ok()?;
        Some(self)
    }
}

impl Readback {
    pub(super) fn record(
        device: &ID3D12Device,
        kind: D3D12_COMMAND_LIST_TYPE,
        writers: &[Arc<Texture>],
        spares: &Arc<Spares>,
    ) -> Result<Self> {
        let mut total = 0u64;
        let mut textures = Vec::new();
        for texture in writers {
            let mut offsets = Vec::with_capacity(texture.mips.len());
            for mip in &texture.mips {
                let offset = total
                    .checked_next_multiple_of(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT.into())
                    .ok_or(E_OUTOFMEMORY)?;
                total = offset.checked_add(mip.bytes).ok_or(E_OUTOFMEMORY)?;
                offsets.push(offset);
            }
            textures.push((texture.clone(), offsets));
        }
        let size = usize::try_from(total)
            .ok()
            .filter(|&n| n <= isize::MAX as usize)
            .ok_or(E_OUTOFMEMORY)?;
        let capacity = super::spares::rounded(total).ok_or(E_OUTOFMEMORY)?;
        let spare = spares
            .take(capacity, std::time::Instant::now())
            .and_then(|(parts, capacity)| Some((parts.reopen()?, capacity)));
        let created = spare.is_none();
        let (parts, capacity) = match spare {
            Some(spare) => spare,
            None => (Parts::new(device, kind, capacity)?, capacity),
        };
        if super::readback_stats::enabled() {
            super::readback_stats::recorded(
                kind,
                writers.iter().map(|texture| super::readback_stats::Recorded {
                    key: Arc::as_ptr(texture).addr(),
                    desc: &texture.desc,
                    address: texture.address,
                    subresources: texture.mips.len(),
                    sources: texture.sources.swap(0, std::sync::atomic::Ordering::Relaxed),
                    copied: texture.mips.iter().map(|mip| mip.bytes).sum(),
                }),
                created.then_some(capacity),
            );
        }
        let Parts { list, buffer, .. } = &parts;
        for (texture, offsets) in &textures {
            for (level, &offset) in offsets.iter().enumerate() {
                let mut footprint = texture.mips[level].footprint;
                footprint.Offset = offset;
                let mut src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(texture.native.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: level as u32,
                    },
                };
                let mut dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(buffer.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: footprint,
                    },
                };
                // SAFETY: each tag selects its initialized union field. A separate
                // ExecuteCommandLists boundary decays the simultaneous-access image.
                unsafe {
                    list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                    ManuallyDrop::drop(&mut src.pResource);
                    ManuallyDrop::drop(&mut dst.pResource);
                }
            }
        }
        // The buffer and the source textures are retained until the worker
        // has seen this list complete.
        unsafe { list.Close()? };
        Ok(Self {
            list: list.cast()?,
            parts,
            capacity,
            spares: spares.clone(),
            size,
            textures,
        })
    }

    /// Keep the buffer and recording for the queue's next publication. The
    /// native fence covering this list has completed.
    pub(super) fn recycle(self) {
        self.spares.put(self.parts, self.capacity, std::time::Instant::now());
    }

    /// Called after the native fence covering every footprint has completed.
    pub(super) fn complete(&self) -> Result<()> {
        let mut mapped = core::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: self.size,
        };
        unsafe { self.parts.buffer.Map(0, Some(&range), Some(&mut mapped))? };
        // SAFETY: this is a completed GPU readback. The mapping covers this
        // allocation, remains live for the loop and is only accessed for reads.
        let data = unsafe { core::slice::from_raw_parts(mapped.cast::<u8>(), self.size) };
        let mut tiled = Vec::new();
        let mut valid = Vec::new();
        let mut guest = Vec::new();
        let stats = super::readback_stats::enabled();
        let result = self.textures.iter().try_for_each(|(texture, offsets)| {
            let mut state = lock(&texture.contents);
            for (level, &offset) in offsets.iter().enumerate() {
                let mip = &texture.mips[level];
                let size = (mip.source.end - mip.source.start) as usize;
                let element = mip.element_bytes as usize;
                let address = (texture.address + mip.source.start) as usize;
                let mut region = query_region(address)?;
                // There are no guest bytes to publish if a single unmapped
                // region covers this mip. Recheck on every publication, since
                // the guest can commit backing after creating the texture.
                if region.0 - address >= size && !mapped_region(&region.1) {
                    state.previous[level] = None;
                    if stats {
                        let key = Arc::as_ptr(texture).addr();
                        super::readback_stats::published(key, size as u64, super::readback_stats::Outcome::Unmapped);
                    }
                    continue;
                }
                tiled.clear();
                tiled.try_reserve_exact(size).map_err(|_| E_OUTOFMEMORY)?;
                tiled.resize(size, 0u8);
                let elements = size.div_ceil(element);
                let dense = u64::from(mip.elements)
                    * u64::from(mip.rows)
                    * u64::from(mip.footprint.Footprint.Depth)
                    * u64::from(mip.element_bytes)
                    == size as u64;
                if !dense {
                    valid.clear();
                    let words = elements.div_ceil(64);
                    valid.try_reserve_exact(words).map_err(|_| E_OUTOFMEMORY)?;
                    valid.resize(words, 0u64);
                }
                let linear = data
                    .get(offset as usize..(offset + mip.bytes) as usize)
                    .ok_or(E_INVALIDARG)?;
                texture.layout_runs(level, |target, source| {
                    tiled
                        .get_mut(target.clone())
                        .ok_or(E_INVALIDARG)?
                        .copy_from_slice(linear.get(source).ok_or(E_INVALIDARG)?);
                    if !dense {
                        mark_valid(&mut valid, target.start / element..target.end / element);
                    }
                    Ok(())
                })?;
                let valid = (!dense).then(|| (&valid[..], element));
                let (baseline, outcome) =
                    publish_mip(address, &tiled, valid, &mut region, state.previous[level], &mut guest)?;
                state.previous[level] = baseline;
                if stats {
                    super::readback_stats::published(Arc::as_ptr(texture).addr(), size as u64, outcome);
                }
            }
            Ok(())
        });
        // SAFETY: no CPU writes were made to the GPU readback allocation.
        unsafe { self.parts.buffer.Unmap(0, Some(&D3D12_RANGE::default())) };
        result
    }
}

/// Write one detiled mip to title memory at `address` and return the new
/// comparison baseline with what the publication did. `valid` marks the
/// elements the GPU image covers when the mip has holes; without it every
/// byte is the GPU's. `previous` is the baseline from the last upload or
/// publication.
fn publish_mip(
    address: usize,
    tiled: &[u8],
    valid: Option<(&[u64], usize)>,
    region: &mut Region,
    previous: Option<Digest>,
    guest: &mut Vec<u8>,
) -> Result<(Option<Digest>, super::readback_stats::Outcome)> {
    use super::readback_stats::Outcome;
    let size = tiled.len();
    // Without holes every byte of the mip is the GPU's.
    let whole = valid.is_none().then(|| Digest::of(tiled));
    // A pass that did not change the texture, or rewrote the same texels,
    // reads back what title memory last held. While the title's bytes are
    // still those, writing them again changes nothing: leave the pages alone,
    // as the console's GPU leaves them for a CPU write watch.
    if let Some(digest) = whole
        && previous == Some(digest)
        && snapshot(address as u64, size, guest)?
        && Digest::of(guest) == digest
    {
        return Ok((previous, Outcome::Skipped));
    }
    // Coalesce adjacent texels in physical address order. Never write row
    // padding, partial-tile holes or neighbouring mips.
    match valid {
        None => write_mapped(address, tiled, 0..size, region)?,
        Some((valid, element)) => {
            let elements = size.div_ceil(element);
            let mut first = next_bit(valid, 0, elements, true);
            while first < elements {
                let end = next_bit(valid, first, elements, false);
                write_mapped(address, tiled, first * element..end * element, region)?;
                first = next_bit(valid, end, elements, true);
            }
        }
    }
    // upload::refresh compares the entire physical mip. A whole mip now holds
    // the GPU's bytes, and a CPU write landing after this publication is the
    // title's, to be uploaded. A mip with holes takes its padding from guest
    // memory.
    let baseline = match whole {
        Some(digest) => Some(digest),
        None => snapshot(address as u64, size, guest)?.then(|| Digest::of(guest)),
    };
    let outcome = if baseline.is_some() && baseline == previous { Outcome::Rewritten } else { Outcome::Changed };
    Ok((baseline, outcome))
}

/// Read `size` guest bytes at `address` into `output`.
pub(super) fn snapshot(address: u64, size: usize, output: &mut Vec<u8>) -> Result<bool> {
    output.clear();
    output.try_reserve_exact(size).map_err(|_| E_OUTOFMEMORY)?;
    output.resize(size, 0);
    if !super::super::heap::backing::read_guest_bytes(address, output) {
        output.clear();
        return Ok(false);
    }
    Ok(true)
}

fn mark_valid(valid: &mut [u64], mut range: Range<usize>) {
    while range.start < range.end {
        let bit = range.start % 64;
        let count = range.len().min(64 - bit);
        valid[range.start / 64] |= (u64::MAX >> (64 - count)) << bit;
        range.start += count;
    }
}

fn next_bit(valid: &[u64], mut at: usize, limit: usize, set: bool) -> usize {
    while at < limit {
        let word = valid[at / 64];
        let matches = if set { word } else { !word };
        let bit = at % 64;
        let next = (matches & (u64::MAX << bit)).trailing_zeros() as usize;
        at = at - bit + next;
        if next < 64 {
            break;
        }
    }
    at.min(limit)
}

/// Where a run of pages with one state and protection ends, that state, and
/// whether a plain store may publish into it.
type Region = (usize, MEMORY_BASIC_INFORMATION, bool);

fn query_region(address: usize) -> Result<Region> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: VirtualQuery probes the address and fills a local structure.
    if unsafe {
        VirtualQuery(
            Some(address as *const _),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    } == 0
    {
        return Err(E_FAIL.into());
    }
    let end = (info.BaseAddress as usize)
        .checked_add(info.RegionSize)
        .filter(|&end| end > address)
        .ok_or(E_INVALIDARG)?;
    Ok((end, info, stores_directly(&info)))
}

/// Ordinary committed read-write data pages take a plain store. A write-watch
/// region keeps the native write, so what the title's watch records for a
/// published texture stays as it was on every host: a watch that Wine keeps
/// with page protection misses the native write but would record a store.
fn stores_directly(info: &MEMORY_BASIC_INFORMATION) -> bool {
    info.State == MEM_COMMIT
        && info.Protect == PAGE_READWRITE
        && super::guarded_copy::available()
        && !watched(info.BaseAddress)
}

/// Whether the region records CPU writes for GetWriteWatch. The query fails on
/// a region that was not allocated with MEM_WRITE_WATCH, and with no reset
/// flag it leaves the record alone.
fn watched(base: *mut core::ffi::c_void) -> bool {
    let (mut address, mut count, mut granularity) = (core::ptr::null_mut(), 1usize, 0u32);
    // SAFETY: the outputs are locals sized for one address.
    unsafe { GetWriteWatch(0, base, 4096, Some(&mut address), Some(&mut count), Some(&mut granularity)) == 0 }
}

fn mapped_region(info: &MEMORY_BASIC_INFORMATION) -> bool {
    info.State == MEM_COMMIT && info.Protect.0 & 0xff != PAGE_NOACCESS.0
}

/// Xbox can reserve a GPU address with no accessible CPU backing. Preserve its
/// native texture until pages are supplied. Never change guest page protection.
fn write_mapped(
    base: usize,
    bytes: &[u8],
    range: Range<usize>,
    region: &mut Region,
) -> Result<()> {
    if let Some(addr) = base.checked_add(range.start)
        && let Some(result) = super::super::heap::backing::try_write_xmem_bytes(addr as u64, &bytes[range.start..range.end])
    {
        return result;
    }
    let mut at = range.start;
    while at < range.end {
        let addr = base.checked_add(at).ok_or(E_INVALIDARG)?;
        // Runs visit increasing physical addresses. Reuse the protection scan
        // for the spans in this region until crossing into a different one.
        if addr >= region.0 {
            *region = query_region(addr)?;
        }
        let end = (region.0 - base).min(range.end);
        let info = &region.1;
        if mapped_region(info) {
            let access = PAGE_PROTECTION_FLAGS(info.Protect.0 & 0xff);
            if info.Protect.contains(PAGE_GUARD)
                || !matches!(
                    access,
                    PAGE_READWRITE
                        | PAGE_WRITECOPY
                        | PAGE_EXECUTE_READWRITE
                        | PAGE_EXECUTE_WRITECOPY
                )
            {
                return Err(E_ACCESSDENIED.into());
            }
            let block = &bytes[at..end];
            let mut written = 0;
            // SAFETY: the source is an owned initialized slice. Guest mappings
            // remain owned by the guest until the submission fence completes.
            // Plain data pages take a store: under Wine the native write is a
            // request to the server for every run, even in this process. The
            // guest can still unmap the pages after the query above, so the
            // store reports a fault as the native write does. Write-watch and
            // copy-on-write pages keep the native write, which also saves
            // Wine's region scan. Keep the instruction-cache flush for
            // executable mappings.
            unsafe {
                if region.2 {
                    if !super::guarded_copy::copy(addr as *mut u8, block) {
                        return Err(E_FAIL.into());
                    }
                    written = block.len();
                } else if matches!(access, PAGE_READWRITE | PAGE_WRITECOPY) {
                    let status = NtWriteVirtualMemory(
                        GetCurrentProcess(),
                        addr as *mut _,
                        block.as_ptr().cast(),
                        block.len(),
                        &mut written,
                    );
                    if status.0 < 0 {
                        return Err(E_FAIL.into());
                    }
                } else {
                    WriteProcessMemory(
                        GetCurrentProcess(),
                        addr as *const _,
                        block.as_ptr().cast(),
                        block.len(),
                        Some(&mut written),
                    )?;
                }
            };
            if written != block.len() {
                return Err(E_FAIL.into());
            }
        }
        at = end;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/device/placement_writeback.rs"]
mod tests;
