use super::parse;

#[test]
fn reserved_images_are_the_default() {
    assert!(parse(None));
    assert!(parse(Some("1")));
    assert!(parse(Some("")));
}

#[test]
fn zero_keeps_every_placed_image_committed() {
    for value in ["0", " 0 ", "off", "false", "no"] {
        assert!(!parse(Some(value)), "{value:?}");
    }
}
