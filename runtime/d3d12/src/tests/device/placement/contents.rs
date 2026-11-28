use super::Digest;

#[test]
fn a_change_to_any_one_byte_is_noticed() {
    let mut bytes: Vec<u8> = (0..4096 + 17).map(|i| (i * 31 % 251) as u8).collect();
    let before = Digest::of(&bytes);
    assert_eq!(before, Digest::of(&bytes.clone()));
    for at in 0..bytes.len() {
        for bit in [1, 0x80] {
            bytes[at] ^= bit;
            assert_ne!(Digest::of(&bytes), before, "byte {at} bit {bit:#x}");
            bytes[at] ^= bit;
        }
    }
}

#[test]
fn changes_that_cancel_in_a_sum_are_noticed() {
    let mut bytes = vec![0u8; 256];
    (bytes[0], bytes[32]) = (1, 2);
    let before = Digest::of(&bytes);
    (bytes[0], bytes[32]) = (2, 1);
    assert_ne!(Digest::of(&bytes), before, "two words of one lane swapped");
    (bytes[0], bytes[32], bytes[8]) = (0, 0, 3);
    assert_ne!(Digest::of(&bytes), before);
}

#[test]
fn zeros_of_different_lengths_differ() {
    let zeros = [0u8; 96];
    let digests: Vec<_> = (1..=zeros.len()).map(|length| Digest::of(&zeros[..length])).collect();
    for (i, digest) in digests.iter().enumerate() {
        assert!(!digests[i + 1..].contains(digest), "length {}", i + 1);
    }
}
