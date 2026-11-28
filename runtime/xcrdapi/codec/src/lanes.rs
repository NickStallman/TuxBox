// bcpack-rs by Ahmet Dedeler (https://github.com/Ahmet-Dedeler, https://ahmetdedeler.com)
// Source: https://github.com/Ahmet-Dedeler/bcpack-rs
// SPDX-License-Identifier: MIT OR Apache-2.0
// If you copy, vendor or port this file, keep this header and credit bcpack-rs in your README.
//
// Ported for TuxBox from `decode_lanes` and `phase1` in src/rans.rs of
// bcpack-rs 5bf5cc56ed56f1ababf1f9592300bd182ff90bd8, used under Apache-2.0.
// Changes: TuxBox's model and error types, a per-lane byte source, the
// progress bound counted in consumed bytes, and no statistics.
//! Several rANS lanes sharing one byte stream, clock by clock.
//!
//! Each lane is the MDU decoder of Microsoft's silicon-rans reference (see
//! [`crate::rans`]), run in its two phases per clock with a one-byte input
//! buffer and a one-symbol output buffer. Two rules that are not in the
//! published Verilog were established by bcpack-rs on retail data:
//!
//! - the feeder: in phase 0 of each clock the lanes that want a byte are
//!   served from the stream in lane order, at most [`FEED_WINDOW`] bytes a
//!   clock, and a lane that is not served stalls for that clock;
//! - lockstep output: each lane has one output slot and hands a symbol over
//!   only while its slot is empty; the slots are released together once every
//!   lane still running holds a symbol.
//!
//! Which lane reads the next byte therefore depends only on the lane states.
use crate::rans::{Error, LOWER, MAX_SYMBOL_BITS, Model, Models};

/// Bytes the feeder hands out per clock (`MDU_RANS_FEED_WINDOW`).
pub const FEED_WINDOW: usize = 8;
/// Lanes one stream can carry here.
pub const MAX_LANES: usize = 16;

/// Where lane bytes come from. The stream decoder ignores `lane`; the test
/// encoder uses it to record the schedule.
pub trait Source {
    fn next(&mut self, lane: usize) -> Option<u8>;
}

impl<I: Iterator<Item = u8>> Source for I {
    fn next(&mut self, _lane: usize) -> Option<u8> {
        Iterator::next(self)
    }
}

/// Decoded symbols, lane by lane.
#[derive(Debug, Clone)]
pub struct Symbols {
    pub per_lane: usize,
    pub symbols: Vec<u16>,
    /// Bytes the lanes read from the source.
    pub consumed: usize,
}

impl Symbols {
    pub fn lane(&self, lane: usize) -> &[u16] {
        &self.symbols[lane * self.per_lane..(lane + 1) * self.per_lane]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Header0,
    Header1,
    Header2,
    Payload,
    Drain,
}

struct Lane<'m> {
    /// Symbol width less one (the Verilog's `alphabet_bits`).
    alphabet: u8,
    state: State,
    started: bool,
    done: bool,
    stalled: bool,
    input: u8,
    input_full: bool,
    symbol: u16,
    symbol_full: bool,
    input_left: u32,
    output_left: u32,
    p0: u32,
    p1: u32,
    long_header: bool,
    reset: bool,
    index: u8,
    width: u8,
    model: Option<&'m Model>,
    end_state: u32,
}

impl Lane<'_> {
    fn wants_byte(&self) -> bool {
        !self.input_full
            && match self.state {
                State::Header0 | State::Header1 | State::Header2 => true,
                State::Payload => self.input_left > 0,
                _ => false,
            }
    }

    fn advance(&self) -> u32 {
        self.model
            .map_or(self.p1, |m| m.advance(self.p1, self.symbol))
    }
}

