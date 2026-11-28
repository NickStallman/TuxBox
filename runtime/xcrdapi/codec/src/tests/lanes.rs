use super::*;
use crate::encode::{MODELS, Random, stream};
use crate::models::FREQ;

#[test]
fn every_silicon_rans_model_tiles_the_probability_range() {
    // Model::new checks the total of 65536; symbol 0 needs at least 2.
    for (width, row) in FREQ.iter().enumerate() {
        for (index, freq) in row.iter().enumerate() {
            let bits = width as u8 + 1;
            let model = MODELS.get(bits, index as u8).unwrap();
            assert_eq!(model.bits(), bits);
            assert!(freq[width + 2..].iter().all(|&f| f == 0));
        }
    }
}

fn random_lanes(widths: &[u8], per_lane: usize, random: &mut Random) -> Vec<Vec<u16>> {
    widths
        .iter()
        .map(|&w| {
            (0..per_lane)
                .map(|_| {
                    let bits = random.below(u64::from(w) + 1) as u32;
                    (random.next() as u16) & ((1u32 << bits) - 1) as u16
                })
                .collect()
        })
        .collect()
}

#[test]
fn interleaved_lanes_round_trip() {
    let mut random = Random(0x5eed);
    for (widths, per_lane, longest) in [
        (&[8u8, 8, 5, 6, 5, 5, 6, 5, 8, 8, 8, 8][..], 700, 150),
        (
            &[4, 4, 8, 8, 8, 8, 8, 7, 7, 5, 6, 8, 8, 8, 8, 5][..],
            300,
            1000,
        ),
        (&[1, 9, 3][..], 1, 1),
        (&[2; 16][..], 64, 7),
    ] {
        let symbols = random_lanes(widths, per_lane, &mut random);
        let data = stream(widths, &symbols, longest, &mut random);
        let mut padded = data.clone();
        padded.extend([0; 5]);
        let decoded = decode_slice(&padded, widths, per_lane, &MODELS).unwrap();
        assert_eq!(decoded.consumed, data.len());
        for (lane, expected) in symbols.iter().enumerate() {
            assert_eq!(decoded.lane(lane), &expected[..]);
        }
    }
}

#[test]
fn malformed_streams_are_refused() {
    let mut random = Random(9);
    let widths = [8u8, 5, 6];
    let symbols = random_lanes(&widths, 40, &mut random);
    let data = stream(&widths, &symbols, 13, &mut random);
    assert_eq!(
        decode_slice(&data[..data.len() - 1], &widths, 40, &MODELS).map(|_| ()),
        Err(Error::Truncated)
    );
    let mut trailing = data.clone();
    trailing.push(1);
    assert_eq!(
        decode_slice(&trailing, &widths, 40, &MODELS).map(|_| ()),
        Err(Error::TrailingData)
    );
    // One block too few leaves bytes unread with symbols still in a lane.
    assert!(decode_slice(&data, &widths, 39, &MODELS).is_err());
    assert!(decode_slice(&data, &widths, 41, &MODELS).is_err());
    assert_eq!(
        decode_slice(&data, &[], 40, &MODELS).map(|_| ()),
        Err(Error::BadAlphabet)
    );
    assert_eq!(
        decode_slice(&data, &[10], 40, &MODELS).map(|_| ()),
        Err(Error::BadAlphabet)
    );
    assert_eq!(
        decode_slice(&data, &widths, 0, &MODELS).map(|_| ()),
        Err(Error::BadAlphabet)
    );
}
