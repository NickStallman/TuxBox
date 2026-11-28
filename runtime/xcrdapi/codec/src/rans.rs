//! The rANS fragment decoder of Microsoft's public hardware sample
//! (github.com/microsoft/silicon-rans, `mdu_minirans.v`, MIT licence),
//! modelled in software from its description and its state machine. No code
//! or table of the sample is copied here: callers supply the models.
//!
//! A lane is a sequence of fragments. Each fragment has a 2 or 3 byte header
//! and then its payload bytes:
//!
//! - header byte 0: bits 7-6 narrow the alphabet by that many high bits for
//!   this fragment, bit 5 selects a 3-byte header, bit 4 resets the rANS state
//!   once this fragment is decoded, bits 3-0 choose one of 16 models;
//! - header byte 1 (and 2): the payload length, `byte1 + 1`, or with a 3-byte
//!   header `(byte2 << 8 | byte1) + 257`.
//!
//! The state is 32 bits, probabilities are 16 bits and input is consumed a
//! byte at a time while the state is below 2^24. A symbol is decoded only
//! while the state is above 2^24. A fragment's last payload byte leaves the
//! state at exactly 2^24 at the end of a stream, or at 2^16 or 2^8 when the
//! encoder dropped trailing zero bytes; otherwise the remaining symbols are
//! decoded without input until the state falls to 2^24 or below and the next
//! header is read with the state carried over.
//!
//! A model gives each symbol a frequency out of 65536. Symbols are grouped by
//! their highest set bit: segment 0 is symbol 0, segment `s` holds symbols
//! `2^(s-1) .. 2^s`, all with the segment's frequency. Symbol 0 starts at
//! cumulative frequency 1 with one less than its frequency, so a low state
//! word of 0 is never a symbol.

/// Widest symbol the decoder produces.
pub const MAX_SYMBOL_BITS: u8 = 9;
const SEGMENTS: usize = MAX_SYMBOL_BITS as usize + 1;
pub(crate) const LOWER: u32 = 1 << 24;
const TOTAL: u32 = 1 << 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The model's frequencies are zero where used or do not total 65536.
    BadModel,
    /// The alphabet is wider than nine bits or narrowed below one bit.
    BadAlphabet,
    /// A fragment selects a model the caller did not supply.
    MissingModel,
    /// The input ended inside a header or a payload.
    Truncated,
    /// The state can neither take input nor give a symbol: corrupt input,
    /// or more symbols in the stream than requested.
    Stalled,
    /// A low state word of 0, which no encoder produces.
    BadState,
    /// A lane finished in a state other than 2^24, 2^16 or 2^8.
    EndState,
    /// Bytes other than zero padding follow the last byte the lanes read.
    TrailingData,
}

/// One probability model for an alphabet of `bits` bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    bits: u8,
    base: [u32; SEGMENTS],
    freq: [u32; SEGMENTS],
}

impl Model {
    /// `freq[s]` is the frequency of each symbol of segment `s`; a model for a
    /// `bits`-bit alphabet has `bits + 1` segments.
    pub fn new(freq: &[u16]) -> Result<Model, Error> {
        if freq.len() < 2 || freq.len() > SEGMENTS {
            return Err(Error::BadAlphabet);
        }
        let mut model = Model {
            bits: (freq.len() - 1) as u8,
            base: [0; SEGMENTS],
            freq: [0; SEGMENTS],
        };
        let mut at = 0u32;
        for (s, &f) in freq.iter().enumerate() {
            // Symbol 0 needs a frequency of 2: one slot is given up.
            if f == 0 || (s == 0 && f < 2) {
                return Err(Error::BadModel);
            }
            model.base[s] = at;
            model.freq[s] = u32::from(f);
            at += u32::from(f) * segment_size(s);
        }
        if at != TOTAL {
            return Err(Error::BadModel);
        }
        Ok(model)
    }

    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// Cumulative start and frequency of `symbol`, as the decoder subtracts
    /// them: what an encoder must use.
    pub fn interval(&self, symbol: u16) -> Option<(u32, u32)> {
        let s = segment_of(symbol);
        if s > usize::from(self.bits) {
            return None;
        }
        let start = self.base[s] + self.freq[s] * (u32::from(symbol) - segment_start(s));
        Some(if start == 0 {
            (1, self.freq[s] - 1)
        } else {
            (start, self.freq[s])
        })
    }

