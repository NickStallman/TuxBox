//! Texture sizes taken from a descriptor. Encodings are from llvm-mc `-mcpu=gfx1030`.
use super::*;
use crate::meta::{RootParam, RootParamKind, RootRange, RootSig};

const END: u32 = 0xbf810000;
const LOAD: [u32; 2] = [0xf40c0202, 0xfa000020]; // s_load_dwordx8 s[8:15], s[4:5], 0x20
const WIDTH_HIGH: u32 = 0x8f03820a; // s_lshl_b32 s3, s10, 2
const WIDTH_LOW: u32 = 0x90029e09; // s_lshr_b32 s2, s9, 30
const JOIN: u32 = 0x88030302; // s_or_b32 s3, s2, s3
const LEVEL: [u32; 2] = [0x9382ff0b, 0x0004000c]; // s_bfe_u32 s2, s11, 0x4000c
const MASK: [u32; 2] = [0x8703ff03, 0x0000ffff]; // s_and_b32 s3, s3, 0xffff
const HEIGHT: [u32; 2] = [0x9386ff0a, 0x0010000e]; // s_bfe_u32 s6, s10, 0x10000e
const PLUS_ONE: [u32; 2] = [0x81038103, 0x81068106]; // s_add_i32 s3, s3, 1; s_add_i32 s6, s6, 1
const SCALE: [u32; 2] = [0x90030203, 0x90060206]; // s_lshr_b32 s3, s3, s2; s_lshr_b32 s6, s6, s2
const USE: [u32; 2] = [0x7e080203, 0x7e0a0206]; // v_mov_b32 v4, s3; v_mov_b32 v5, s6
const IMAGE: [u32; 2] = [0xf0000f08, 0x00020004]; // image_load v[0:3], v[4:5], s[8:15], 2D

/// A rooted compute program with no parameters yet.
fn extract(words: &[u32]) -> ShaderExtract {
    let isa: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    ShaderExtract { flat_inputs: Vec::new(), flat_outputs: Vec::new(),
        stage: Stage::Compute, shader_model: (6, 0), required_features: None,
        bitcode_stripped: true, fspx_config: None, ps09_pixel_inputs: None,
        fssm: Vec::new(), entry: Some("main".into()), isa_data: isa.clone(),
        isa, isa_offset_in_fssm: 0, psv_threads: None, isa_preamble: [0, 32, 1, 1],
        has_input_sig: false, has_output_sig: false, has_psv0: false, has_root_sig: true,
        input_sig: Vec::new(), output_sig: Vec::new(), resources: Vec::new(),
        gs_info: None, mesh_info: None, primitive_sig: Vec::new(),
        root_sig: Some(RootSig { version: 1, flags: 0, num_static_samplers: 0, static_samplers: Vec::new(), params: Vec::new() }),
    }
}

/// A compute program under one table of four SRVs, t0 to t3.
fn tabled(words: &[&[u32]]) -> ShaderExtract {
    let mut ex = extract(&[&words.concat()[..], &[END]].concat());
    let range = RootRange { range_type: 0, num: 4, base_register: 0, space: 0, offset_in_table: 0 };
    ex.root_sig.as_mut().unwrap().params.push(RootParam { visibility: 0, kind: RootParamKind::Table(vec![range]) });
    ex
}

fn sequence(after_level: &[u32]) -> ShaderExtract {
    tabled(&[&LOAD, &[WIDTH_HIGH, WIDTH_LOW, JOIN], &LEVEL, after_level, &MASK, &HEIGHT, &PLUS_ONE, &SCALE, &USE, &IMAGE])
}

#[test]
fn the_load_fills_the_size_fields_from_the_texture() {
    let hlsl = translate(&sequence(&[])).unwrap().hlsl;
    let fill = hlsl.find("tex1.GetDimensions(0u, w0, h0, n0);").expect(&hlsl);
    for line in ["s9 = ((max(w0, 1u) - 1u) & 3u) << 30;",
                 "s10 = ((max(w0, 1u) - 1u) >> 2) | ((max(h0, 1u) - 1u) << 14);", "s11 = 0u;"] {
        let at = hlsl[fill..].find(line).unwrap_or_else(|| panic!("{line}: {hlsl}"));
        // Before the program's first read of the words.
        assert!(fill + at < hlsl.find("s_lshl_b32").unwrap(), "{hlsl}");
    }
}

