use super::*;
use crate::encode::{Random, code, references, stream};
use crate::record::{self, Format};

fn controls() -> Vec<(usize, Control)> {
    CONTROL
        .iter()
        .enumerate()
        .filter_map(|(k, c)| c.map(|c| (k, c)))
        .collect()
}

fn random_state(c: Control, random: &mut Random) -> State {
    let mut st = State {
        subsets: c.subsets() as u8,
        ..Default::default()
    };
    for (ch, &p) in c.precision().iter().enumerate() {
        for row in st.ep.iter_mut().take(c.subsets()) {
            for e in 0..2 {
                if p != 0 {
                    row[2 * ch + e] = ((random.next() & ((1 << p) - 1)) as u8) << (8 - p);
                }
            }
        }
    }
    for (set, &b) in INDEX_BITS[usize::from(c.mode)].iter().enumerate() {
        for ix in st.ix[set].iter_mut() {
            if b != 0 {
                *ix = ((random.next() & ((1 << b) - 1)) as u8) << (4 - b);
            }
        }
    }
    st
}

/// Lane symbols for blocks of the given controls and states.
fn symbols(blocks: &[(usize, Control, State)], refs: &[(usize, usize)]) -> Vec<Vec<u16>> {
    let mut lanes = vec![vec![0u16; 2 * blocks.len()]; 16];
    for (i, ((code_word, c, target), &(er, xr))) in blocks.iter().zip(refs).enumerate() {
        let get = |r| match source(i, r) {
            Source::Zero => State::default(),
            Source::Block(j) => blocks[j].2,
            Source::Dictionary(k) => dictionary(k),
        };
        let (e, x) = (get(er), get(xr));
        let mut put = |lane: usize, (a, b): (u16, u16)| {
            lanes[lane][2 * i] = a;
            lanes[lane][2 * i + 1] = b;
        };
        put(0, deinterleave(code(xr as u8, er as u8, 8) as u8));
        put(1, deinterleave(xr as u8));
        put(15, ((code_word & 31) as u16, (code_word >> 5) as u16));
        let subsets = c.subsets();
        let common = subsets.min(usize::from(e.subsets));
        for (ch, &p) in c.precision().iter().enumerate() {
            if p == 0 {
                continue;
            }
            for row in 0..subsets {
                let from = if row < common { row } else { 0 };
                let lane = endpoint_lane(subsets, row, ch).unwrap();
                let codes: Vec<u16> = (0..2)
                    .map(|k| {
                        code(
                            e.ep[from][2 * ch + k] >> (8 - p),
                            target.ep[row][2 * ch + k] >> (8 - p),
                            u32::from(p),
                        )
                    })
                    .collect();
                put(lane, (codes[0], codes[1]));
            }
        }
        for (set, &b) in INDEX_BITS[usize::from(c.mode)].iter().enumerate() {
            if b == 0 {
                continue;
            }
            for (row, &lane) in INDEX_LANES[set].iter().enumerate() {
                let half = |h: usize| {
                    let codes: Vec<u16> = (0..2)
                        .map(|k| {
                            let p = 4 * row + 2 * h + k;
                            code(
                                x.ix[set][p] >> (4 - b),
                                target.ix[set][p] >> (4 - b),
                                u32::from(b),
                            )
                        })
                        .collect();
                    index_symbol(codes[0], codes[1], row)
                };
                put(lane, (half(0), half(1)));
            }
        }
    }
    lanes
}

#[test]
fn every_bc7_header_has_one_control_code() {
    let all = controls();
    assert_eq!(all.len(), 285);
    for (_, c) in &all {
        assert!(c.partition >> PARTITION_BITS[usize::from(c.mode)] == 0);
        assert_eq!(all.iter().filter(|(_, d)| d == c).count(), 1);
    }
}

#[test]
fn random_blocks_round_trip() {
    let all = controls();
    let mut random = Random(0xbc7);
    for (count, longest) in [(1usize, 1usize), (400, 150), (1500, 2000)] {
        let blocks: Vec<(usize, Control, State)> = (0..count)
            .map(|_| {
                let (k, c) = all[random.below(all.len() as u64) as usize];
                (k, c, random_state(c, &mut random))
            })
            .collect();
        let refs: Vec<(usize, usize)> = (0..count).map(|i| references(i, &mut random)).collect();
        let data = stream(
            Format::Bc7.widths(),
            &symbols(&blocks, &refs),
            longest,
            &mut random,
        );
        let mut expected = vec![0u8; count * 16];
        for ((_, c, st), block) in blocks.iter().zip(expected.chunks_exact_mut(16)) {
            pack(*c, st, block);
        }
        let mut out = vec![0u8; expected.len()];
        let report = record::decode(Format::Bc7, &data, &mut out).unwrap();
        assert_eq!(out, expected);
        assert_eq!(report.unknown_controls, 0);
        // Every block is a BC7 block of the mode its control names.
        for ((_, c, _), block) in blocks.iter().zip(out.chunks_exact(16)) {
            assert_eq!(block[0].trailing_zeros(), u32::from(c.mode));
        }
    }
}

/// Two mode-6 blocks the storage guest decodes: block 1 predicts from block
/// 0 and from dictionary entry 0.
fn guest_fixture() -> (Vec<u8>, Vec<u8>) {
    let (k, c) = controls().into_iter().find(|(_, c)| c.mode == 6).unwrap();
    let mut random = Random(0xb7);
    let blocks: Vec<(usize, Control, State)> = (0..2)
        .map(|_| (k, c, random_state(c, &mut random)))
        .collect();
    let refs = [(0, 0), (1, 2)];
    let data = stream(
        Format::Bc7.widths(),
        &symbols(&blocks, &refs),
        2,
        &mut random,
    );
    let mut expected = vec![0u8; 32];
    for ((_, c, st), block) in blocks.iter().zip(expected.chunks_exact_mut(16)) {
        pack(*c, st, block);
    }
    (data, expected)
}

