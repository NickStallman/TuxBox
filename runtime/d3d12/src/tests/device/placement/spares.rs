use super::*;

const MIB: u64 = 1 << 20;

#[test]
fn a_returned_spare_serves_the_next_publication_of_its_size() {
    let spares = Spares::default();
    let now = Instant::now();
    assert!(spares.take(925 * MIB, now).is_none());
    spares.put("large", rounded(925 * MIB).unwrap(), now);
    let (item, size) = spares.take(900 * MIB, now).unwrap();
    assert_eq!((item, size), ("large", 1024 * MIB));
    assert!(spares.take(900 * MIB, now).is_none(), "a spare serves one publication at a time");
}

#[test]
fn the_smallest_fitting_spare_is_taken_and_a_small_publication_leaves_a_large_one() {
    let spares = Spares::default();
    let now = Instant::now();
    for size in [512 * MIB, MIB, 2 * MIB] {
        spares.put(size, size, now);
    }
    assert_eq!(spares.take(MIB + 1, now).map(|(item, _)| item), Some(2 * MIB));
    assert_eq!(spares.take(MIB / 2, now).map(|(item, _)| item), Some(MIB));
    assert!(spares.take(MIB / 2, now).is_none());
    assert_eq!(spares.sizes(), [512 * MIB]);
}

#[test]
fn unused_spares_are_released_and_the_budget_is_kept() {
    let spares = Spares::default();
    let start = Instant::now();
    spares.put(0, 64 * MIB, start);
    spares.put(1, 4 * MIB, start + RETAIN / 2);
    assert!(spares.take(64 * MIB, start + RETAIN).is_none(), "the older spare expired");
    assert_eq!(spares.sizes(), [4 * MIB]);
    let later = start + RETAIN;
    spares.put(2, RETAIN_BYTES - 2 * MIB, later);
    assert_eq!(spares.sizes(), [RETAIN_BYTES - 2 * MIB], "the oldest spare is released over the budget");
    spares.put(3, 2 * MIB, later);
    assert_eq!(spares.sizes(), [2 * MIB, RETAIN_BYTES - 2 * MIB]);
    spares.put(4, MIB, later);
    assert_eq!(spares.sizes(), [MIB, 2 * MIB]);
}

#[test]
fn sizes_are_rounded_to_an_eighth_of_their_power_of_two() {
    assert_eq!(rounded(1), Some(64 << 10));
    assert_eq!(rounded(512 << 10), Some(512 << 10));
    assert_eq!(rounded(513 << 10), Some(640 << 10));
    assert_eq!(rounded(925 * MIB), Some(1024 * MIB));
    assert_eq!(rounded(u64::MAX), None);
}