#[test]
fn a_descriptor_used_only_as_a_descriptor_is_not_filled() {
    let hlsl = translate(&tabled(&[&LOAD, &IMAGE])).unwrap().hlsl;
    assert!(!hlsl.contains("GetDimensions"), "{hlsl}");
}

#[test]
fn a_size_value_used_outside_the_computation_is_refused() {
    let refusal = |ex: ShaderExtract| translate(&ex).err().expect("refused");
    // v_mov_b32 v6, s2 reads the base level, which is not the console's.
    assert!(refusal(sequence(&[0x7e0c0202])).contains("used outside its size computation at 0x1c"));
    // The width without its level: v_mov_b32 v4, s3 straight after the add.
    let unscaled = tabled(&[&LOAD, &[WIDTH_HIGH, WIDTH_LOW, JOIN], &MASK, &[PLUS_ONE[0], USE[0]], &IMAGE]);
    assert!(refusal(unscaled).contains("used outside its size computation"));
    // A width scaled by another texture's level is not this view's size either.
    let other = [0x9382ff13, 0x0004000c]; // s_bfe_u32 s2, s19, 0x4000c
    let second = [0xf40c0402, 0xfa000040]; // s_load_dwordx8 s[16:23], s[4:5], 0x40
    let mixed = tabled(&[&LOAD, &second, &[WIDTH_HIGH, WIDTH_LOW, JOIN], &other, &MASK, &[PLUS_ONE[0], SCALE[0], USE[0]], &IMAGE]);
    assert!(refusal(mixed).contains("used outside its size computation"));
}

#[test]
fn a_size_value_lost_at_a_join_is_refused_only_when_read() {
    // s_cbranch_scc0 over the level extract: s2 holds the level on one path only.
    let skip = 0xbf840002;
    let joined = |read: &[u32]| tabled(&[&LOAD, &[skip], &LEVEL, read, &IMAGE]);
    assert!(translate(&joined(&[])).is_ok());
    // v_mov_b32 v6, s2 after the join.
    let reason = translate(&joined(&[0x7e0c0202])).err().expect("refused");
    assert!(reason.contains("used outside its size computation"), "{reason}");
}

#[test]
fn the_mask_may_be_held_in_a_register() {
    // s_mov_b32 s7, 0xffff; s_and_b32 s3, s3, s7
    let held = tabled(&[&LOAD, &[0xbe8703ff, 0x0000ffff, WIDTH_HIGH, WIDTH_LOW, JOIN], &LEVEL, &[0x87030703], &HEIGHT, &PLUS_ONE, &SCALE, &USE, &IMAGE]);
    assert!(translate(&held).unwrap().hlsl.contains("tex1.GetDimensions(0u, w0, h0, n0);"));
    // Another mask leaves the height in the value.
    let wide = tabled(&[&LOAD, &[WIDTH_HIGH, WIDTH_LOW, JOIN], &LEVEL, &[0x8703ff03, 0x000fffff], &PLUS_ONE, &SCALE, &USE, &IMAGE]);
    assert!(translate(&wide).is_err());
}

/// The program `tools/check_descriptor_size.py` runs on the GPU.
#[test]
fn texture_size_from_descriptor_fields_for_the_gpu_check() {
    let translated = translate(&sequence(&[])).unwrap();
    println!("BEGIN_HLSL\n{}\nEND_HLSL", translated.hlsl);
}

const BUFFER: [u32; 2] = [0xf4080202, 0xfa000020]; // s_load_dwordx4 s[8:11], s[4:5], 0x20
const READ: [u32; 2] = [0xe0300000, 0x80020000]; // buffer_load_dword v0, off, s[8:11], 0
const LAST: u32 = 0x8103c10a; // s_add_i32 s3, s10, -1

/// The same table with PSV naming t1 a raw buffer.
fn buffered(words: &[&[u32]]) -> ShaderExtract {
    let mut ex = tabled(words);
    ex.resources = vec![crate::psv::PsvResource { res_type: 4, space: 0, lower: 1, upper: 1 }];
    ex
}

