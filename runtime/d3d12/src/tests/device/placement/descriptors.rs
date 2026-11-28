use super::{DescriptorMap, PlacedView, coalesce, outside};
use std::sync::Arc;
use std::time::Instant;

const STRIDE: usize = 32;
const BASE: usize = 0x10_0000;

fn slot(index: usize) -> usize {
    BASE + index * STRIDE
}

fn heap(count: usize) -> std::ops::Range<usize> {
    BASE..slot(count)
}

fn gather(
    map: &DescriptorMap<PlacedView<u32>>,
    sampled: &[std::ops::Range<usize>],
    read: &[std::ops::Range<usize>],
) -> (Vec<u32>, Vec<u32>) {
    let (mut reads, mut writes) = (Vec::new(), Vec::new());
    map.gather(sampled, read, &mut reads, &mut writes);
    let values = |list: Vec<Arc<u32>>| {
        let mut values: Vec<u32> = list.iter().map(|t| **t).collect();
        values.sort_unstable();
        values
    };
    (values(reads), values(writes))
}

#[test]
fn ranges_join_when_they_overlap_or_touch() {
    assert_eq!(coalesce(&[8..12, 0..4, 4..6, 10..16, 20..20, 30..31]), [0..6, 8..16, 30..31]);
    assert_eq!(outside(&[0..10, 20..30], &[2..4, 8..22, 29..40]), [0..2, 4..8, 22..29]);
}

#[test]
fn a_heap_named_by_every_list_is_walked_once() {
    const COUNT: usize = 33_000;
    let map = DescriptorMap::<PlacedView<u32>>::default();
    let textures: Vec<_> = (0..COUNT as u32).map(Arc::new).collect();
    for (index, texture) in textures.iter().enumerate() {
        map.set(slot(index), Some(texture), index % 100 == 0);
    }
    for (lists, read) in [(1, 0), (25, 0), (25, 25)] {
        let sampled = vec![heap(COUNT); lists];
        let read = vec![heap(COUNT); read];
        let (mut reads, mut writes) = (Vec::new(), Vec::new());
        let begin = Instant::now();
        map.gather(&sampled, &read, &mut reads, &mut writes);
        let walk = begin.elapsed();
        assert_eq!(reads.len() + writes.len(), COUNT, "each descriptor is gathered once");
        assert_eq!(writes.len(), COUNT / 100);
        // What submit does next with the batch's textures.
        let begin = Instant::now();
        reads.extend(writes);
        reads.sort_unstable_by_key(Arc::as_ptr);
        reads.dedup_by(|a, b| Arc::ptr_eq(a, b));
        let unique = begin.elapsed();
        println!("gather of {COUNT} views, {} sampled and {} read ranges: walk {walk:?}, unique {unique:?}",
            sampled.len(), read.len());
    }
}

#[test]
fn written_copied_and_overwritten_descriptors_follow_the_heap() {
    let map = DescriptorMap::<PlacedView<u32>>::default();
    let [a, b, c] = [1, 2, 3].map(Arc::new);
    let all = [heap(16)];
    assert_eq!(gather(&map, &all, &[]), (vec![], vec![]));
    map.set(slot(3), Some(&a), false);
    map.set(slot(5), Some(&b), true);
    assert_eq!(gather(&map, &all, &[]), (vec![1], vec![2]));
    // A writable view is only a read where the batch cannot write.
    assert_eq!(gather(&map, &[], &all), (vec![1, 2], vec![]));
    assert_eq!(gather(&map, &[slot(0)..slot(4)], &all), (vec![1, 2], vec![]));
    assert_eq!(gather(&map, &[slot(4)..slot(6)], &all), (vec![1], vec![2]));
    // Copied in.
    map.copy(STRIDE, std::iter::once((slot(8), 1)), std::iter::once((slot(3), 1)));
    map.set(slot(9), Some(&c), false);
    assert_eq!(gather(&map, &[slot(8)..slot(10)], &[]), (vec![1, 3], vec![]));
    // Overwritten and removed.
    map.set(slot(3), Some(&c), false);
    map.set(slot(5), None, false);
    assert_eq!(gather(&map, &all, &[]), (vec![1, 3, 3], vec![]));
    drop(map.clear_deferred(STRIDE, slot(8), 1));
    assert_eq!(gather(&map, &all, &all), (vec![3, 3], vec![]));
    // Released textures drop out without a descriptor write.
    drop(c);
    assert_eq!(gather(&map, &all, &[]), (vec![], vec![]));
    // Outside every participating range.
    map.set(slot(20), Some(&a), false);
    assert_eq!(gather(&map, &all, &[heap(20)]), (vec![], vec![]));
}
