// bcpack-rs by Ahmet Dedeler (https://github.com/Ahmet-Dedeler, https://ahmetdedeler.com)
// Source: https://github.com/Ahmet-Dedeler/bcpack-rs
// SPDX-License-Identifier: MIT OR Apache-2.0
// If you copy, vendor or port this file, keep this header and credit bcpack-rs in your README.
//
// Ported for TuxBox from src/bc7.rs of bcpack-rs
// 2d6333b03af4eaa33b2cffe9a8ab0439424955dd, used under Apache-2.0.
// Changes: tables are constants, no tracking of unknown table values.
//! Block rebuild for BC7: 16 lanes, two symbols (E, O) per lane and block.
//!
//! | Lane | Content |
//! | --- | --- |
//! | 0, 1 | the references ([`crate::record`]) |
//! | 2-5 | subset 0 endpoint residuals R, G, B, A (E = endpoint 0, O = endpoint 1) |
//! | 6-9 | subset 1 R, G, B, A; in modes 4 and 5 lanes 6-8 are rows 0-2 of the second index set |
//! | 10 | subset 2 R; in modes 4 and 5 row 3 of the second index set |
//! | 5, 9 | in three-subset modes: subset 2 G and B instead of alpha |
//! | 11-14 | rows 0-3 of the first index set |
//! | 15 | control code: mode, partition, rotation, index selection |
//!
//! A block's state is its subset count, its endpoints as 8-bit values aligned
//! to the top bit (per subset R0 R1 G0 G1 B0 B1 A0 A1, in the channel order
//! after undoing the rotation) and two sets of 16 indices as 4-bit values
//! aligned to the top bit. A field of `p` bits is the reference's top `p`
//! bits plus the residual, modulo `2^p`. With p-bits, red is one bit wider
//! and holds them; a rotation swaps alpha's width with the rotated channel's.
//! A subset the reference lacks is predicted from its subset 0. An index
//! symbol holds two pixels' zigzag residuals on its even and odd bits; E is
//! columns 0-1 of the row and O columns 2-3, the odd bits are the left pixel
//! in rows 0-2 and the right pixel in row 3.
use crate::lanes::Symbols;
use crate::record::{Report, Source, deinterleave, interleave, references, residual, source};
use crate::tables::bc7::{CONTROLS, DICTIONARY};
use std::sync::LazyLock;

pub const SUBSETS: [u8; 8] = [3, 2, 3, 2, 1, 1, 1, 2];
/// Endpoint bits of R, G, B, A per mode, without p-bits.
pub const CHANNEL_BITS: [[u8; 4]; 8] = [
    [4, 4, 4, 0],
    [6, 6, 6, 0],
    [5, 5, 5, 0],
    [7, 7, 7, 0],
    [5, 5, 5, 6],
    [7, 7, 7, 8],
    [7, 7, 7, 7],
    [5, 5, 5, 5],
];
/// Index bits of the first and second set per mode.
pub const INDEX_BITS: [[u8; 2]; 8] = [
    [3, 0],
    [3, 0],
    [2, 0],
    [2, 0],
    [3, 2],
    [2, 2],
    [4, 0],
    [2, 0],
];
pub const PARTITION_BITS: [u8; 8] = [4, 6, 6, 6, 0, 0, 0, 6];
/// Anchor of the second subset in two-subset partitions (BC7 specification).
const ANCHOR_2: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 2, 8, 2, 2, 8, 8, 15, 2, 8,
    2, 2, 8, 8, 2, 2, 15, 15, 6, 8, 2, 8, 15, 15, 2, 8, 2, 2, 2, 15, 15, 6, 6, 2, 6, 8, 15, 15, 2,
    2, 15, 15, 15, 15, 15, 2, 2, 15,
];
/// Anchors of the second and third subsets in three-subset partitions.
const ANCHOR_3A: [u8; 64] = [
    3, 3, 15, 15, 8, 3, 15, 15, 8, 8, 6, 6, 6, 5, 3, 3, 3, 3, 8, 15, 3, 3, 6, 10, 5, 8, 8, 6, 8, 5,
    15, 15, 8, 15, 3, 5, 6, 10, 8, 15, 15, 3, 15, 5, 15, 15, 15, 15, 3, 15, 5, 5, 5, 8, 5, 10, 5,
    10, 8, 13, 15, 12, 3, 3,
];
const ANCHOR_3B: [u8; 64] = [
    15, 8, 8, 3, 15, 15, 3, 8, 15, 15, 15, 15, 15, 15, 15, 8, 15, 8, 15, 3, 15, 8, 15, 8, 3, 15, 6,
    10, 15, 15, 10, 8, 15, 3, 15, 10, 10, 8, 9, 10, 6, 15, 8, 15, 3, 6, 6, 8, 15, 3, 15, 15, 15,
    15, 15, 15, 15, 15, 15, 15, 3, 15, 15, 8,
];
/// The lanes of the four rows of each index set.
const INDEX_LANES: [[usize; 4]; 2] = [[11, 12, 13, 14], [6, 7, 8, 10]];