/// Decodes `widths.len()` lanes of `per_lane` symbols each; `widths[i]` is
/// lane `i`'s symbol width in bits.
pub fn decode(
    source: &mut impl Source,
    widths: &[u8],
    per_lane: usize,
    models: &Models,
) -> Result<Symbols, Error> {
    let count = widths.len();
    if count == 0
        || count > MAX_LANES
        || widths.iter().any(|w| !(1..=MAX_SYMBOL_BITS).contains(w))
        || per_lane == 0
        || per_lane > (u32::MAX / 2) as usize
    {
        return Err(Error::BadAlphabet);
    }
    let total = count.checked_mul(per_lane).ok_or(Error::BadAlphabet)?;
    let mut symbols = Vec::new();
    symbols
        .try_reserve_exact(total)
        .map_err(|_| Error::BadAlphabet)?;
    symbols.resize(total, 0u16);
    let mut lanes: Vec<Lane> = widths
        .iter()
        .map(|&w| Lane {
            alphabet: w - 1,
            state: State::Idle,
            started: false,
            done: false,
            stalled: false,
            input: 0,
            input_full: false,
            symbol: 0,
            symbol_full: false,
            input_left: 0,
            output_left: 0,
            p0: 0,
            p1: 0,
            long_header: false,
            reset: false,
            index: 0,
            width: 1,
            model: None,
            end_state: 0,
        })
        .collect();
    let mut written = vec![0usize; count];
    let mut slots = 0u32;
    let mut consumed = 0usize;
    let mut exhausted = false;
    let mut cycle = 0u64;
    let mut progress = (usize::MAX, usize::MAX, u32::MAX);
    let mut progressed_at = 0u64;
    loop {
        // Phase 0: state update, feeder, hand-over to the output slots.
        let mut served = 0;
        for (i, lane) in lanes.iter_mut().enumerate() {
            let bit = 1u32 << i;
            let blocked = lane.symbol_full && slots & bit != 0;
            let wants = lane.wants_byte();
            let mut byte = None;
            if wants && !blocked && served < FEED_WINDOW && !exhausted {
                byte = source.next(i);
                exhausted = byte.is_none();
            }
            lane.stalled = (wants && byte.is_none()) || blocked;
            if lane.stalled {
                continue;
            }
            if matches!(lane.state, State::Payload | State::Drain) {
                lane.p0 = if lane.symbol_full {
                    lane.advance()
                } else {
                    lane.p1
                };
            }
            if let Some(b) = byte {
                lane.input = b;
                lane.input_full = true;
                consumed += 1;
                served += 1;
            }
            if lane.symbol_full {
                slots |= bit;
                symbols[i * per_lane + written[i]] = lane.symbol;
                written[i] += 1;
                lane.symbol_full = false;
            }
        }
        // Phase 1: headers, renormalisation and symbol decode.
        let mut running = 0u32;
        let mut finished = true;
        let mut states = 0u32;
        for (i, lane) in lanes.iter_mut().enumerate() {
            if !lane.stalled {
                phase1(lane, models, per_lane as u32)?;
            }
            let bit = 1u32 << i;
            if !(lane.done && !lane.symbol_full && slots & bit == 0) {
                running |= bit;
            }
            finished &= lane.done && !lane.symbol_full;
            states += lane.state as u32;
        }
        if slots != 0 && slots & running == running {
            slots = 0;
        }
        cycle += 1;
        if finished {
            if lanes
                .iter()
                .any(|l| !matches!(l.end_state, LOWER | 0x1_0000 | 0x100))
            {
                return Err(Error::EndState);
            }
            return Ok(Symbols {
                per_lane,
                symbols,
                consumed,
            });
        }
        let now = (consumed, written.iter().sum::<usize>(), states);
        if now != progress {
            progress = now;
            progressed_at = cycle;
        } else if cycle - progressed_at > 64 {
            return Err(if exhausted {
                Error::Truncated
            } else {
                Error::Stalled
            });
        }
    }
}

fn phase1<'m>(lane: &mut Lane<'m>, models: &'m Models, per_lane: u32) -> Result<(), Error> {
    match lane.state {
        State::Idle => {
            if !lane.started {
                lane.started = true;
                lane.state = State::Header0;
                lane.output_left = per_lane;
                lane.p0 = 0;
                lane.p1 = 0;
            }
        }
        State::Header0 => {
            if lane.input_full {
                let b = lane.input;
                lane.input_full = false;
                lane.input_left = 0;
                lane.state = State::Header1;
                lane.index = b & 15;
                lane.long_header = b & 0x20 != 0;
                lane.reset = b & 0x10 != 0;
                // The Verilog narrows in 4 bits and clamps to 9; above 8 is invalid.
                let narrowed = lane.alphabet.wrapping_sub(b >> 6) & 15;
                if narrowed > 8 {
                    return Err(Error::BadAlphabet);
                }
                lane.width = narrowed + 1;
            }
        }
        State::Header1 => {
            if lane.input_full {
                lane.input_full = false;
                lane.model = Some(
                    models
                        .get(lane.width, lane.index)
                        .ok_or(Error::MissingModel)?,
                );
                if lane.long_header {
                    lane.input_left = u32::from(lane.input);
                    lane.state = State::Header2;
                } else {
                    lane.input_left = u32::from(lane.input) + 1;
                    lane.state = State::Payload;
                }
            }
        }
        State::Header2 => {
            if lane.input_full {
                lane.input_full = false;
                lane.input_left = (u32::from(lane.input) << 8 | (lane.input_left & 0xff)) + 257;
                lane.state = State::Payload;
            }
        }
        state => {
            let model = lane.model.ok_or(Error::MissingModel)?;
            let feeds = state == State::Payload
                && lane.p0 < LOWER
                && lane.input_left > 0
                && lane.input_full;
            let x = if feeds {
                (lane.p0 & 0xff_ffff) << 8 | u32::from(lane.input)
            } else {
                lane.p0
            };
            let decodes = x > LOWER && lane.output_left > 0;
            if decodes {
                lane.output_left -= 1;
                lane.symbol = model.symbol(x & 0xffff, lane.width);
            }
            lane.p1 = x;
            lane.symbol_full = decodes;
            let end = |lane: &mut Lane| {
                lane.done = true;
                lane.state = State::Idle;
                lane.end_state = if decodes {
                    model.advance(x, lane.symbol)
                } else {
                    x
                };
            };
            if state == State::Payload {
                if feeds {
                    lane.input_left -= 1;
                    lane.input_full = false;
                }
                if lane.input_left == 0 {
                    if lane.output_left == 0 {
                        end(lane);
                    } else if x == LOWER {
                        lane.state = State::Header0;
                        if lane.reset {
                            lane.p1 = 0;
                        }
                    } else {
                        lane.state = State::Drain;
                    }
                }
            } else if x <= LOWER {
                if lane.output_left == 0 {
                    end(lane);
                } else {
                    lane.state = State::Header0;
                    if lane.reset {
                        lane.p0 = 0;
                        lane.p1 = 0;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Decodes lanes from a slice and checks that only zero padding follows the
/// bytes they read.
pub fn decode_slice(
    data: &[u8],
    widths: &[u8],
    per_lane: usize,
    models: &Models,
) -> Result<Symbols, Error> {
    let symbols = decode(&mut data.iter().copied(), widths, per_lane, models)?;
    if data[symbols.consumed..].iter().any(|&b| b != 0) {
        return Err(Error::TrailingData);
    }
    Ok(symbols)
}

#[cfg(test)]
#[path = "tests/lanes.rs"]
mod tests;
