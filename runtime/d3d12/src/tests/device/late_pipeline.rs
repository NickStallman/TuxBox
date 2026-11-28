use super::*;

#[test]
fn a_stand_in_is_used_until_the_compile_finishes() {
    let slot = Slot::<u32>::default();
    assert_eq!(slot.pick((&1, &"stand-in"), &"compiled"), (&1, &"stand-in", false));
    let compile = slot.clone();
    std::thread::spawn(move || compile.fill(2)).join().unwrap();
    assert_eq!(slot.pick((&1, &"stand-in"), &"compiled"), (&2, &"compiled", true));
    assert_eq!(slot.pick((&1, &"stand-in"), &"compiled"), (&2, &"compiled", false));
}