/// What a control code selects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Control {
    pub mode: u8,
    pub partition: u8,
    pub rotation: u8,
    pub selection: u8,
}

impl Control {
    pub fn subsets(self) -> usize {
        usize::from(SUBSETS[usize::from(self.mode)])
    }

    fn has_pbits(self) -> bool {
        matches!(self.mode, 0 | 1 | 3 | 6 | 7)
    }

    /// Bits of the state's R, G, B and A fields.
    pub fn precision(self) -> [u8; 4] {
        let mut p = CHANNEL_BITS[usize::from(self.mode)];
        if self.rotation != 0 {
            p.swap(3, usize::from(self.rotation) - 1);
        } else if self.has_pbits() {
            p[0] += 1;
        }
        p
    }

    fn anchors(self) -> u16 {
        let part = usize::from(self.partition & 63);
        match self.subsets() {
            2 => 1 | 1 << ANCHOR_2[part],
            3 => 1 | 1 << ANCHOR_3A[part] | 1 << ANCHOR_3B[part],
            _ => 1,
        }
    }
}

/// Lane-15 symbols E and O to the control code.
pub fn control_code(e: u16, o: u16) -> usize {
    usize::from(o & 15) << 5 | usize::from(e & 31)
}

/// Control code to control, `None` for codes that are not a BC7 header.
pub static CONTROL: LazyLock<[Option<Control>; 512]> = LazyLock::new(|| {
    let mut table = [None; 512];
    for c in &CONTROLS {
        let control = Control {
            mode: c[2],
            partition: c[3],
            rotation: c[4],
            selection: c[5],
        };
        table[control_code(u16::from(c[0]), u16::from(c[1]))] = Some(control);
    }
    table
});

/// One block's state, or a dictionary entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    pub subsets: u8,
    pub ep: [[u8; 8]; 3],
    pub ix: [[u8; 16]; 2],
}

fn dictionary(k: usize) -> State {
    let (subsets, ep, ix) = &DICTIONARY[k];
    let mut st = State {
        subsets: *subsets,
        ..Default::default()
    };
    for (j, &v) in ep.iter().enumerate() {
        st.ep[j / 8][j % 8] = v;
    }
    for (j, &v) in ix.iter().enumerate() {
        st.ix[j / 16][j % 16] = v;
    }
    st
}

/// The lane with the residuals of subset `row`, channel `ch` (R G B A).
fn endpoint_lane(subsets: usize, row: usize, ch: usize) -> Option<usize> {
    match (row, ch) {
        (0, 3) | (1, 3) if subsets == 3 => None,
        (0, c) => Some(2 + c),
        (1, c) => Some(6 + c),
        (2, 0) => Some(10),
        (2, 1) if subsets == 3 => Some(5),
        (2, 2) if subsets == 3 => Some(9),
        _ => None,
    }
}

/// The residual codes (left, right) of an index symbol in row `row`.
pub fn index_pair(symbol: u16, row: usize) -> (u16, u16) {
    let (even, odd) = deinterleave(symbol as u8);
    if row < 3 { (odd, even) } else { (even, odd) }
}

/// The index symbol holding residual codes `left` and `right` in row `row`.
pub fn index_symbol(left: u16, right: u16, row: usize) -> u16 {
    u16::from(if row < 3 {
        interleave(right, left)
    } else {
        interleave(left, right)
    })
}

struct Bits {
    v: u128,
    at: u32,
}

impl Bits {
    fn put(&mut self, value: u32, n: u32) {
        if n > 0 && self.at < 128 {
            self.v |= (u128::from(value) & ((1u128 << n) - 1)) << self.at;
        }
        self.at += n;
    }
}

/// The order of the state's channels in the block.
fn channel_order(rotation: u8) -> [usize; 4] {
    let mut o = [0, 1, 2, 3];
    if rotation != 0 {
        o.swap(3, usize::from(rotation) - 1);
    }
    o
}

