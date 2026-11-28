use super::*;
use crate::rootsig::RootParam;

const LIMIT: u32 = 65_535;

fn constants(register: u32, space: u32, num32: u32) -> RootParam {
    RootParam { visibility: 0, kind: RootParamKind::Constants { register, space, num32 } }
}

fn root(params: Vec<RootParam>) -> RootSig {
    RootSig { version: 2, flags: 0, params, num_static_samplers: 0, static_samplers: Vec::new() }
}

#[test]
fn takes_only_the_trailing_group_base() {
    let mut with = root(vec![constants(0, 0, 4), constants(REGISTER, SPACE, WORDS)]);
    assert!(take(&mut with));
    assert_eq!(with.params.len(), 1);
    // Taken once: the title's own constants stay.
    assert!(!take(&mut with));
    assert_eq!(with.params.len(), 1);

    // Not last, another size or another register is the title's.
    for params in [
        vec![constants(REGISTER, SPACE, WORDS), constants(0, 0, 4)],
        vec![constants(REGISTER, SPACE, 4)],
        vec![constants(1, SPACE, WORDS)],
        vec![constants(REGISTER, 0, WORDS)],
    ] {
        let mut other = root(params);
        let count = other.params.len();
        assert!(!take(&mut other));
        assert_eq!(other.params.len(), count);
    }
}

fn covered(parts: &[Part]) -> Vec<[u32; 3]> {
    let mut ids = Vec::new();
    for part in parts {
        for z in 0..part.groups[2] {
            for y in 0..part.groups[1] {
                for x in 0..part.groups[0] {
                    ids.push([part.base[0] + x, part.base[1] + y, part.base[2] + z]);
                }
            }
        }
    }
    ids.sort_unstable();
    ids
}

fn every(groups: [u32; 3]) -> Vec<[u32; 3]> {
    let mut ids = Vec::new();
    for z in 0..groups[2] {
        for y in 0..groups[1] {
            for x in 0..groups[0] {
                ids.push([x, y, z]);
            }
        }
    }
    ids.sort_unstable();
    ids
}

#[test]
fn splits_only_the_dimension_over_the_limit() {
    let parts = split([70_000, 1, 1], LIMIT, 64).unwrap();
    assert_eq!(parts, [
        Part { base: [0, 0, 0], groups: [65_535, 1, 1] },
        Part { base: [65_535, 0, 0], groups: [4_465, 1, 1] },
    ]);
    assert_eq!(split([2, 70_000, 3], LIMIT, 64).unwrap(), [
        Part { base: [0, 0, 0], groups: [2, 65_535, 3] },
        Part { base: [0, 65_535, 0], groups: [2, 4_465, 3] },
    ]);
    // A count on the limit is one dispatch.
    assert_eq!(split([LIMIT, LIMIT, 1], LIMIT, 64).unwrap(), [Part { base: [0; 3], groups: [LIMIT, LIMIT, 1] }]);
}

#[test]
fn every_group_runs_once_with_its_console_id() {
    // A small limit keeps the enumeration short; the arithmetic is the same.
    for groups in [[10, 1, 1], [7, 9, 1], [1, 1, 11], [5, 4, 9], [3, 0, 7]] {
        let parts = split(groups, 4, 1000).unwrap();
        let ids = covered(&parts);
        assert!(parts.iter().all(|p| p.groups.iter().all(|&n| n <= 4)), "{groups:?}: {parts:?}");
        assert_eq!(ids, every(groups), "{groups:?}");
    }
    let parts = split([70_000, 2, 1], LIMIT, 64).unwrap();
    assert!(parts.iter().all(|p| p.groups.iter().all(|&n| n <= LIMIT)));
    assert_eq!(covered(&parts), every([70_000, 2, 1]));
}

#[test]
fn refuses_more_parts_than_allowed() {
    assert_eq!(split([u32::MAX, 1, 1], LIMIT, 65_537).map(|p| p.len()), Some(65_537));
    let last = *split([u32::MAX, 1, 1], LIMIT, 65_537).unwrap().last().unwrap();
    assert_eq!(last, Part { base: [65_536 * LIMIT, 0, 0], groups: [u32::MAX - 65_536 * LIMIT, 1, 1] });
    assert!(split([u32::MAX, 1, 1], LIMIT, 65_536).is_none());
    assert!(split([1, 1, 1], 0, 64).is_none());
}
