use super::*;

fn decode(words: &[u32]) -> Vec<Insn> {
    let isa: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    crate::rdna2::disassemble(&isa)
}

#[test]
fn a_source_field_the_instruction_does_not_read_is_unused() {
    // An unused field of the three-operand encoding is zero, which decodes as s0.
    for (words, unused) in [
        ([0xd5b30007, 0x00000108], 1), // v_sqrt_f32 v7, v8
        ([0xd7110007, 0x00021308], 2), // v_pack_b32_f16 v7, v8, v9
        ([0xd6ff0007, 0x00021509], 2), // v_lshlrev_b64 v[7:8], v9, v[10:11]
        ([0xd70f0607, 0x00021308], 2), // v_add_co_u32 v7, s6, v8, v9
    ] {
        let ins = &decode(&words)[0];
        assert!(unused_field(ins, unused, 0), "{words:x?}: {ins:?}");
        assert!(!unused_field(ins, unused, 1), "{words:x?}: only s0 is a zero field");
    }
    // v_cndmask_b32 v7, v8, v9, s0 and v_mad_u32_u24 v7, v8, v9, s0 read the third.
    for words in [[0xd5010007, 0x00021308], [0xd5430007, 0x00021308]] {
        let ins = &decode(&words)[0];
        assert!(!unused_field(ins, 2, 0), "{words:x?}: {ins:?}");
    }
}

#[test]
fn a_constant_is_known_only_where_every_path_ran_its_move() {
    // s_mov_b32 s5, 7; v_mov_b32 v0, s5
    let straight = decode(&[0xbe850387, 0x7e000205]);
    assert_eq!(constant_before(&straight, 1, 5), Some(7));
    assert_eq!(constant_before(&straight, 1, 6), None);
    // s_cbranch_scc0 past the move: one way to the use skips it.
    let skipped = decode(&[0xbf840001, 0xbe850387, 0x7e000205]);
    assert_eq!(constant_before(&skipped, 2, 5), None);
    assert_eq!(run_start(&skipped, 2), 2);
}

#[test]
fn a_program_with_only_32_bit_masks_has_one_word_masks() {
    // s_mov_b32 s26, exec_lo; v_cmp_lt_f32 s24, v0, v1
    let narrow = decode(&[0xbe9a037e, 0xd4010018, 0x00020300, 0xbf810000]);
    assert!(one_word_masks(&narrow));
    // s_mov_b64 s[26:27], exec handles the mask as 64 bits.
    assert!(!one_word_masks(&decode(&[0xbe9a047e, 0xd4010018, 0x00020300, 0xbf810000])));
    // Nothing shows the width of a program that never touches a mask with a scalar instruction.
    assert!(!one_word_masks(&decode(&[0xd4010018, 0x00020300, 0xbf810000])));
}
