use super::*;

/// Our own synthetic model for a `bits`-bit alphabet. `shape` 0-2 scales the
/// frequencies above symbol 0; symbol 0 takes what is left of 65536.
fn model(bits: u8, shape: u32) -> Model {
    let mut freq = vec![0u16; usize::from(bits) + 1];
    let mut used = 0u32;
    for (s, slot) in freq.iter_mut().enumerate().skip(1) {
        let f = ((4096u32 >> s) * (shape + 1)).max(1);
        *slot = f as u16;
        used += f * segment_size(s);
    }
    freq[0] = (TOTAL - used) as u16;
    Model::new(&freq).unwrap()
}

fn models() -> Models {
    let mut models = Models::default();
    for bits in 1..=MAX_SYMBOL_BITS {
        for shape in 0..3 {
            models.insert(shape as u8, model(bits, shape));
        }
    }
    models
}

struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Mostly small symbols, as residuals are, with every width present.
    fn symbol(&mut self, bits: u8) -> u16 {
        let width = (self.next() % u64::from(bits + 1)) as u32;
        (self.next() as u16) & ((1u32 << width) - 1) as u16
    }
}

/// Byte-wise rANS encoder for the decoder's conventions: the payload in
/// reading order, and how many of its last bytes encoding the final symbol
/// pushed (the only bytes an encoder may trim).
fn encode(model: &Model, symbols: &[u16]) -> (Vec<u8>, usize) {
    let mut x = u64::from(LOWER);
    let mut reversed = Vec::new();
    let mut tail = 0;
    for (i, &symbol) in symbols.iter().enumerate().rev() {
        let (start, freq) = model.interval(symbol).unwrap();
        let (start, freq) = (u64::from(start), u64::from(freq));
        while x >= freq << 16 {
            reversed.push(x as u8);
            x >>= 8;
        }
        if i + 1 == symbols.len() {
            tail = reversed.len();
        }
        x = ((x / freq) << 16) + x % freq + start;
    }
    for _ in 0..4 {
        reversed.push(x as u8);
        x >>= 8;
    }
    reversed.reverse();
    (reversed, tail)
}

/// A fragment header for `len` payload bytes.
fn header(index: u8, narrow: u8, reset: bool, len: usize) -> Vec<u8> {
    let flags = narrow << 6 | u8::from(reset) << 4 | index;
    if len <= 256 {
        vec![flags, (len - 1) as u8]
    } else {
        let n = len - 257;
        vec![flags | 0x20, n as u8, (n >> 8) as u8]
    }
}

fn round_trip(stream: &[u8], bits: u8, expected: &[u16]) {
    let mut out = vec![0xffff; expected.len()];
    let used = decode(&mut stream.iter().copied(), bits, &models(), &mut out).unwrap();
    assert_eq!(out, expected);
    assert_eq!(used, stream.len());
}

#[test]
fn every_width_and_model_round_trips() {
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    for bits in 1..=MAX_SYMBOL_BITS {
        for shape in 0..3u8 {
            let symbols: Vec<u16> = (0..200).map(|_| random.symbol(bits)).collect();
            let (payload, _) = encode(&models().get(bits, shape).unwrap().clone(), &symbols);
            let stream = [header(shape, 0, true, payload.len()), payload].concat();
            round_trip(&stream, bits, &symbols);
        }
    }
}

#[test]
fn intervals_tile_the_probability_range() {
    for bits in 1..=MAX_SYMBOL_BITS {
        let m = model(bits, 2);
        let mut next = 1;
        for symbol in 0..(1u16 << bits) {
            let (start, freq) = m.interval(symbol).unwrap();
            assert_eq!(start, next, "{bits}-bit symbol {symbol}");
            for slot in start..start + freq {
                assert_eq!(m.symbol(slot, bits), symbol);
            }
            next = start + freq;
        }
        assert_eq!(next, TOTAL);
        assert_eq!(m.interval(1 << bits), None);
    }
}

