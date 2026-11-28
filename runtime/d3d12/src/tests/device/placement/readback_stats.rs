use super::*;

fn desc(width: u64, height: u32, format: i32, mips: u16) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: width,
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: mips,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(format),
        ..Default::default()
    }
}

#[test]
fn the_costliest_textures_are_named_with_their_shape_sources_and_unchanged_mips() {
    let (target, map) = (desc(3840, 2160, 10, 1), desc(256, 256, 41, 9));
    let mut window = Window::default();
    let texture = |key, desc, sources, copied| Recorded { key, desc, address: key as u64 * 0x10000, subresources: desc.MipLevels as usize, sources, copied };
    for _ in 0..3 {
        window.recorded(
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            [texture(1, &target, Source::RenderTarget as u8, 64 << 20), texture(2, &map, Source::HeapView as u8, 1 << 20)],
            Some(128 << 20),
        );
    }
    window.recorded(D3D12_COMMAND_LIST_TYPE_COMPUTE, [texture(2, &map, Source::Copy as u8, 1 << 20)], None);
    window.published(1, 64 << 20, Outcome::Changed);
    window.published(2, 1 << 20, Outcome::Skipped);
    window.published(2, 1 << 20, Outcome::Rewritten);
    window.published(2, 1 << 20, Outcome::Unmapped);

    let lines = window.report(5.0);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("batches=4 textures=2 copied=196.0MiB largest=65.0MiB/2 textures"), "{}", lines[0]);
    assert!(lines[0].contains("unchanged=2 (2.0MiB, 1 written again) unmapped=1"), "{}", lines[0]);
    assert!(lines[0].contains("buffers created=3 (384.0MiB) reused=1"), "{}", lines[0]);
    assert!(lines[1].starts_with("placed texture readback top 1: 0x10000 3840x2160x1 format=10"), "{}", lines[1]);
    assert!(lines[1].contains("readbacks=3 copied=192.0MiB unchanged=0/1 mips"), "{}", lines[1]);
    assert!(lines[1].ends_with("sources=render-target queues=direct"), "{}", lines[1]);
    assert!(lines[2].contains("256x256x1 format=41 layout=0x0 flags=0x0 mips=9 subresources=9 readbacks=4"), "{}", lines[2]);
    assert!(lines[2].contains("unchanged=2/3 mips (2.0MiB) unmapped=1"), "{}", lines[2]);
    assert!(lines[2].ends_with("sources=copy+heap-uav queues=direct+compute"), "{}", lines[2]);
}

#[test]
fn only_the_top_textures_are_listed() {
    let shape = desc(64, 64, 28, 1);
    let mut window = Window::default();
    window.recorded(
        D3D12_COMMAND_LIST_TYPE_DIRECT,
        (1..=20).map(|key| Recorded { key, desc: &shape, address: 0, subresources: 1, sources: 0, copied: key as u64 }),
        None,
    );
    let lines = window.report(5.0);
    assert_eq!(lines.len(), 1 + TOP);
    assert!(lines[1].contains("copied=0.0MiB") && lines[1].ends_with("sources=unknown queues=direct"), "{}", lines[1]);
}