/// A state as BC7 block bytes. Anchor indices lose their top bit.
pub fn pack(c: Control, st: &State, out: &mut [u8]) {
    let mode = u32::from(c.mode);
    let subsets = c.subsets();
    let prec = c.precision();
    let mut w = Bits { v: 0, at: 0 };
    w.put(1 << mode, mode + 1);
    w.put(
        u32::from(c.partition),
        u32::from(PARTITION_BITS[usize::from(c.mode)]),
    );
    if mode == 4 || mode == 5 {
        w.put(u32::from(c.rotation), 2);
    }
    if mode == 4 {
        w.put(u32::from(c.selection), 1);
    }
    let field = |row: usize, col: usize, p: u8| u32::from(st.ep[row][col] >> (8 - p));
    for ch in channel_order(c.rotation) {
        let p = prec[ch];
        if p == 0 {
            continue;
        }
        // Red's lowest bit is the p-bit, written after the colours.
        let drop = u32::from(ch == 0 && c.has_pbits());
        for e in 0..2 * subsets {
            w.put(field(e / 2, 2 * ch + e % 2, p) >> drop, u32::from(p) - drop);
        }
    }
    if mode == 1 {
        for row in 0..subsets {
            w.put(field(row, 0, prec[0]) & 1, 1);
        }
    } else if c.has_pbits() {
        for e in 0..2 * subsets {
            w.put(field(e / 2, e % 2, prec[0]) & 1, 1);
        }
    }
    let anchors = c.anchors();
    let bits = INDEX_BITS[usize::from(c.mode)];
    let mut set = |s: usize| {
        let b = u32::from(bits[s]);
        for (p, &ix) in st.ix[s].iter().enumerate() {
            w.put(u32::from(ix) >> (4 - b), b - u32::from(anchors >> p & 1));
        }
    };
    match mode {
        4 => {
            set(1);
            set(0);
        }
        5 => {
            set(0);
            set(1);
        }
        _ => set(0),
    }
    out[..16].copy_from_slice(&w.v.to_le_bytes());
}

/// The new state of block `i` from its residuals and the two references.
fn apply(c: Control, lanes: &Symbols, i: usize, er: &State, xr: &State) -> State {
    let subsets = c.subsets();
    let mut st = State {
        subsets: subsets as u8,
        ..Default::default()
    };
    let common = subsets.min(usize::from(er.subsets));
    for (ch, &p) in c.precision().iter().enumerate() {
        if p == 0 {
            continue;
        }
        for row in 0..subsets {
            let from = if row < common { row } else { 0 };
            let lane = endpoint_lane(subsets, row, ch);
            for e in 0..2 {
                let col = 2 * ch + e;
                let res = lane.map_or(0, |l| residual(lanes.lane(l)[2 * i + e], u32::from(p)));
                let base = er.ep[from][col] >> (8 - p);
                st.ep[row][col] = (base.wrapping_add(res) & (0xffu16 >> (8 - p)) as u8) << (8 - p);
            }
        }
    }
    let bits = INDEX_BITS[usize::from(c.mode)];
    for set in 0..2 {
        let b = bits[set];
        if b == 0 {
            continue;
        }
        for (row, &lane) in INDEX_LANES[set].iter().enumerate() {
            for half in 0..2 {
                let (l, r) = index_pair(lanes.lane(lane)[2 * i + half], row);
                for (h, code) in [l, r].into_iter().enumerate() {
                    let p = 4 * row + 2 * half + h;
                    let base = xr.ix[set][p] >> (4 - b);
                    st.ix[set][p] = (base.wrapping_add(residual(code, u32::from(b)))
                        & (0xf >> (4 - b)))
                        << (4 - b);
                }
            }
        }
    }
    st
}

/// Rebuilds BC7 blocks from the decoded lanes.
pub fn assemble(lanes: &Symbols, out: &mut [u8]) -> Report {
    let controls = &*CONTROL;
    let mut history = [State::default(); 256];
    let mut report = Report::default();
    let (l0, l1, l15) = (lanes.lane(0), lanes.lane(1), lanes.lane(15));
    for (i, block) in out.chunks_exact_mut(16).enumerate() {
        let (er, xr) = references(
            interleave(l0[2 * i], l0[2 * i + 1]),
            interleave(l1[2 * i], l1[2 * i + 1]),
        );
        let Some(c) = controls[control_code(l15[2 * i], l15[2 * i + 1])] else {
            report.unknown_controls += 1;
            block.fill(0);
            history[i & 255] = State::default();
            continue;
        };
        let get = |r: usize, report: &mut Report| match source(i, r) {
            Source::Zero => State::default(),
            Source::Block(j) => history[j & 255],
            Source::Dictionary(k) => {
                report.dictionary_references += 1;
                dictionary(k)
            }
        };
        let (e, x) = (get(er, &mut report), get(xr, &mut report));
        let st = apply(c, lanes, i, &e, &x);
        pack(c, &st, block);
        history[i & 255] = st;
    }
    report
}

#[cfg(test)]
#[path = "tests/bc7.rs"]
mod tests;