#[test]
fn fragments_reset_narrow_and_take_long_headers() {
    let mut random = Random(7);
    let mut stream = Vec::new();
    let mut expected = Vec::new();
    // 9-bit lane: narrowed fragments around a long one (3-byte header).
    for (narrow, count, shape) in [(2u8, 50, 1u8), (0, 900, 0), (3, 40, 2)] {
        let width = 9 - narrow;
        let symbols: Vec<u16> = (0..count).map(|_| random.symbol(width)).collect();
        let (payload, _) = encode(&model(width, u32::from(shape)), &symbols);
        assert_eq!(count == 900, payload.len() > 256);
        stream.extend(header(shape, narrow, true, payload.len()));
        stream.extend(payload);
        expected.extend(symbols);
    }
    round_trip(&stream, 9, &expected);
}

#[test]
fn a_continuous_stream_crosses_fragments_with_its_state() {
    let mut random = Random(11);
    let symbols: Vec<u16> = (0..3000).map(|_| random.symbol(8)).collect();
    let (payload, _) = encode(&model(8, 1), &symbols);
    // Split at uneven points; without a reset the next fragment feeds the
    // carried state, and symbols left in it are decoded between fragments.
    for piece in [1usize, 3, 97, 256, 257, 1000] {
        let mut stream = Vec::new();
        for chunk in payload.chunks(piece) {
            stream.extend(header(1, 0, false, chunk.len()));
            stream.extend(chunk);
        }
        round_trip(&stream, 8, &symbols);
    }
}

#[test]
fn trailing_zero_bytes_may_be_trimmed() {
    let mut random = Random(3);
    let mut trimmed_any = false;
    for _ in 0..50 {
        let symbols: Vec<u16> = (0..64).map(|_| random.symbol(4)).collect();
        let (mut payload, tail) = encode(&model(4, 0), &symbols);
        let mut cut = 0;
        while cut < tail.min(2) && payload.last() == Some(&0) {
            payload.pop();
            cut += 1;
        }
        trimmed_any |= cut > 0;
        let stream = [header(0, 0, true, payload.len()), payload].concat();
        round_trip(&stream, 4, &symbols);
    }
    assert!(trimmed_any);
}

#[test]
fn malformed_input_is_refused() {
    let symbols = [1u16, 2, 3, 0, 7, 5];
    let (payload, _) = encode(&model(3, 0), &symbols);
    let good = [header(0, 0, true, payload.len()), payload.clone()].concat();
    let run = |stream: &[u8], bits: u8, count: usize| {
        decode(
            &mut stream.iter().copied(),
            bits,
            &models(),
            &mut vec![0; count],
        )
    };
    assert_eq!(run(&good, 3, 6).map(|_| ()), Ok(()));
    assert_eq!(run(&good[..good.len() - 1], 3, 6), Err(Error::Truncated));
    assert_eq!(run(&good[..1], 3, 6), Err(Error::Truncated));
    assert_eq!(run(&good, 3, 5), Err(Error::Stalled));
    assert_eq!(run(&good, 0, 6), Err(Error::BadAlphabet));
    assert_eq!(run(&good, 10, 6), Err(Error::BadAlphabet));
    let narrow = [header(0, 3, true, payload.len()), payload.clone()].concat();
    assert_eq!(run(&narrow, 3, 6), Err(Error::BadAlphabet));
    let missing = [header(9, 0, true, payload.len()), payload].concat();
    assert_eq!(run(&missing, 3, 6), Err(Error::MissingModel));
    // A low state word of 0 is never encoded.
    assert_eq!(run(&[0x10, 3, 0x02, 0, 0, 0], 3, 1), Err(Error::BadState));
    assert_eq!(Model::new(&[1, 32768]), Err(Error::BadModel));
    assert_eq!(Model::new(&[32768, 32767]), Err(Error::BadModel));
    assert_eq!(Model::new(&[32768, 0]), Err(Error::BadModel));
    assert_eq!(Model::new(&[65535]), Err(Error::BadAlphabet));
}
