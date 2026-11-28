use super::{isa_table_image_writes, isa_writes_no_image, may_write_images, stage_table_image_writes, stage_writes_no_image, table_image_writes};
use crate::rdna2;

fn isa(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

const ENDPGM: u32 = 0xbf81_0000;
/// MIMG instructions of two dwords with the opcode in bits 18..26.
fn mimg(op: u32) -> [u32; 2] {
    [0xf000_0108 | op << 18, 0]
}

fn mnemonics(words: &[u32]) -> Vec<String> {
    rdna2::instructions(&isa(words)).map(|op| op.mnemonic).collect()
}

#[test]
fn a_program_that_only_loads_images_writes_none() {
    let [load, operands] = mimg(0);
    let words = [load, operands, ENDPGM];
    assert_eq!(mnemonics(&words), ["image_load", "s_endpgm"]);
    assert!(isa_writes_no_image(&isa(&words)).is_some());
}

#[test]
fn every_store_and_atomic_counts_as_a_write() {
    for (op, name) in [
        (8, "image_store"),
        (9, "image_store_mip"),
        (10, "image_store_pck"),
        (11, "image_store_mip_pck"),
        (15, "image_atomic_swap"),
        (16, "image_atomic_cmpswap"),
        (17, "image_atomic_add"),
        (31, "image_atomic_fmax"),
    ] {
        let [first, operands] = mimg(op);
        let words = [first, operands, ENDPGM];
        assert_eq!(mnemonics(&words)[0], name);
        assert!(isa_writes_no_image(&isa(&words)).is_none(), "{name}");
    }
}

#[test]
fn what_cannot_be_read_counts_as_a_write() {
    // Nothing to read, a partial dword, and a truncated final instruction.
    assert!(isa_writes_no_image(&[]).is_none());
    assert!(isa_writes_no_image(&isa(&[ENDPGM])[..3]).is_none());
    assert!(isa_writes_no_image(&isa(&[mimg(0)[0]])).is_none());
    // An opcode the decoder has no name for.
    let unknown = rdna2::instructions(&isa(&[0xf3fc_0108, 0])).next().unwrap();
    assert!(unknown.mnemonic.contains('<'), "{}", unknown.mnemonic);
    assert!(isa_writes_no_image(&isa(&[0xf3fc_0108, 0, ENDPGM])).is_none());
    // Desktop or unknown containers are not console code.
    assert!(may_write_images(&[]));
    assert!(may_write_images(b"DXBC not a container"));
}

/// s_mov_b32 s0, s5 and s_mov_b32 s1, 0: the address of table 1's pointer.
const TABLE_ADDRESS: [u32; 2] = [0xbe80_0305, 0xbe81_0380];
/// s_load_dwordx8 s[8:15], s[0:1], with a constant byte offset.
fn fetch(offset: u32) -> [u32; 2] {
    [0xf40c_0200, 0xfa00_0000 | offset]
}
/// image_store v0, v[1:2], s[8:15] dmask:0x1 dim:2D
const STORE: [u32; 2] = [0xf020_1108, 0x0002_0001];
/// Table 1's pointer arrives in s5.
const TABLES: [(u8, u32); 1] = [(5, 1)];

fn program(parts: &[&[u32]]) -> Vec<u8> {
    isa(&parts.concat())
}

#[test]
fn a_store_through_a_table_slot_names_that_slot() {
    let code = program(&[&TABLE_ADDRESS, &fetch(0x60), &STORE, &[ENDPGM]]);
    assert_eq!(mnemonics_of(&code), ["s_mov_b32", "s_mov_b32", "s_load_dwordx8", "image_store", "s_endpgm"]);
    assert_eq!(isa_table_image_writes(&code, &TABLES), Some(vec![(1, 3)]));
    // A fetch that no store uses names nothing.
    let code = program(&[&TABLE_ADDRESS, &fetch(0x60), &[ENDPGM]]);
    assert_eq!(isa_table_image_writes(&code, &TABLES), Some(Vec::new()));
}

fn mnemonics_of(code: &[u8]) -> Vec<String> {
    rdna2::instructions(code).map(|op| op.mnemonic).collect()
}

#[test]
fn a_store_that_cannot_be_traced_to_a_table_slot_is_unknown() {
    // The pointer's register is not a table pointer: the heap base, say.
    let code = program(&[&TABLE_ADDRESS, &fetch(0x60), &STORE, &[ENDPGM]]);
    assert_eq!(isa_table_image_writes(&code, &[(6, 2)]), None);
    // The high half of the address was never cleared.
    let code = program(&[&TABLE_ADDRESS[..1], &fetch(0x60), &STORE, &[ENDPGM]]);
    assert_eq!(isa_table_image_writes(&code, &TABLES), None);
    // The offset comes from a register (s0 here), not a constant.
    let code = program(&[&TABLE_ADDRESS, &[0xf40c_0200, 0x0000_0000], &STORE, &[ENDPGM]]);
    assert_eq!(isa_table_image_writes(&code, &TABLES), None);
    // An offset that is not a whole descriptor.
    let code = program(&[&TABLE_ADDRESS, &fetch(0x64), &STORE, &[ENDPGM]]);
    assert_eq!(isa_table_image_writes(&code, &TABLES), None);
    // One dword of the descriptor is overwritten before the store: s_mov_b32 s9, 0.
    let code = program(&[&TABLE_ADDRESS, &fetch(0x60), &[0xbe89_0380], &STORE, &[ENDPGM]]);
    assert_eq!(isa_table_image_writes(&code, &TABLES), None);
    // No descriptor was loaded at all.
    assert_eq!(isa_table_image_writes(&program(&[&STORE, &[ENDPGM]]), &TABLES), None);
}

#[test]
fn paths_that_disagree_about_the_descriptor_are_unknown() {
    // s_cbranch_scc0 over the second fetch: the store sees slot 3 or slot 4.
    let code = program(&[&TABLE_ADDRESS, &fetch(0x60), &[0xbf84_0002], &fetch(0x80), &STORE, &[ENDPGM]]);
    assert_eq!(isa_table_image_writes(&code, &TABLES), None);
    // Both paths fetch the same slot: still that slot.
    let code = program(&[&TABLE_ADDRESS, &fetch(0x60), &[0xbf84_0002], &fetch(0x60), &STORE, &[ENDPGM]]);
    assert_eq!(isa_table_image_writes(&code, &TABLES), Some(vec![(1, 3)]));
    // Two stores through two slots name both.
    let code = program(&[&TABLE_ADDRESS, &fetch(0x60), &STORE, &fetch(0x20), &STORE, &[ENDPGM]]);
    assert_eq!(isa_table_image_writes(&code, &TABLES), Some(vec![(1, 1), (1, 3)]));
}

#[test]
fn a_container_that_is_not_console_code_is_unknown() {
    assert_eq!(table_image_writes(&[]), None);
    assert_eq!(table_image_writes(b"DXBC not a container"), None);
}

#[test]
fn setting_a_hardware_register_leaves_the_descriptor_alone() {
    // s_setreg_imm32_b32 with 9 in its scalar field, then its literal: the
    // field names no scalar the instruction writes.
    let setreg = [0xba89_0001, 0x0000_0003];
    let code = program(&[&TABLE_ADDRESS, &fetch(0x60), &setreg, &STORE, &[ENDPGM]]);
    assert_eq!(mnemonics_of(&code)[3], "s_setreg_imm32_b32");
    assert_eq!(isa_table_image_writes(&code, &TABLES), Some(vec![(1, 3)]));
}

/// s_swappc_b64 s[12:13], s[12:13]: a vertex program's call into its fetch program.
const FETCH_CALL: u32 = 0xbe8c_210c;

#[test]
fn a_vertex_fetch_call_writes_nothing_because_the_translation_drops_it() {
    assert_eq!(mnemonics(&[FETCH_CALL, ENDPGM]), ["s_swappc_b64", "s_endpgm"]);
    let code = isa(&[FETCH_CALL, ENDPGM]);
    assert!(stage_writes_no_image(&code, true).is_some());
    assert_eq!(stage_table_image_writes(&code, &TABLES, true), Some(Vec::new()));
    // Stores of the vertex program itself are still traced.
    let code = program(&[&[FETCH_CALL], &TABLE_ADDRESS, &fetch(0x60), &STORE, &[ENDPGM]]);
    assert_eq!(stage_table_image_writes(&code, &TABLES, true), Some(vec![(1, 3)]));
}

#[test]
fn a_call_outside_a_vertex_program_still_counts_as_a_write() {
    let code = isa(&[FETCH_CALL, ENDPGM]);
    assert!(isa_writes_no_image(&code).is_none());
    assert!(stage_writes_no_image(&code, false).is_none());
    assert_eq!(isa_table_image_writes(&code, &TABLES), None);
    assert_eq!(stage_table_image_writes(&code, &TABLES, false), None);
    // Other calls stay unknown in a vertex program too.
    let set_pc = 0xbe80_200c; // s_setpc_b64 s[12:13]
    assert_eq!(mnemonics(&[set_pc])[0], "s_setpc_b64");
    assert!(stage_writes_no_image(&isa(&[set_pc, ENDPGM]), true).is_none());
}
