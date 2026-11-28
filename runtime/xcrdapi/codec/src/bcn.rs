// bcpack-rs by Ahmet Dedeler (https://github.com/Ahmet-Dedeler, https://ahmetdedeler.com)
// Source: https://github.com/Ahmet-Dedeler/bcpack-rs
// SPDX-License-Identifier: MIT OR Apache-2.0
// If you copy, vendor or port this file, keep this header and credit bcpack-rs in your README.
//
// Ported for TuxBox from src/bcn.rs of bcpack-rs
// 2d6333b03af4eaa33b2cffe9a8ab0439424955dd, used under Apache-2.0.
// Changes: tables are constants, no tracking of unknown table values.
//! Block rebuild for BC3, BC4 and BC5.
//!
//! A block's state is its endpoint fields and per-pixel index *positions*:
//! where a pixel sits on the line from endpoint 0 to endpoint 1. For 3-bit
//! alpha indices position 0 is endpoint 0, 7 is endpoint 1 and 1-6 lie
//! between (BC4 codes 0, 2, 3, 4, 5, 6, 7, 1); for BC3's 2-bit colour indices
//! positions 0-3 are BC1 codes 0, 2, 3, 1. Alpha endpoints are held 128
//! below their stored value, so the zero state is a flat 128.
//!
//! The state is the reference state (endpoints of the endpoint reference,
//! positions of the index reference) plus the residuals in the lanes, each
//! modulo its width. Index residuals come as one symbol per pixel pair
//! (3-bit) or per 2x2 group (2-bit), turned into positions by a rank table.
//!
//! | Format | Lanes 0, 1 | Endpoints | Indices |
//! | --- | --- | --- | --- |
//! | BC4 | one symbol each | lane 2 = e0, lane 3 = e1 | lane 4 + k = pixels 2k, 2k + 1 |
//! | BC5 | E/O interleaved | lane 2 = red e0 (E), e1 (O); lane 3 = green | lanes 4-7 red, 8-11 green: E = pixels 4k, 4k + 1, O = 4k + 2, 4k + 3 |
//! | BC3 | E/O interleaved | lane 2 = alpha e0, e1; lanes 7-9 = R, G, B of colour 0 (E) and 1 (O) | lanes 3-6 alpha as BC5; lane 10 = groups TL (E), TR (O), lane 11 = BL, BR |
use crate::lanes::Symbols;
use crate::record::{Format, Report, Source, interleave, references, residual, source};
use crate::tables::{bc3, bc4, bc5};

/// Position on the alpha line to the BC4 code.
pub const ALPHA_CODE: [u8; 8] = [0, 2, 3, 4, 5, 6, 7, 1];
/// Position on the colour line to the BC1 code.
pub const COLOUR_CODE: [u8; 4] = [0, 2, 3, 1];

/// Endpoints (BC4 e0 e1; BC5 red e0 e1, green e0 e1; BC3 alpha e0 e1, then
/// R0 G0 B0 R1 G1 B1) and positions (BC4 16; BC5 red then green; BC3 alpha
/// then colour).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    pub ep: [u8; 8],
    pub ix: [u8; 32],
}

struct Layout {
    ep_bits: &'static [u8],
    alpha: usize,
    colour: usize,
    pairs: &'static [[u8; 2]; 64],
}

fn layout(format: Format) -> Layout {
    match format {
        Format::Bc3 => Layout {
            ep_bits: &[8, 8, 5, 6, 5, 5, 6, 5],
            alpha: 16,
            colour: 16,
            pairs: &bc3::PAIRS,
        },
        Format::Bc4 => Layout {
            ep_bits: &[8, 8],
            alpha: 16,
            colour: 0,
            pairs: &bc4::PAIRS,
        },
        _ => Layout {
            ep_bits: &[8, 8, 8, 8],
            alpha: 32,
            colour: 0,
            pairs: &bc5::PAIRS,
        },
    }
}

fn dictionary(format: Format, k: usize) -> State {
    let mut st = State::default();
    let (ep, ix): (&[u8], &[u8]) = match format {
        Format::Bc3 => (&bc3::DICTIONARY[k].0, &bc3::DICTIONARY[k].1),
        Format::Bc4 => (&bc4::DICTIONARY[k].0, &bc4::DICTIONARY[k].1),
        _ => (&bc5::DICTIONARY[k].0, &bc5::DICTIONARY[k].1),
    };
    st.ep[..ep.len()].copy_from_slice(ep);
    st.ix[..ix.len()].copy_from_slice(ix);
    st
}