#[test]
fn the_guest_fixture_decodes() {
    let (data, blocks) = guest_fixture();
    if std::env::var_os("PRINT_FIXTURE").is_some() {
        let hex = |b: &[u8]| {
            b.iter()
                .map(|v| format!("0x{v:02x}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "stream {} bytes\n{}\nblocks\n{}",
            data.len(),
            hex(&data),
            hex(&blocks)
        );
    }
    assert_eq!(data, GUEST_STREAM);
    let mut out = vec![0u8; blocks.len()];
    record::decode(Format::Bc7, &data, &mut out).unwrap();
    assert_eq!(out, blocks);
}

/// The same bytes as `bc7` in runtime/xcrdapi/guest.cpp.
const GUEST_STREAM: &[u8] = &[
    0x1a, 0x11, 0x1d, 0x19, 0x1a, 0x16, 0x1e, 0x15, 0x03, 0x03, 0x05, 0x04, 0x04, 0x04, 0x03, 0x04,
    0x01, 0x0b, 0x01, 0x08, 0x40, 0xac, 0x01, 0x03, 0x63, 0x92, 0x00, 0xd3, 0x00, 0xbf, 0x62, 0x79,
    0x7f, 0x01, 0xff, 0xf9, 0xfe, 0xb7, 0x46, 0x06, 0x41, 0xc1, 0xec, 0xb5, 0x9c, 0x30, 0xe5, 0x97,
    0x00, 0x00, 0x00, 0x00, 0x9d, 0x17, 0x15, 0x12, 0x17, 0x12, 0x00, 0x1d, 0x10, 0x15, 0x03, 0x03,
    0x03, 0x03, 0x15, 0x05, 0x05, 0x04, 0x11, 0x3d, 0x0b, 0x4b, 0x05, 0x01, 0x01, 0x0d, 0x8b, 0x5c,
    0x63, 0x91, 0x02, 0x00, 0x00, 0xb0, 0x0d, 0x1d, 0x18, 0x00, 0x74, 0xff, 0x5a, 0x02, 0x04, 0xe5,
    0xa2, 0x9e, 0xf7, 0x1f, 0x32, 0xf3, 0x04, 0x1c, 0xbc, 0x14, 0x04, 0x07, 0x16, 0x18, 0x19, 0x13,
    0x03, 0x69, 0x01, 0x04, 0x03, 0x05, 0x03, 0x26, 0x03, 0x00, 0x09, 0x9e, 0x01, 0x42, 0xaa, 0x38,
    0xff, 0x24, 0x4c, 0x00, 0xbc, 0x00, 0x57, 0x77, 0xfd, 0xc7, 0xff, 0xd4, 0x6b, 0xff, 0x54, 0x66,
    0xd9, 0xd0, 0x58, 0x00, 0x00, 0x8f, 0x00, 0x00, 0x78, 0x00, 0x19, 0x16, 0x00, 0xb0, 0x00, 0x1b,
    0x12, 0x1c, 0x1f, 0x10, 0x00, 0x1c, 0x04, 0x03, 0x03, 0x03, 0x04, 0x18, 0x03, 0x05, 0x07, 0x42,
    0x02, 0x01, 0x10, 0x04, 0x12, 0x01, 0x87, 0xdf, 0xeb, 0x00, 0x08, 0x88, 0xbe, 0x00, 0xfc, 0x02,
    0x21, 0x40, 0x00, 0x70, 0x0d, 0xff, 0x66, 0x55, 0x71, 0x02, 0x09, 0xf8, 0x51, 0x55, 0x04, 0x03,
    0x18, 0x17, 0x1a, 0x7f, 0x04, 0x03, 0x04, 0x80, 0x55, 0x04, 0x40, 0xa1, 0x55, 0xc7, 0x00, 0x9e,
    0xfe, 0x1d, 0xfe, 0x84, 0x11, 0xbc, 0x19, 0x00, 0x00, 0x00, 0x00, 0x11, 0x60, 0x00, 0x07, 0x15,
    0x1f, 0x17, 0x19, 0x00, 0x19, 0x17, 0x16, 0x1d, 0x03, 0x03, 0x05, 0x18, 0x03, 0x04, 0x03, 0x03,
    0x01, 0x04, 0x01, 0x03, 0x12, 0x01, 0x2d, 0x01, 0x00, 0x44, 0x00, 0x9e, 0xbe, 0xf0, 0x7a, 0xbd,
    0x10, 0x6b, 0xff, 0x4c, 0x0d, 0xe5, 0x04, 0x86, 0x01, 0xab, 0xc4, 0xd4, 0x51, 0x1c, 0x47, 0xe9,
    0x56, 0x15, 0x1b, 0x01, 0x03, 0x16, 0x03, 0x05, 0x00, 0x47, 0x04, 0x1d, 0x01, 0xff, 0xb3, 0x01,
    0xd4, 0x00, 0x8a, 0x27, 0x20, 0x03, 0xff, 0xff, 0x2c, 0xf5, 0x9d, 0x74, 0x91, 0xfc, 0x1d, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x05, 0x1a, 0x00, 0x00, 0x00, 0x01, 0x03, 0x00, 0x02, 0xff, 0x20, 0x57,
    0x01, 0x00, 0x01, 0x00,
];
