use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn element(register: u32, comp_type: u32, mask: u8) -> SigElement {
    SigElement { name: "TEXCOORD".into(), index: register, sysval: 0, comp_type, register, mask, rw_mask: mask }
}

#[test]
fn half_registers_share_parameters_by_channel() {
    let sig = [element(1, 6, 15), element(2, 1, 3), element(3, 6, 7), element(4, 6, 3), element(5, 6, 7), element(6, 6, 15)];
    let params = assign(&sig, &[6], |_| true);
    assert_eq!(params, vec![
        Param::Half(vec![(1, 0), (3, 2)]), Param::Full(2), Param::Half(vec![(4, 0), (5, 1)]), Param::Full(6),
    ]);
    assert_eq!(params[0].component(3, false), Some((3, 2)));
    assert_eq!(params[2].component(1, true), Some((5, 1)));
    assert_eq!(params[2].channel(5, 2), Some((2, Some(false))));
    assert_eq!(params[3].channel(6, 3), Some((3, None)));
}

/// What a program reads: (parameter, channel, half, takes a vertex's own value).
fn wanted(reads: &[(usize, usize, Option<bool>, bool)]) -> BTreeMap<usize, BTreeSet<Interpolation>> {
    let mut by_parameter: BTreeMap<usize, BTreeSet<Interpolation>> = BTreeMap::new();
    for &(parameter, channel, half, own) in reads {
        by_parameter.entry(parameter).or_default().insert((channel, half, own));
    }
    by_parameter
}

fn read(register: u32, comp_type: u32, mask: u8, rw_mask: u8) -> SigElement {
    SigElement { rw_mask, ..element(register, comp_type, mask) }
}

#[test]
fn the_reads_choose_among_parameters_the_declarations_call_read() {
    // As in ProjectScarlett's VfxVBuffer pixel programs: a constant register, a
    // float4, a register the program never interpolates although it is declared
    // read, a pair of halves, and a float2; the hardware says four parameters.
    let sig = [
        read(3, 1, 12, 12), read(4, 3, 15, 15), read(5, 3, 3, 0), read(5, 3, 12, 12),
        read(6, 6, 7, 0), read(7, 6, 7, 7), read(18, 3, 3, 3),
    ];
    let flat = [3];
    let all = assign(&sig, &flat, |_| true);
    let reads = wanted(&[(0, 2, None, true), (1, 0, None, false), (1, 3, None, false), (2, 2, Some(false), false), (3, 1, None, false)]);
    let chosen = forced(&all, &sig, &flat, 4, &reads).unwrap();
    assert_eq!(chosen, vec![Param::Full(3), Param::Full(4), Param::Half(vec![(6, 0), (7, 2)]), Param::Full(18)]);
}

#[test]
fn reads_that_allow_two_sources_choose_nothing() {
    // Either register could be the one parameter.
    let sig = [read(1, 3, 15, 15), read(2, 3, 15, 15)];
    let all = assign(&sig, &[], |_| true);
    assert_eq!(forced(&all, &sig, &[], 1, &wanted(&[(0, 0, None, false)])), None);
    // With both parameters present there is no choice left.
    assert_eq!(forced(&all, &sig, &[], 2, &wanted(&[(0, 0, None, false)])), Some(all.clone()));
    // Interpolating a constant register is not a possible reading of it.
    assert_eq!(forced(&all, &sig, &[1], 1, &wanted(&[(0, 0, None, false)])), Some(vec![Param::Full(2)]));
    assert_eq!(forced(&all, &sig, &[1], 1, &wanted(&[(0, 0, None, true)])), None);
    // A half cannot come from a full register, nor a parameter past the count.
    assert_eq!(forced(&all, &sig, &[], 2, &wanted(&[(0, 0, Some(true), false)])), None);
    assert_eq!(forced(&all, &sig, &[], 1, &wanted(&[(1, 0, None, false)])), None);
}