    /// Symbol whose slot holds the low state word `slot`, decoding at most
    /// `bits` bits.
    pub(crate) fn symbol(&self, slot: u32, bits: u8) -> u16 {
        let mut s = usize::from(bits);
        while s > 0 && slot < self.base[s] {
            s -= 1;
        }
        (segment_start(s) + (slot - self.base[s]) / self.freq[s]) as u16
    }
}

impl Model {
    /// The state after decoding `symbol` from `state`, as the hardware
    /// computes it (modulo 2^32).
    pub(crate) fn advance(&self, state: u32, symbol: u16) -> u32 {
        let (start, freq) = self.interval(symbol).unwrap_or((0, 0));
        freq.wrapping_mul(state >> 16)
            .wrapping_add(state & (TOTAL - 1))
            .wrapping_sub(start)
    }
}

fn segment_size(s: usize) -> u32 {
    if s == 0 { 1 } else { 1 << (s - 1) }
}

fn segment_start(s: usize) -> u32 {
    if s == 0 { 0 } else { 1 << (s - 1) }
}

fn segment_of(symbol: u16) -> usize {
    (u16::BITS - symbol.leading_zeros()) as usize
}

/// The 16 models a fragment header chooses from, for each alphabet width.
#[derive(Debug, Clone, Default)]
pub struct Models {
    by_width: Vec<[Option<Model>; 16]>,
}

impl Models {
    pub fn insert(&mut self, index: u8, model: Model) {
        let bits = usize::from(model.bits);
        if self.by_width.len() <= bits {
            self.by_width.resize_with(bits + 1, Default::default);
        }
        self.by_width[bits][usize::from(index & 15)] = Some(model);
    }

    pub(crate) fn get(&self, bits: u8, index: u8) -> Option<&Model> {
        self.by_width
            .get(usize::from(bits))?
            .get(usize::from(index))?
            .as_ref()
    }
}

/// Decodes one lane: `out.len()` symbols of at most `bits` bits each, from
/// fragments read through `input`. Returns the bytes consumed.
pub fn decode(
    input: &mut impl Iterator<Item = u8>,
    bits: u8,
    models: &Models,
    out: &mut [u16],
) -> Result<usize, Error> {
    if !(1..=MAX_SYMBOL_BITS).contains(&bits) {
        return Err(Error::BadAlphabet);
    }
    let mut used = 0usize;
    let mut next = || {
        used += 1;
        input.next().ok_or(Error::Truncated)
    };
    let mut state = 0u32;
    let mut written = 0usize;
    while written < out.len() {
        let header = next()?;
        let narrow = header >> 6;
        let width = bits
            .checked_sub(narrow)
            .filter(|&w| w >= 1)
            .ok_or(Error::BadAlphabet)?;
        let model = models.get(width, header & 15).ok_or(Error::MissingModel)?;
        let low = usize::from(next()?);
        let mut remaining = if header & 0x20 != 0 {
            (usize::from(next()?) << 8 | low) + 257
        } else {
            low + 1
        };
        // Payload: one byte in while the state is low, one symbol out while
        // it is high, as the hardware's processing state does each cycle.
        while remaining > 0 {
            let fed = state < LOWER;
            if fed {
                state = state << 8 | u32::from(next()?);
                remaining -= 1;
            }
            if state > LOWER && written < out.len() {
                state = step(model, width, state, &mut out[written])?;
                written += 1;
            } else if !fed {
                return Err(Error::Stalled);
            }
        }
        // Drain: the payload is spent, decode what the state still holds.
        while state > LOWER {
            if written == out.len() {
                return Err(Error::Stalled);
            }
            state = step(model, width, state, &mut out[written])?;
            written += 1;
        }
        if header & 0x10 != 0 {
            state = 0;
        }
    }
    Ok(used)
}

fn step(model: &Model, bits: u8, state: u32, out: &mut u16) -> Result<u32, Error> {
    let slot = state & (TOTAL - 1);
    if slot == 0 {
        return Err(Error::BadState);
    }
    let symbol = model.symbol(slot, bits);
    let (start, freq) = model.interval(symbol).ok_or(Error::BadState)?;
    *out = symbol;
    Ok(freq * (state >> 16) + slot - start)
}

#[cfg(test)]
#[path = "tests/rans.rs"]
mod tests;
