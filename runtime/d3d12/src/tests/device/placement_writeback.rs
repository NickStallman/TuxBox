use super::{Digest, publish_mip, query_region, write_mapped};
use crate::device::placement::readback_stats::Outcome;
use windows::Win32::System::Memory::{
    GetWriteWatch, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS,
    PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE, VIRTUAL_ALLOCATION_TYPE, VirtualAlloc,
    VirtualFree, VirtualProtect,
};

const PAGE: usize = 4096;
/// Allocation type that records CPU writes for GetWriteWatch.
const MEM_WRITE_WATCH: VIRTUAL_ALLOCATION_TYPE = VIRTUAL_ALLOCATION_TYPE(0x0020_0000);

struct Pages(*mut u8);

impl Pages {
    fn new(count: usize, extra: VIRTUAL_ALLOCATION_TYPE) -> Self {
        // SAFETY: a fresh private allocation.
        let base = unsafe { VirtualAlloc(None, count * PAGE, MEM_COMMIT | MEM_RESERVE | extra, PAGE_READWRITE) };
        assert!(!base.is_null());
        Self(base.cast())
    }

    fn protect(&self, page: usize, protection: PAGE_PROTECTION_FLAGS) {
        let mut old = PAGE_PROTECTION_FLAGS::default();
        // SAFETY: the page belongs to this allocation.
        unsafe { VirtualProtect(self.0.add(page * PAGE).cast(), PAGE, protection, &mut old).unwrap() };
    }

    fn bytes(&self, range: std::ops::Range<usize>) -> &[u8] {
        // SAFETY: tests only read pages they left readable.
        unsafe { std::slice::from_raw_parts(self.0.add(range.start), range.len()) }
    }

    /// Pages of a write-watch allocation that record a CPU write.
    fn watched_writes(&self, count: usize) -> usize {
        let mut addresses = [core::ptr::null_mut(); 16];
        let (mut found, mut granularity) = (addresses.len(), 0u32);
        // SAFETY: the outputs are locals of the stated capacity.
        let status = unsafe {
            GetWriteWatch(0, self.0.cast(), count * PAGE, Some(addresses.as_mut_ptr()), Some(&mut found), Some(&mut granularity))
        };
        assert_eq!(status, 0);
        found
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        // SAFETY: the allocation belongs to this test.
        unsafe { VirtualFree(self.0.cast(), 0, MEM_RELEASE).unwrap() };
    }
}

fn publish(pages: &Pages, bytes: &[u8], range: std::ops::Range<usize>) -> windows::core::Result<()> {
    let base = pages.0.addr();
    let mut region = query_region(base + range.start)?;
    write_mapped(base, bytes, range, &mut region)
}

#[test]
fn plain_data_pages_take_a_store() {
    assert!(crate::survey::install_handler());
    let pages = Pages::new(3, VIRTUAL_ALLOCATION_TYPE(0));
    assert!(query_region(pages.0.addr()).unwrap().2);
    let bytes: Vec<u8> = (0..3 * PAGE).map(|i| (i % 251) as u8 + 1).collect();
    publish(&pages, &bytes, 100..2 * PAGE + 7).unwrap();
    assert_eq!(pages.bytes(100..2 * PAGE + 7), &bytes[100..2 * PAGE + 7]);
    assert!(pages.bytes(0..100).iter().all(|&b| b == 0));
    assert!(pages.bytes(2 * PAGE + 7..3 * PAGE).iter().all(|&b| b == 0));
}

#[test]
fn a_write_watch_region_keeps_the_native_write() {
    let pages = Pages::new(2, MEM_WRITE_WATCH);
    assert!(!query_region(pages.0.addr()).unwrap().2);
    // Asking whether the region is watched neither adds nor clears a record.
    assert_eq!(pages.watched_writes(2), 0);
    let bytes = vec![0x77u8; 2 * PAGE];
    publish(&pages, &bytes, 8..PAGE + 8).unwrap();
    assert_eq!(pages.bytes(8..PAGE + 8), &bytes[8..PAGE + 8]);
    // A CPU write by the title is still recorded afterwards.
    // SAFETY: the page is committed and writable.
    unsafe { pages.0.add(PAGE + 100).write_volatile(1) };
    assert!(!query_region(pages.0.addr()).unwrap().2);
    assert!(pages.watched_writes(2) >= 1);
}

#[test]
fn closed_and_read_only_pages_are_not_stored_into() {
    let pages = Pages::new(3, VIRTUAL_ALLOCATION_TYPE(0));
    let bytes = vec![0x33u8; 3 * PAGE];
    // A page with no access has no guest bytes to publish and is skipped.
    pages.protect(1, PAGE_NOACCESS);
    publish(&pages, &bytes, 0..3 * PAGE).unwrap();
    assert!(pages.bytes(0..PAGE).iter().all(|&b| b == 0x33));
    assert!(pages.bytes(2 * PAGE..3 * PAGE).iter().all(|&b| b == 0x33));
    pages.protect(1, PAGE_READONLY);
    assert!(pages.bytes(PAGE..2 * PAGE).iter().all(|&b| b == 0));
    // A read-only page is the title's to protect: publication fails instead.
    assert!(publish(&pages, &bytes, 0..3 * PAGE).is_err());
    assert!(pages.bytes(PAGE..2 * PAGE).iter().all(|&b| b == 0));
}