#[test]
fn the_load_fills_a_buffer_record_count() {
    let hlsl = translate(&buffered(&[&BUFFER, &[LAST], &READ])).unwrap().hlsl;
    let fill = hlsl.find("uint n0; raw_sbuf1.GetDimensions(n0);").expect(&hlsl);
    let word = fill + hlsl[fill..].find("s10 = n0;").expect(&hlsl);
    assert!(word < hlsl.find("s_add_i32").unwrap(), "{hlsl}");
    // Words 1 and 3 are not filled.
    assert!(!hlsl.contains("s9 = ") && !hlsl.contains("s11 = "), "{hlsl}");
}

#[test]
fn a_buffer_used_only_as_a_buffer_is_not_asked_for_its_count() {
    let hlsl = translate(&buffered(&[&BUFFER, &READ])).unwrap().hlsl;
    assert!(!hlsl.contains("GetDimensions"), "{hlsl}");
}

#[test]
fn a_buffer_asked_only_for_its_count_is_declared_for_the_query() {
    // Nothing reads through the descriptor; PSV names t1 a raw buffer.
    let hlsl = translate(&buffered(&[&BUFFER, &[LAST]])).unwrap().hlsl;
    assert!(hlsl.contains("ByteAddressBuffer raw_sbuf1 : register(t1);") && hlsl.contains("raw_sbuf1.GetDimensions(n0);"), "{hlsl}");
}

#[test]
fn a_count_with_no_host_buffer_to_ask_is_refused() {
    let refused = |ex: ShaderExtract| translate(&ex).err().expect("refused");
    let expected = "buffer record count read from a descriptor with no one host buffer";
    // PSV names no buffer at t1.
    assert!(refused(tabled(&[&BUFFER, &[LAST]])).contains(expected));
    // A structured buffer whose stride no record gives: the declared element is a guess.
    let mut guessed = tabled(&[&BUFFER, &[LAST], &READ]);
    guessed.resources = vec![crate::psv::PsvResource { res_type: 5, space: 0, lower: 1, upper: 1 }];
    assert!(refused(guessed.clone()).contains(expected));
    guessed.fssm = fssm([0x300, 8, 1, 12, 0x404]);
    assert!(translate(&guessed).unwrap().hlsl.contains("sbuf1.GetDimensions(n0, st0);"));
}

/// FSSM with one used-resource row and no user-data rows.
fn fssm(used: [u32; 5]) -> Vec<u8> {
    let mut program = vec![0u8; 0x110];
    program[..8].copy_from_slice(b"FSSM\x01\x01\0\0");
    for (rows, field, count) in [(&[][..], 0xe8, 0xf8), (&[used][..], 0xf0, 0xfc)] {
        let offset = (program.len() - 16) as u64;
        program[field..field + 8].copy_from_slice(&offset.to_le_bytes());
        program[count..count + 4].copy_from_slice(&(rows.len() as u32).to_le_bytes());
        program.extend(rows.iter().flatten().copied().flat_map(u32::to_le_bytes));
    }
    let size = (program.len() - 16) as u32;
    program[8..12].copy_from_slice(&size.to_le_bytes());
    program
}

/// The three programs `tools/check_buffer_count.py` runs on the GPU: the
/// count of a raw, a structured (12-byte records) and a typed buffer.
#[test]
fn buffer_record_counts_for_the_gpu_check() {
    let format_load = [0xe0000000, 0x80020000]; // buffer_load_format_x v0, off, s[8:11], 0
    let mut text = String::new();
    for (kind, ty, read, query) in [
        ("raw", 4, READ, "uint n0; raw_sbuf1.GetDimensions(n0);"),
        ("structured", 5, READ, "uint n0, st0; sbuf1.GetDimensions(n0, st0);"),
        ("typed", 3, format_load, "uint n0; stb1.GetDimensions(n0);"),
    ] {
        let mut ex = tabled(&[&BUFFER, &[LAST], &read]);
        ex.resources = vec![crate::psv::PsvResource { res_type: ty, space: 0, lower: 1, upper: 1 }];
        if kind == "structured" {
            // Slot 1 of table 0 (s4): a 12-byte record.
            ex.fssm = fssm([0x300, 8, 1, 12, 0x404]);
        }
        let hlsl = translate(&ex).unwrap().hlsl;
        assert!(hlsl.contains(query) && hlsl.contains("s10 = n0;"), "{kind}: {hlsl}");
        if kind == "structured" {
            assert!(hlsl.contains("struct TbElem_sbuf1 { uint w[3]; };"), "{hlsl}");
        }
        text.push_str(&format!("//// {kind}\n{hlsl}\n"));
    }
    println!("BEGIN_HLSL\n{text}END_HLSL");
}