/// The references and the residual state of block `i`.
fn record(format: Format, l: &Layout, lanes: &Symbols, i: usize) -> (usize, usize, State) {
    let mut st = State::default();
    let pair = |st: &mut State, base: usize, k: usize, symbol: u16| {
        let p = l.pairs[usize::from(symbol & 63)];
        st.ix[base + 2 * k] = p[0];
        st.ix[base + 2 * k + 1] = p[1];
    };
    if format == Format::Bc4 {
        let (e, x) = references(lanes.lane(0)[i] as u8, lanes.lane(1)[i] as u8);
        st.ep[0] = residual(lanes.lane(2)[i], 8);
        st.ep[1] = residual(lanes.lane(3)[i], 8);
        for k in 0..8 {
            pair(&mut st, 0, k, lanes.lane(4 + k)[i]);
        }
        return (e, x, st);
    }
    let eo = |lane: usize| (lanes.lane(lane)[2 * i], lanes.lane(lane)[2 * i + 1]);
    let ((d0, d1), (x0, x1)) = (eo(0), eo(1));
    let (e, x) = references(interleave(d0, d1), interleave(x0, x1));
    if format == Format::Bc5 {
        for c in 0..2 {
            let (a, b) = eo(2 + c);
            st.ep[2 * c] = residual(a, 8);
            st.ep[2 * c + 1] = residual(b, 8);
            for lane in 0..4 {
                let (a, b) = eo(4 + 4 * c + lane);
                pair(&mut st, 16 * c, 2 * lane, a);
                pair(&mut st, 16 * c, 2 * lane + 1, b);
            }
        }
    } else {
        let (a, b) = eo(2);
        st.ep[0] = residual(a, 8);
        st.ep[1] = residual(b, 8);
        for lane in 0..4 {
            let (a, b) = eo(3 + lane);
            pair(&mut st, 0, 2 * lane, a);
            pair(&mut st, 0, 2 * lane + 1, b);
        }
        for c in 0..3 {
            let (a, b) = eo(7 + c);
            st.ep[2 + c] = residual(a, u32::from(l.ep_bits[2 + c]));
            st.ep[5 + c] = residual(b, u32::from(l.ep_bits[5 + c]));
        }
        let ((tl, tr), (bl, br)) = (eo(10), eo(11));
        for (q, symbol) in [tl, tr, bl, br].into_iter().enumerate() {
            let quad = bc3::QUADS[usize::from(symbol & 255)];
            let (ox, oy) = ((q & 1) * 2, (q >> 1) * 2);
            for (k, (dx, dy)) in [(0, 0), (1, 0), (0, 1), (1, 1)].into_iter().enumerate() {
                st.ix[16 + 4 * (oy + dy) + ox + dx] = quad[k];
            }
        }
    }
    (e, x, st)
}

fn pack_alpha(e0: u8, e1: u8, ix: &[u8], out: &mut [u8]) {
    let mut v = u64::from(e0 ^ 0x80) | u64::from(e1 ^ 0x80) << 8;
    for (p, &pos) in ix.iter().take(16).enumerate() {
        v |= u64::from(ALPHA_CODE[usize::from(pos & 7)]) << (16 + 3 * p);
    }
    out[..8].copy_from_slice(&v.to_le_bytes());
}

fn pack_colour(ep: &[u8], ix: &[u8], out: &mut [u8]) {
    let c0 = u16::from(ep[0]) << 11 | u16::from(ep[1]) << 5 | u16::from(ep[2]);
    let c1 = u16::from(ep[3]) << 11 | u16::from(ep[4]) << 5 | u16::from(ep[5]);
    let mut w = 0u32;
    for (p, &pos) in ix.iter().take(16).enumerate() {
        w |= u32::from(COLOUR_CODE[usize::from(pos & 3)]) << (2 * p);
    }
    out[0..2].copy_from_slice(&c0.to_le_bytes());
    out[2..4].copy_from_slice(&c1.to_le_bytes());
    out[4..8].copy_from_slice(&w.to_le_bytes());
}

/// A state as the format's block bytes.
pub fn pack(format: Format, st: &State, out: &mut [u8]) {
    match format {
        Format::Bc4 => pack_alpha(st.ep[0], st.ep[1], &st.ix[..16], out),
        Format::Bc5 => {
            pack_alpha(st.ep[0], st.ep[1], &st.ix[..16], &mut out[..8]);
            pack_alpha(st.ep[2], st.ep[3], &st.ix[16..], &mut out[8..16]);
        }
        _ => {
            pack_alpha(st.ep[0], st.ep[1], &st.ix[..16], &mut out[..8]);
            pack_colour(&st.ep[2..8], &st.ix[16..], &mut out[8..16]);
        }
    }
}

/// Rebuilds BC3, BC4 or BC5 blocks from the decoded lanes.
pub fn assemble(format: Format, lanes: &Symbols, out: &mut [u8]) -> Report {
    let l = layout(format);
    let size = format.block_bytes();
    let mut history = [State::default(); 256];
    let mut report = Report::default();
    for (i, block) in out.chunks_exact_mut(size).enumerate() {
        let (er, xr, mut st) = record(format, &l, lanes, i);
        let get = |r: usize, report: &mut Report| match source(i, r) {
            Source::Zero => State::default(),
            Source::Block(j) => history[j & 255],
            Source::Dictionary(k) => {
                report.dictionary_references += 1;
                dictionary(format, k)
            }
        };
        let e = get(er, &mut report);
        let x = get(xr, &mut report);
        for (k, &bits) in l.ep_bits.iter().enumerate() {
            st.ep[k] = st.ep[k].wrapping_add(e.ep[k]) & ((1u16 << bits) - 1) as u8;
        }
        for p in 0..l.alpha + l.colour {
            let mask = if p < l.alpha { 7 } else { 3 };
            st.ix[p] = (st.ix[p] + x.ix[p]) & mask;
        }
        pack(format, &st, block);
        history[i & 255] = st;
    }
    report
}

#[cfg(test)]
#[path = "tests/bcn.rs"]
mod tests;
