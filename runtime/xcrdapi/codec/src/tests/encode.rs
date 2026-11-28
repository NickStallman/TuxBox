//! A test encoder for multi-lane streams. Each lane is split into fragments
//! that reset the state, each a reverse rANS stream from 2^24; the lanes are
//! interleaved by running the decoder on per-lane queues and recording which
//! lane takes each byte, which works because the feeder looks only at lane
//! states. (The same method bcpack-rs's src/encode.rs describes.)
use crate::lanes::{self, Source};
use crate::models;
use crate::rans::{LOWER, Model, Models};
use std::collections::VecDeque;
use std::sync::LazyLock;

pub static MODELS: LazyLock<Models> = LazyLock::new(models::silicon_rans);

pub struct Random(pub u64);
impl Random {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// One fragment's payload: the reverse rANS encoding of `symbols`.
pub fn payload(model: &Model, symbols: &[u16]) -> Vec<u8> {
    let mut x = u64::from(LOWER);
    let mut reversed = Vec::new();
    for &symbol in symbols.iter().rev() {
        let (start, freq) = model.interval(symbol).expect("symbol fits the model");
        let (start, freq) = (u64::from(start), u64::from(freq));
        while x >= freq << 16 {
            reversed.push(x as u8);
            x >>= 8;
        }
        x = ((x / freq) << 16) + x % freq + start;
    }
    for _ in 0..4 {
        reversed.push(x as u8);
        x >>= 8;
    }
    reversed.reverse();
    reversed
}

/// A lane's bytes: fragments of up to `longest` symbols with random models.
pub fn lane(width: u8, symbols: &[u16], longest: usize, random: &mut Random) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut at = 0;
    while at < symbols.len() {
        let n = (1 + random.below(longest as u64) as usize).min(symbols.len() - at);
        let index = random.below(16) as u8;
        let body = payload(MODELS.get(width, index).unwrap(), &symbols[at..at + n]);
        assert!(body.len() <= 65792);
        if body.len() <= 256 {
            bytes.extend([0x10 | index, (body.len() - 1) as u8]);
        } else {
            let n = body.len() - 257;
            bytes.extend([0x30 | index, n as u8, (n >> 8) as u8]);
        }
        bytes.extend(body);
        at += n;
    }
    bytes
}

struct Queues {
    lanes: Vec<VecDeque<u8>>,
    stream: Vec<u8>,
}

impl Source for Queues {
    fn next(&mut self, lane: usize) -> Option<u8> {
        let b = self.lanes[lane].pop_front()?;
        self.stream.push(b);
        Some(b)
    }
}

/// The single stream that feeds `symbols[lane]` to each lane.
pub fn stream(widths: &[u8], symbols: &[Vec<u16>], longest: usize, random: &mut Random) -> Vec<u8> {
    let mut queues = Queues {
        lanes: widths
            .iter()
            .zip(symbols)
            .map(|(&w, s)| lane(w, s, longest, random).into())
            .collect(),
        stream: Vec::new(),
    };
    let decoded = lanes::decode(&mut queues, widths, symbols[0].len(), &MODELS).unwrap();
    assert!(queues.lanes.iter().all(VecDeque::is_empty));
    for (i, s) in symbols.iter().enumerate() {
        assert_eq!(decoded.lane(i), &s[..]);
    }
    queues.stream
}

/// The residual code nearest zero that takes `from` to `to` modulo `2^bits`.
pub fn code(from: u8, to: u8, bits: u32) -> u16 {
    let size = 1i32 << bits;
    let mut r = (i32::from(to) - i32::from(from)).rem_euclid(size);
    if r >= size / 2 {
        r -= size;
    }
    if r >= 0 {
        2 * r as u16
    } else {
        (-2 * r - 1) as u16
    }
}

/// Random endpoint and index references for block `i`: zero, a block of the
/// chunk, or a dictionary entry.
pub fn references(i: usize, random: &mut Random) -> (usize, usize) {
    let mut one = || match random.below(3) {
        0 => 0,
        1 if i > 0 => 1 + random.below(i.min(255) as u64) as usize,
        _ => 1 + random.below(255) as usize,
    };
    (one(), one())
}
