// bcpack-rs by Ahmet Dedeler (https://github.com/Ahmet-Dedeler, https://ahmetdedeler.com)
// Source: https://github.com/Ahmet-Dedeler/bcpack-rs
// SPDX-License-Identifier: MIT OR Apache-2.0
// If you copy, vendor or port this file, keep this header and credit bcpack-rs in your README.
//
// Written for TuxBox after docs/BCPACK_SPEC.md and src/bc1.rs (`assemble`) of
// bcpack-rs 5bf5cc56ed56f1ababf1f9592300bd182ff90bd8, with the index
// reference of 2d6333b03af4eaa33b2cffe9a8ab0439424955dd; used under Apache-2.0.
//! BCPack BC1: 12 rANS lanes of one symbol per block, rebuilt into 8-byte
//! BC1 blocks.
//!
//! | Lane | Width | Symbol |
//! | --- | --- | --- |
//! | 0 | 8 | back reference: `d = unzigzag(v) mod 256` blocks back, 0 for none |
//! | 1 | 8 | index reference `x`; 0 in all retail data bcpack-rs measured |
//! | 2-7 | 5 6 5 5 6 5 | zigzag residuals of R0 G0 B0 R1 G1 B1 |
//! | 8-11 | 8 | index codebook symbols of the 2x2 quadrants TL TR BL BR |
//!
//! The endpoint reference is `d = (x + unzigzag(lane 0)) mod 256`. A block's
//! endpoint fields are those of the block `d` back plus the residuals, each
//! modulo its width; a reference before the first block reads slot
//! `(i - d) mod 256` of the format's window ([`crate::tables::bc1`]). The
//! index codes are the codebook's residuals added, as positions along the
//! endpoint line (codes 0, 2, 3, 1), to block `i - x`; an index reference
//! before the first block has no known table and reads zero.
use crate::lanes::Symbols;
use crate::rans::Error;
use crate::record::{self, Format, Report, references, unzigzag};
use crate::tables::bc1::{QUADS, WINDOW};
use std::sync::LazyLock;

/// Widths of the endpoint fields R0 G0 B0 R1 G1 B1.
pub const FIELD_BITS: [u8; 6] = [5, 6, 5, 5, 6, 5];
/// Bytes of one BC1 block.
pub const BLOCK_BYTES: usize = 8;
const CODE_TO_LINE: [u32; 4] = [0, 3, 1, 2];
const LINE_TO_CODE: [u32; 4] = [0, 2, 3, 1];

/// Index-word bits each codebook symbol sets, per quadrant. Pixel (x, y) is
/// at bits `2 * (4 * y + x)`.
static QUAD_BITS: LazyLock<[[u32; 256]; 4]> = LazyLock::new(|| {
    let mut bits = [[0u32; 256]; 4];
    for (quadrant, (ox, oy)) in [(0, 0), (2, 0), (0, 2), (2, 2)].into_iter().enumerate() {
        for (symbol, codes) in QUADS.iter().enumerate() {
            for (k, (dx, dy)) in [(0, 0), (1, 0), (0, 1), (1, 1)].into_iter().enumerate() {
                bits[quadrant][symbol] |=
                    u32::from(codes[k] & 3) << (2 * (4 * (oy + dy) + ox + dx));
            }
        }
    }
    bits
});

fn map(word: u32, table: &[u32; 4]) -> u32 {
    (0..16).fold(0, |w, p| {
        w | table[(word >> (2 * p) & 3) as usize] << (2 * p)
    })
}

fn add_lines(a: u32, b: u32) -> u32 {
    (0..16).fold(0, |w, p| {
        w | ((a >> (2 * p)).wrapping_add(b >> (2 * p)) & 3) << (2 * p)
    })
}

/// Decodes a BCPack BC1 stream into `out`, which holds `out.len() / 8`
/// blocks in stream order. Only zero bytes may follow the stream.
pub fn decode(data: &[u8], out: &mut [u8]) -> Result<Report, Error> {
    record::decode(Format::Bc1, data, out)
}

/// Rebuilds BC1 blocks from the 12 decoded lanes.
pub fn assemble(lanes: &Symbols, out: &mut [u8]) -> Report {
    let quads = &*QUAD_BITS;
    let mut fields = [[0u8; 6]; 256];
    let mut lines = [0u32; 256];
    let mut report = Report::default();
    for (i, block) in out.chunks_exact_mut(BLOCK_BYTES).enumerate() {
        let (d, x) = references(lanes.lane(0)[i] as u8, lanes.lane(1)[i] as u8);
        let prediction = if d == 0 {
            [0; 6]
        } else if d <= i {
            fields[(i - d) & 255]
        } else {
            report.dictionary_references += 1;
            WINDOW[(i + 256 - d) % 256]
        };
        let mut e = [0u8; 6];
        for k in 0..6 {
            let mask = (1i32 << FIELD_BITS[k]) - 1;
            e[k] = ((i32::from(prediction[k]) + unzigzag(lanes.lane(2 + k)[i])) & mask) as u8;
        }
        fields[i & 255] = e;
        let mut residual = 0u32;
        for (q, bits) in quads.iter().enumerate() {
            residual |= bits[usize::from(lanes.lane(8 + q)[i] & 0xff)];
        }
        let reference = if x == 0 {
            0
        } else if x <= i {
            lines[(i - x) & 255]
        } else {
            report.unknown_references += 1;
            0
        };
        let line = add_lines(map(residual, &CODE_TO_LINE), reference);
        lines[i & 255] = line;
        let c0 = u16::from(e[0]) << 11 | u16::from(e[1]) << 5 | u16::from(e[2]);
        let c1 = u16::from(e[3]) << 11 | u16::from(e[4]) << 5 | u16::from(e[5]);
        block[0..2].copy_from_slice(&c0.to_le_bytes());
        block[2..4].copy_from_slice(&c1.to_le_bytes());
        block[4..8].copy_from_slice(&map(line, &LINE_TO_CODE).to_le_bytes());
    }
    report
}

#[cfg(test)]
#[path = "tests/bc1.rs"]
mod tests;
