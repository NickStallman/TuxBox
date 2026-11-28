use super::*;
use crate::encode::{Random, code, references, stream};
use crate::record::{self, deinterleave};

fn pair_symbol(a: u8, b: u8, pairs: &[[u8; 2]; 64]) -> u16 {
    pairs.iter().position(|p| *p == [a, b]).unwrap() as u16
}

/// Lane symbols for `states` with the given references.
fn symbols(format: Format, states: &[State], refs: &[(usize, usize)]) -> Vec<Vec<u16>> {
    let l = layout(format);
    let per = format.block_bytes() / 8;
    let mut lanes = vec![vec![0u16; states.len() * per]; 12];
    for (i, (target, &(er, xr))) in states.iter().zip(refs).enumerate() {
        let get = |r| match source(i, r) {
            Source::Zero => State::default(),
            Source::Block(j) => states[j],
            Source::Dictionary(k) => dictionary(format, k),
        };
        let (e, x) = (get(er), get(xr));
        let mut res = State::default();
        for (k, &bits) in l.ep_bits.iter().enumerate() {
            res.ep[k] = target.ep[k].wrapping_sub(e.ep[k]) & ((1u16 << bits) - 1) as u8;
        }
        for p in 0..l.alpha + l.colour {
            res.ix[p] = target.ix[p].wrapping_sub(x.ix[p]) & if p < l.alpha { 7 } else { 3 };
        }
        let ep = |k: usize| code(0, res.ep[k], u32::from(l.ep_bits[k]));
        let pair = |base: usize, k: usize| {
            pair_symbol(res.ix[base + 2 * k], res.ix[base + 2 * k + 1], l.pairs)
        };
        let lane0 = code(xr as u8, er as u8, 8) as u8;
        if format == Format::Bc4 {
            lanes[0][i] = u16::from(lane0);
            lanes[1][i] = xr as u16;
            lanes[2][i] = ep(0);
            lanes[3][i] = ep(1);
            for k in 0..8 {
                lanes[4 + k][i] = pair(0, k);
            }
            continue;
        }
        let mut put = |lane: usize, (a, b): (u16, u16)| {
            lanes[lane][2 * i] = a;
            lanes[lane][2 * i + 1] = b;
        };
        put(0, deinterleave(lane0));
        put(1, deinterleave(xr as u8));
        if format == Format::Bc5 {
            for c in 0..2 {
                put(2 + c, (ep(2 * c), ep(2 * c + 1)));
                for lane in 0..4 {
                    put(
                        4 + 4 * c + lane,
                        (pair(16 * c, 2 * lane), pair(16 * c, 2 * lane + 1)),
                    );
                }
            }
        } else {
            put(2, (ep(0), ep(1)));
            for lane in 0..4 {
                put(3 + lane, (pair(0, 2 * lane), pair(0, 2 * lane + 1)));
            }
            for c in 0..3 {
                put(7 + c, (ep(2 + c), ep(5 + c)));
            }
            let quad = |q: usize| {
                let (ox, oy) = ((q & 1) * 2, (q >> 1) * 2);
                let key: Vec<u8> = [(0, 0), (1, 0), (0, 1), (1, 1)]
                    .iter()
                    .map(|(dx, dy)| res.ix[16 + 4 * (oy + dy) + ox + dx])
                    .collect();
                bc3::QUADS.iter().position(|v| v[..] == key[..]).unwrap() as u16
            };
            put(10, (quad(0), quad(1)));
            put(11, (quad(2), quad(3)));
        }
    }
    lanes
}

fn random_state(format: Format, random: &mut Random) -> State {
    let l = layout(format);
    let mut st = State::default();
    for (k, &bits) in l.ep_bits.iter().enumerate() {
        st.ep[k] = (random.next() & ((1 << bits) - 1)) as u8;
    }
    for p in 0..l.alpha + l.colour {
        st.ix[p] = (random.next() & if p < l.alpha { 7 } else { 3 }) as u8;
    }
    st
}

#[test]
fn the_rank_tables_are_bijections() {
    for pairs in [&bc3::PAIRS, &bc4::PAIRS, &bc5::PAIRS] {
        for a in 0..8 {
            for b in 0..8 {
                assert_eq!(pairs.iter().filter(|p| **p == [a, b]).count(), 1);
            }
        }
    }
    let mut seen = [false; 256];
    for q in &bc3::QUADS {
        let key = q.iter().fold(0usize, |k, &v| k << 2 | usize::from(v));
        assert!(!seen[key]);
        seen[key] = true;
    }
}

#[test]
fn random_blocks_round_trip() {
    let mut random = Random(0xbc345);
    for format in [Format::Bc3, Format::Bc4, Format::Bc5] {
        for (count, longest) in [(1usize, 1usize), (300, 150), (1500, 2000)] {
            let states: Vec<State> = (0..count)
                .map(|_| random_state(format, &mut random))
                .collect();
            let refs: Vec<(usize, usize)> =
                (0..count).map(|i| references(i, &mut random)).collect();
            let data = stream(
                format.widths(),
                &symbols(format, &states, &refs),
                longest,
                &mut random,
            );
            let size = format.block_bytes();
            let mut expected = vec![0u8; count * size];
            for (st, block) in states.iter().zip(expected.chunks_exact_mut(size)) {
                pack(format, st, block);
            }
            let mut out = vec![0u8; expected.len()];
            let report = record::decode(format, &data, &mut out).unwrap();
            assert_eq!(out, expected, "{format:?}");
            let early = refs
                .iter()
                .enumerate()
                .map(|(i, &(e, x))| usize::from(e > i) + usize::from(x > i))
                .sum();
            assert_eq!(report.dictionary_references, early);
        }
    }
}