/// Publish `bytes` as a whole mip over the start of `pages`.
fn publish_whole(pages: &Pages, bytes: &[u8], previous: Option<Digest>) -> windows::core::Result<(Option<Digest>, Outcome)> {
    let base = pages.0.addr();
    let mut region = query_region(base)?;
    publish_mip(base, bytes, None, &mut region, previous, &mut Vec::new())
}

#[test]
fn a_publication_of_the_bytes_title_memory_holds_writes_nothing() {
    let pages = Pages::new(2, VIRTUAL_ALLOCATION_TYPE(0));
    let bytes: Vec<u8> = (0..2 * PAGE).map(|i| (i % 249) as u8).collect();
    let previous = publish_whole(&pages, &bytes, None).unwrap();
    assert_eq!(previous, (Some(Digest::of(&bytes)), Outcome::Changed));
    // A store into a read-only page fails, so the publication must not try.
    pages.protect(0, PAGE_READONLY);
    pages.protect(1, PAGE_READONLY);
    assert_eq!(publish_whole(&pages, &bytes, previous.0).unwrap(), (previous.0, Outcome::Skipped));
}

#[test]
fn a_publication_still_writes_over_bytes_the_title_changed() {
    assert!(crate::survey::install_handler());
    let pages = Pages::new(1, VIRTUAL_ALLOCATION_TYPE(0));
    let gpu = vec![0x5au8; PAGE];
    let (previous, _) = publish_whole(&pages, &gpu, None).unwrap();
    // The title writes the texture's memory after the last publication; the
    // GPU then writes the same texels again.
    // SAFETY: the page is committed and writable.
    unsafe { pages.0.add(17).write_volatile(1) };
    assert_eq!(publish_whole(&pages, &gpu, previous).unwrap(), (previous, Outcome::Rewritten));
    assert_eq!(pages.bytes(0..PAGE), &gpu[..]);
    // New texels are written and become the baseline.
    let next = vec![0xa5u8; PAGE];
    assert_eq!(publish_whole(&pages, &next, previous).unwrap(), (Some(Digest::of(&next)), Outcome::Changed));
    assert_eq!(pages.bytes(0..PAGE), &next[..]);
}

#[test]
fn a_mip_with_holes_is_written_and_its_padding_kept() {
    let pages = Pages::new(1, VIRTUAL_ALLOCATION_TYPE(0));
    // SAFETY: the page is committed and writable.
    unsafe { pages.0.add(PAGE - 4).cast::<u32>().write_volatile(0x0909_0909) };
    let mut tiled = vec![3u8; PAGE];
    tiled[PAGE - 4..].fill(0);
    // Every 4-byte element but the last is the GPU's.
    let mut valid = vec![u64::MAX; PAGE / 4 / 64];
    *valid.last_mut().unwrap() &= !(1 << 63);
    let base = pages.0.addr();
    let mut region = query_region(base).unwrap();
    let mut expected = tiled.clone();
    expected[PAGE - 4..].fill(9);
    let previous = Some(Digest::of(&expected));
    // The comparison needs every byte, so a mip with holes is always written.
    let result = publish_mip(base, &tiled, Some((&valid, 4)), &mut region, previous, &mut Vec::new()).unwrap();
    assert_eq!(result, (previous, Outcome::Rewritten));
    assert_eq!(pages.bytes(0..PAGE), &expected[..]);
}

fn hardware_device() -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Device> {
    use windows::Win32::Graphics::{Direct3D::D3D_FEATURE_LEVEL_12_0, Direct3D12::D3D12CreateDevice, Dxgi::*};
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        if unsafe { adapter.GetDesc1() }.is_ok_and(|desc| desc.Flags & 2 != 0) {
            continue;
        }
        let mut device = None;
        if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_ok() {
            return device;
        }
    }
    None
}

#[test]
fn a_finished_readback_lends_its_buffer_and_list_to_the_next() {
    use super::{Readback, Spares};
    use crate::device::placement::{Images, Texture};
    use std::sync::Arc;
    use windows::Win32::Graphics::{Direct3D12::*, Dxgi::Common::*};
    use windows::core::Interface;
    let Some(device) = hardware_device() else { return };
    let pages = Pages::new(16, VIRTUAL_ALLOCATION_TYPE(0));
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 64,
        Height: 64,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT(0x100),
        ..Default::default()
    };
    let images = Arc::new(Images::default());
    let texture = Texture::new(&device, &images, pages.0.addr() as u64, &desc, None).unwrap();
    let spares = Arc::new(Spares::default());
    let queue: ID3D12CommandQueue = unsafe {
        device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
    }.unwrap();
    let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.unwrap();
    let mut buffers = Vec::new();
    for tick in 1..=3 {
        let readback = Readback::record(&device, D3D12_COMMAND_LIST_TYPE_DIRECT, &[texture.clone()], &spares).unwrap();
        buffers.push(readback.parts.buffer.as_raw().addr());
        unsafe {
            queue.ExecuteCommandLists(&[Some(readback.list.clone())]);
            queue.Signal(&fence, tick).unwrap();
            fence.SetEventOnCompletion(tick, windows::Win32::Foundation::HANDLE::default()).unwrap();
        }
        readback.complete().unwrap();
        readback.recycle();
    }
    assert_eq!(buffers, [buffers[0]; 3], "each readback after the first takes the finished one's buffer");
    assert_eq!(spares.sizes().len(), 1);
}
