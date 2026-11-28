//! A digest of one mip's guest bytes, kept to notice that the title changed them.
//!
//! The provider used to keep an exact copy of every placed texture for this
//! comparison, which is as much memory again as the textures themselves.

const SEEDS: [u64; 4] = [0x243f_6a88_85a3_08d3, 0x1319_8a2e_0370_7344, 0xa409_3822_299f_31d0, 0x082e_fa98_ec4e_6c89];
const FIRST: u64 = 0x9e37_79b1_85eb_ca87;
const SECOND: u64 = 0xc2b2_ae3d_27d4_eb4f;

/// Four independent 64-bit lanes and the length. Each step is invertible in its
/// lane and in its input word, so changing any one word always changes the
/// result. Other collisions need a chance match of 64 bits or more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Digest {
    lanes: [u64; 4],
    bytes: usize,
}

fn round(lane: u64, word: u64) -> u64 {
    lane.wrapping_add(word.wrapping_mul(SECOND)).rotate_left(31).wrapping_mul(FIRST)
}

impl Digest {
    pub(super) fn of(bytes: &[u8]) -> Self {
        let mut lanes = SEEDS;
        let mut mix = |block: &[u8; 32]| {
            for (lane, word) in lanes.iter_mut().zip(block.as_chunks::<8>().0) {
                *lane = round(*lane, u64::from_le_bytes(*word));
            }
        };
        let (blocks, tail) = bytes.as_chunks::<32>();
        blocks.iter().for_each(&mut mix);
        if !tail.is_empty() {
            let mut last = [0; 32];
            last[..tail.len()].copy_from_slice(tail);
            mix(&last);
        }
        Self { lanes, bytes: bytes.len() }
    }
}
