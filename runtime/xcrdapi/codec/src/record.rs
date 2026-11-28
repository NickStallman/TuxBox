// bcpack-rs by Ahmet Dedeler (https://github.com/Ahmet-Dedeler, https://ahmetdedeler.com)
// Source: https://github.com/Ahmet-Dedeler/bcpack-rs
// SPDX-License-Identifier: MIT OR Apache-2.0
// If you copy, vendor or port this file, keep this header and credit bcpack-rs in your README.
//
// Ported for TuxBox from src/format.rs of bcpack-rs
// 2d6333b03af4eaa33b2cffe9a8ab0439424955dd, used under Apache-2.0.
//! What the BCPack formats share: lane layouts and the two references of a
//! block record.
//!
//! Every lane gives one symbol per 8 unpacked bytes, so a 16-byte block has
//! two symbols per lane, E (first) and O (second). A record starts with an
//! index reference `x` (lane 1; for 16-byte blocks the E and O nibbles
//! interleaved bit by bit, E on the even bits) and an endpoint reference
//! `(x + unzigzag(lane 0)) mod 256`, lane 0 formed the same way. A reference
//! `r` of block `i` is 0 for the zero state, `r <= i` for block `i - r`, and
//! otherwise entry `r - i - 1` of the format's 255-entry dictionary.
use crate::lanes::{self, Symbols};
use crate::models;
use crate::rans::{Error, Models};
use std::sync::LazyLock;

pub(crate) static MODELS: LazyLock<Models> = LazyLock::new(models::silicon_rans);

/// The BCPack formats this crate decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Bc1,
    Bc3,
    Bc4,
    Bc5,
    Bc7,
}

impl Format {
    /// From `DSTORAGE_BCPACK_MODE`; BC2 and BC6H are not decoded.
    pub fn from_mode(mode: u64) -> Option<Format> {
        Some(match mode {
            1 => Format::Bc1,
            3 => Format::Bc3,
            4 => Format::Bc4,
            5 => Format::Bc5,
            7 => Format::Bc7,
            _ => return None,
        })
    }

    pub fn block_bytes(self) -> usize {
        match self {
            Format::Bc1 | Format::Bc4 => 8,
            _ => 16,
        }
    }

    /// Symbol widths of the format's lanes.
    pub fn widths(self) -> &'static [u8] {
        match self {
            Format::Bc1 => &[8, 8, 5, 6, 5, 5, 6, 5, 8, 8, 8, 8],
            Format::Bc3 => &[4, 4, 8, 6, 6, 6, 6, 5, 6, 5, 8, 8],
            Format::Bc4 => &[8, 8, 8, 8, 6, 6, 6, 6, 6, 6, 6, 6],
            Format::Bc5 => &[4, 4, 8, 8, 6, 6, 6, 6, 6, 6, 6, 6],
            Format::Bc7 => &[4, 4, 8, 8, 8, 8, 8, 7, 7, 5, 6, 8, 8, 8, 8, 5],
        }
    }

    /// Decodes the entropy layer of a stream that unpacks to `bytes` bytes.
    pub fn symbols(self, data: &[u8], bytes: usize) -> Result<Symbols, Error> {
        if bytes == 0 || !bytes.is_multiple_of(self.block_bytes()) {
            return Err(Error::BadAlphabet);
        }
        lanes::decode_slice(data, self.widths(), bytes / 8, &MODELS)
    }
}

/// Two 4-bit symbols as one byte, `even` on the even bits.
pub fn interleave(even: u16, odd: u16) -> u8 {
    (0..4).fold(0, |b, k| {
        b | ((even >> k & 1) as u8) << (2 * k) | ((odd >> k & 1) as u8) << (2 * k + 1)
    })
}

/// The even-bit and odd-bit nibbles of a byte.
pub fn deinterleave(b: u8) -> (u16, u16) {
    (0..4).fold((0, 0), |(e, o), k| {
        (
            e | u16::from(b >> (2 * k) & 1) << k,
            o | u16::from(b >> (2 * k + 1) & 1) << k,
        )
    })
}

/// 0, 1, 2, 3, 4 to 0, -1, 1, -2, 2.
pub fn unzigzag(v: u16) -> i32 {
    let v = i32::from(v);
    if v & 1 != 0 { -((v + 1) >> 1) } else { v >> 1 }
}

/// A zigzag residual as a value modulo `2^bits`.
pub fn residual(v: u16, bits: u32) -> u8 {
    (unzigzag(v) & ((1 << bits) - 1)) as u8
}

/// The endpoint and index references from the lane-0 and lane-1 bytes.
pub fn references(d: u8, x: u8) -> (usize, usize) {
    (
        ((i32::from(x) + unzigzag(u16::from(d))) & 0xff) as usize,
        usize::from(x),
    )
}

/// Where reference `r` of block `i` points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Zero,
    Block(usize),
    Dictionary(usize),
}

pub fn source(i: usize, r: usize) -> Source {
    if r == 0 {
        Source::Zero
    } else if r <= i {
        Source::Block(i - r)
    } else {
        Source::Dictionary(r - i - 1)
    }
}

/// What a decode relied on that the stream does not carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// References that read the format's dictionary.
    pub dictionary_references: usize,
    /// BC7 blocks whose control code is not a BC7 header (written as zeros).
    pub unknown_controls: usize,
    /// BC1 index references before the first block, which read zero: no
    /// table for them is known.
    pub unknown_references: usize,
}

/// Decodes a BCPack stream of `format` into `out`, whose length is the
/// unpacked size. Only zero bytes may follow the stream.
pub fn decode(format: Format, data: &[u8], out: &mut [u8]) -> Result<Report, Error> {
    let symbols = format.symbols(data, out.len())?;
    Ok(match format {
        Format::Bc1 => crate::bc1::assemble(&symbols, out),
        Format::Bc3 | Format::Bc4 | Format::Bc5 => crate::bcn::assemble(format, &symbols, out),
        Format::Bc7 => crate::bc7::assemble(&symbols, out),
    })
}