/// v[54:57] = s[8:11]; s[12:15] = the first lane's v[54:57]: a waterfall's copy and read
/// of the buffer descriptor, which `descriptor_broadcast` proves.
const ROUND_TRIP: [u32; 8] = [0x7e6c0208, 0x7e6e0209, 0x7e70020a, 0x7e72020b, 0x7e180536, 0x7e1a0537, 0x7e1c0538, 0x7e1e0539];
const READ_BACK: [u32; 2] = [0xe0300000, 0x80030000]; // buffer_load_dword v0, off, s[12:15], 0
const LAST_BACK: u32 = 0x8103c10e; // s_add_i32 s3, s14, -1
const SAME_BACK: u32 = 0x7dc4700e; // v_cmp_eq_u64 vcc, s[14:15], v[56:57]

/// A program indexing the resource heap (class 5 base in s20), so that waterfall copies
/// are followed, under the table of `tabled`.
fn heap_indexing(mut ex: ShaderExtract) -> ShaderExtract {
    let mut program = vec![0u8; 0x110];
    program[..8].copy_from_slice(b"FSSM\x01\x01\0\0");
    let offset = (program.len() - 16) as u64;
    program[0xe8..0xf0].copy_from_slice(&offset.to_le_bytes());
    program[0xf8..0xfc].copy_from_slice(&1u32.to_le_bytes());
    program.extend([5u32, 0, 1, 0, 20].into_iter().flat_map(u32::to_le_bytes));
    let size = (program.len() - 16) as u32;
    program[8..12].copy_from_slice(&size.to_le_bytes());
    ex.fssm = program;
    ex.required_features = Some(1 << 25);
    ex.root_sig.as_mut().unwrap().flags = 0x400;
    ex
}

#[test]
fn a_table_descriptor_read_back_from_a_waterfall_copy_is_that_descriptor() {
    // The copy of word 2 moves the descriptor; the read gives it back whole.
    let ex = heap_indexing(buffered(&[&BUFFER, &ROUND_TRIP, &READ_BACK]));
    let hlsl = translate(&ex).unwrap().hlsl;
    assert!(hlsl.contains("raw_sbuf1") && !hlsl.contains("GetDimensions"), "{hlsl}");
}

#[test]
fn a_count_read_back_from_a_waterfall_copy_is_still_a_count() {
    // The word read back is used as a number: the load fills it, and the copies carry it.
    let ex = heap_indexing(buffered(&[&BUFFER, &ROUND_TRIP, &[LAST_BACK]]));
    let hlsl = translate(&ex).unwrap().hlsl;
    assert!(hlsl.contains("raw_sbuf1.GetDimensions(n0);") && hlsl.contains("s10 = n0;"), "{hlsl}");
    // With no host buffer to ask, that use is refused as before.
    let ex = heap_indexing(tabled(&[&BUFFER, &ROUND_TRIP, &[LAST_BACK]]));
    let refusal = translate(&ex).err().expect("refused");
    assert!(refusal.contains("buffer record count read from a descriptor with no one host buffer"), "{refusal}");
    // The loop's test for lanes holding the same descriptor compares the words back,
    // and isn't sure to be a count: a sampler's word 2 is no count at all.
    let ex = heap_indexing(tabled(&[&BUFFER, &ROUND_TRIP, &[SAME_BACK, LAST]]));
    assert!(translate(&ex).is_err(), "the loaded word itself is still a count");
    let ex = heap_indexing(tabled(&[&BUFFER, &ROUND_TRIP, &[SAME_BACK]]));
    assert!(translate(&ex).is_ok());
}
