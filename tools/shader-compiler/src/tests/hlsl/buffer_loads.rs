//! Loads from constant, raw and typed buffers, and stores and atomics to writable
//! ones, for `tools/check_buffer_loads.py`.
//! Encodings are from llvm-mc `-mcpu=gfx1030`.
use crate::meta::{RootParam, RootParamKind, RootRange, RootSig};
use crate::psv::PsvResource;
use crate::{ShaderExtract, Stage};

const END: u32 = 0xbf81_0000;
/// `s_load_dwordx4 s[8:11], s[4:5], 0x20`: the descriptor of slot 1 of the first table.
const DESCRIPTOR: [u32; 2] = [0xf408_0202, 0xfa00_0020];
/// `v_mov_b32 v4, 16`: the byte offset or element index of the vector loads.
const SIXTEEN: u32 = 0x7e08_0290;

/// A compute program under one table of four descriptors of `range_type`, with
/// PSV naming slot 1 a resource of `view` where the root alone does not say.
fn program(range_type: u32, view: Option<u32>, threads: u32, words: &[u32]) -> ShaderExtract {
    // A root-level buffer has no descriptor to load: the program copies its address.
    let root_level = view == Some(ROOT_RAW);
    // A program that reads its own table has no buffer at all.
    let own_table = view == Some(CODE_TABLE);
    let start: &[u32] = if root_level { &[COPY_ROOT] } else if own_table { &[] } else { &DESCRIPTOR };
    let isa: Vec<u8> = [start, words, &[END]].concat().iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut isa_data = isa.clone();
    if own_table {
        isa_data.extend(TABLE.iter().flat_map(|w| w.to_le_bytes()));
    }
    let range = RootRange { range_type, num: 4, base_register: 0, space: 0, offset_in_table: 0 };
    let mut params = vec![RootParam { visibility: 0, kind: RootParamKind::Table(vec![range]) }];
    let mut resources: Vec<PsvResource> = view.filter(|_| !root_level && !own_table)
        .map(|res_type| PsvResource { res_type, space: 0, lower: 1, upper: 1 }).into_iter().collect();
    let mut fssm = Vec::new();
    if root_level {
        // Root parameter 1 is a raw SRV at t5, whose address arrives in s4:s5.
        params.push(RootParam { visibility: 0, kind: RootParamKind::Descriptor { kind: 3, register: 5, space: 0 } });
        resources.push(PsvResource { res_type: 4, space: 0, lower: 5, upper: 5 });
        fssm = records(&[[1, 0, 2, 5, 0x104]], &[[0x100, 0, 5, 1, 0x1_0404]]);
    }
    ShaderExtract {
        flat_inputs: Vec::new(), flat_outputs: Vec::new(), stage: Stage::Compute, shader_model: (6, 0),
        required_features: None, bitcode_stripped: true, fspx_config: None, ps09_pixel_inputs: None,
        fssm, entry: Some("main".into()), isa_data, isa, isa_offset_in_fssm: 0,
        psv_threads: None, isa_preamble: [0, threads, 1, 1], has_input_sig: false, has_output_sig: false,
        has_psv0: false, has_root_sig: true, input_sig: Vec::new(), output_sig: Vec::new(),
        resources,
        gs_info: None, mesh_info: None, primitive_sig: Vec::new(),
        root_sig: Some(RootSig { version: 1, flags: 0, num_static_samplers: 0, static_samplers: Vec::new(), params }),
    }
}

/// A view type that stands for the root-level raw buffer, which PSV names at t5.
const ROOT_RAW: u32 = u32::MAX;
/// `s_mov_b32 s8, s4`: the root buffer's address, where the typed loads look for a descriptor.
const COPY_ROOT: u32 = 0xbe88_0304;

/// A view type that stands for no buffer: the program reads the table after its code.
const CODE_TABLE: u32 = u32::MAX - 1;
/// The words after the code of the `code` programs.
const TABLE: [u32; 8] = [0x7ab1_0000, 0x7ab1_0011, 0x7ab1_0022, 0x7ab1_0033, 0x7ab1_0044, 0x7ab1_0055, 0x7ab1_0066, 0x7ab1_0077];
/// `s_getpc_b64 s[0:1]`, then `v_mov_b32 v4, s0` and `v_mov_b32 v5, s1`: the
/// address of the second instruction, as the vector address of a flat load.
const OWN_ADDRESS: [u32; 3] = [0xbe80_1f00, 0x7e08_0200, 0x7e0a_0201];

/// `s_getpc_b64 s[2:3]`, `s_add_u32 s2, s2, 48` and `s_addc_u32 s3, s3, 0`: the table after
/// the 13-word halfword programs, as the title's own halfword lookups address it. Then the
/// element index in s10 follows.
const HALFWORD_BASE: [u32; 3] = [0xbe82_1f00, 0x8002_b002, 0x8203_8003];
/// `s_mov_b32 s11, 0`, `s_lshl_b64 s[12:13], s[10:11], 1`, `s_add_u32 s14, s12, s2`,
/// `s_addc_u32 s15, s13, s3`, `v_mov_b32 v4, s14`, `v_mov_b32 v5, s15` and
/// `flat_load_ushort v7, v[4:5]`.
const HALFWORD_LOAD: [u32; 8] = [0xbe8b_0380, 0x8f8c_810a, 0x800e_020c, 0x820f_030d, 0x7e08_020e, 0x7e0a_020f,
    0xdc28_0000, 0x077d_0004];

/// An FSSM with user-data and used-resource rows.
fn records(user: &[[u32; 5]], used: &[[u32; 5]]) -> Vec<u8> {
    let mut program = vec![0u8; 0x110];
    program[..8].copy_from_slice(b"FSSM\x01\x01\0\0");
    for (rows, field, count) in [(user, 0xe8, 0xf8), (used, 0xf0, 0xfc)] {
        let offset = (program.len() - 16) as u64;
        program[field..field + 8].copy_from_slice(&offset.to_le_bytes());
        program[count..count + 4].copy_from_slice(&(rows.len() as u32).to_le_bytes());
        program.extend(rows.iter().flatten().copied().flat_map(u32::to_le_bytes));
    }
    let size = (program.len() - 16) as u32;
    program[8..12].copy_from_slice(&size.to_le_bytes());
    program
}

/// One program: its name, the buffer kind the check binds, the root range type
/// and PSV view type of slot 1, the registers holding the result, what they
/// must hold (`pN` is word N of the source buffer, `~x` a float within one unit
/// in the last place of `x`, anything else a literal),
/// and the instructions after the descriptor load.
type Case = (&'static str, &'static str, u32, Option<u32>, &'static str, &'static str, &'static [u32]);

const A: u32 = 0x1122_3344;
const B: u32 = 0x5566_7788;
/// `v_mov_b32 v5..v8, literal`: the data of the stores and atomics.
const DATA: [u32; 8] = [0x7e0a_02ff, A, 0x7e0c_02ff, B, 0x7e0e_02ff, 0x0bad_f00d, 0x7e10_02ff, 0x1234_5678];
/// `buffer_load_dwordx4 v[9:12], v4, s[8:11], 0 offen` and `buffer_load_dword v9, ...`: what a write left.
const BACK4: [u32; 2] = [0xe038_1000, 0x8002_0904];
const BACK1: [u32; 2] = [0xe030_1000, 0x8002_0904];
/// `buffer_load_format_xyzw v[9:12], v4, s[8:11], 0 idxen`.
const BACK_TYPED: [u32; 2] = [0xe00c_2000, 0x8002_0904];

macro_rules! words {
    ($($part:expr),+) => {{
        const PARTS: &[&[u32]] = &[$(&$part),+];
        const LEN: usize = { let mut n = 0; let mut i = 0; while i < PARTS.len() { n += PARTS[i].len(); i += 1; } n };
        const JOINED: [u32; LEN] = {
            let mut out = [0u32; LEN];
            let (mut at, mut i) = (0, 0);
            while i < PARTS.len() { let mut j = 0; while j < PARTS[i].len() { out[at] = PARTS[i][j]; at += 1; j += 1; } i += 1; }
            out
        };
        &JOINED
    }};
}

const CASES: &[Case] = &[
    // s_buffer_load_dword* s20.., s[8:11], 0x10
    ("constant-scalar-x1", "constant", 2, None, "s20", "p4", &[0xf420_0504, 0xfa00_0010]),
    ("constant-scalar-x2", "constant", 2, None, "s20 s21", "p4 p5", &[0xf424_0504, 0xfa00_0010]),
    ("constant-scalar-x4", "constant", 2, None, "s20 s21 s22 s23", "p4 p5 p6 p7", &[0xf428_0504, 0xfa00_0010]),
    ("constant-scalar-x8", "constant", 2, None, "s20 s21 s22 s23 s24 s25 s26 s27", "p4 p5 p6 p7 p8 p9 p10 p11",
     &[0xf42c_0504, 0xfa00_0010]),
    ("constant-scalar-x16", "constant", 2, None,
     "s20 s21 s22 s23 s24 s25 s26 s27 s28 s29 s30 s31 s32 s33 s34 s35",
     "p4 p5 p6 p7 p8 p9 p10 p11 p12 p13 p14 p15 p16 p17 p18 p19", &[0xf430_0504, 0xfa00_0010]),
    ("raw-scalar-x4", "raw", 0, Some(4), "s20 s21 s22 s23", "p4 p5 p6 p7", &[0xf428_0504, 0xfa00_0010]),
    // s_load_dwordx2 s[12:13], s[4:5], 0x0 first: a two-word fetch from the table changes nothing the load reads.
    ("raw-after-fetch-x2", "raw", 0, Some(4), "v7", "p4", &[0xf404_0302, 0xfa00_0000, SIXTEEN, 0xe030_1000, 0x8002_0704]),
    // buffer_load_dword v7, off, s[8:11], 0 offset:8
    ("raw-vector-fixed", "raw", 0, Some(4), "v7", "p2", &[0xe030_0008, 0x8002_0700]),
    // buffer_load_dword* v7.., v4, s[8:11], 0 offen
    ("raw-vector-x1", "raw", 0, Some(4), "v7", "p4", &[SIXTEEN, 0xe030_1000, 0x8002_0704]),
    ("raw-vector-x2", "raw", 0, Some(4), "v7 v8", "p4 p5", &[SIXTEEN, 0xe034_1000, 0x8002_0704]),
    ("raw-vector-x3", "raw", 0, Some(4), "v7 v8 v9", "p4 p5 p6", &[SIXTEEN, 0xe03c_1000, 0x8002_0704]),
    ("raw-vector-x4", "raw", 0, Some(4), "v7 v8 v9 v10", "p4 p5 p6 p7", &[SIXTEEN, 0xe038_1000, 0x8002_0704]),
    ("constant-vector-x4", "constant", 2, None, "v7 v8 v9 v10", "p4 p5 p6 p7", &[SIXTEEN, 0xe038_1000, 0x8002_0704]),
    // buffer_load_format_* v7.., v4, s[8:11], 0 idxen: element 16 of four-word elements
    ("typed-x", "typed", 0, Some(3), "v7", "p64", &[SIXTEEN, 0xe000_2000, 0x8002_0704]),
    ("typed-xy", "typed", 0, Some(3), "v7 v8", "p64 p65", &[SIXTEEN, 0xe004_2000, 0x8002_0704]),
    ("typed-xyz", "typed", 0, Some(3), "v7 v8 v9", "p64 p65 p66", &[SIXTEEN, 0xe008_2000, 0x8002_0704]),
    ("typed-xyzw", "typed", 0, Some(3), "v7 v8 v9 v10", "p64 p65 p66 p67", &[SIXTEEN, 0xe00c_2000, 0x8002_0704]),
    // buffer_load_format_d16_*: each channel as an f16, two to a register. The
    // four floats of element 16 all round to 0x4802.
    ("typed-d16-x", "typed", 0, Some(3), "v7", "0x4802", &[SIXTEEN, 0xe200_2000, 0x8002_0704]),
    ("typed-d16-xyzw", "typed", 0, Some(3), "v7 v8", "0x48024802 0x48024802", &[SIXTEEN, 0xe20c_2000, 0x8002_0704]),
    // tbuffer_load_format_* v7.., v4, s[8:11], 0 offen from the root-level buffer:
    // word 4 is 0x41000404, and each format unpacks it its own way.
    ("root-32-float", "root-raw", 0, Some(ROOT_RAW), "v7", "p4", &[SIXTEEN, 0xe8b0_1000, 0x8002_0704]),
    ("root-32-uint", "root-raw", 0, Some(ROOT_RAW), "v7", "p4", &[SIXTEEN, 0xe8a0_1000, 0x8002_0704]),
    // Channels the format lacks read as 0, 0 and 1.
    ("root-32-float-xyzw", "root-raw", 0, Some(ROOT_RAW), "v7 v8 v9 v10", "p4 0x0 0x0 0x3f800000",
     &[SIXTEEN, 0xe8b3_1000, 0x8002_0704]),
    ("root-16-16-uint", "root-raw", 0, Some(ROOT_RAW), "v7 v8", "0x404 0x4100", &[SIXTEEN, 0xe8d9_1000, 0x8002_0704]),
    ("root-16-16-unorm", "root-raw", 0, Some(ROOT_RAW), "v7 v8", "~0x3c808081 ~0x3e820082",
     &[SIXTEEN, 0xe8b9_1000, 0x8002_0704]),
    ("root-16-16-snorm", "root-raw", 0, Some(ROOT_RAW), "v7 v8", "~0x3d008101 ~0x3f020104",
     &[SIXTEEN, 0xe8c1_1000, 0x8002_0704]),
    ("root-16-16-float", "root-raw", 0, Some(ROOT_RAW), "v7 v8", "0x38808000 0x40200000",
     &[SIXTEEN, 0xe8e9_1000, 0x8002_0704]),
    ("root-16-float", "root-raw", 0, Some(ROOT_RAW), "v7", "0x38808000", &[SIXTEEN, 0xe868_1000, 0x8002_0704]),
    ("root-8-8-8-8-uint", "root-raw", 0, Some(ROOT_RAW), "v7 v8 v9 v10", "0x4 0x4 0x0 0x41",
     &[SIXTEEN, 0xe9e3_1000, 0x8002_0704]),
    ("root-8-8-8-8-unorm", "root-raw", 0, Some(ROOT_RAW), "v7 v8 v9 v10", "~0x3c808081 ~0x3c808081 0x0 ~0x3e828283",
     &[SIXTEEN, 0xe9c3_1000, 0x8002_0704]),
    ("root-2-10-10-10-unorm", "root-raw", 0, Some(ROOT_RAW), "v7 v8 v9 v10",
     "~0x3b802008 ~0x3a802008 ~0x3c802008 ~0x3eaaaaab", &[SIXTEEN, 0xe993_1000, 0x8002_0704]),
    ("root-2-10-10-10-unorm-xyz", "root-raw", 0, Some(ROOT_RAW), "v7 v8 v9", "~0x3b802008 ~0x3a802008 ~0x3c802008",
     &[SIXTEEN, 0xe992_1000, 0x8002_0704]),
    // Two words: four 16-bit channels.
    ("root-16-16-16-16-uint", "root-raw", 0, Some(ROOT_RAW), "v7 v8 v9 v10", "0x404 0x4100 0x505 0x4100",
     &[SIXTEEN, 0xea2b_1000, 0x8002_0704]),
    // flat_load_dword* v7.., v[4:5] offset:24. The six-word program ends at byte 24
    // and the address is byte 4, so the load starts at the table's second word.
    ("code-x1", "code", 0, Some(CODE_TABLE), "v7", "0x7ab10011", words!(OWN_ADDRESS, [0xdc30_0018, 0x077d_0004])),
    ("code-x2", "code", 0, Some(CODE_TABLE), "v7 v8", "0x7ab10011 0x7ab10022", words!(OWN_ADDRESS, [0xdc34_0018, 0x077d_0004])),
    ("code-x3", "code", 0, Some(CODE_TABLE), "v7 v8 v9", "0x7ab10011 0x7ab10022 0x7ab10033",
     words!(OWN_ADDRESS, [0xdc3c_0018, 0x077d_0004])),
    ("code-x4", "code", 0, Some(CODE_TABLE), "v7 v8 v9 v10", "0x7ab10011 0x7ab10022 0x7ab10033 0x7ab10044",
     words!(OWN_ADDRESS, [0xdc38_0018, 0x077d_0004])),
    // flat_load_ushort v7, v[4:5] of halfword 2 and 3 of the table: each half of its second word.
    ("code-ushort-low", "code", 0, Some(CODE_TABLE), "v7", "0x11", words!(HALFWORD_BASE, [0xbe8a_0382], HALFWORD_LOAD)),
    ("code-ushort-high", "code", 0, Some(CODE_TABLE), "v7", "0x7ab1", words!(HALFWORD_BASE, [0xbe8a_0383], HALFWORD_LOAD)),
    // buffer_store_dword* v5.., v4, s[8:11], 0 offen, then the four words read back
    ("store-x1", "raw-write", 1, Some(7), "v9 v10 v11 v12", "0x11223344 p5 p6 p7",
     words!([SIXTEEN], DATA, [0xe070_1000, 0x8002_0504], BACK4)),
    ("store-x2", "raw-write", 1, Some(7), "v9 v10 v11 v12", "0x11223344 0x55667788 p6 p7",
     words!([SIXTEEN], DATA, [0xe074_1000, 0x8002_0504], BACK4)),
    ("store-x3", "raw-write", 1, Some(7), "v9 v10 v11 v12", "0x11223344 0x55667788 0x0badf00d p7",
     words!([SIXTEEN], DATA, [0xe07c_1000, 0x8002_0504], BACK4)),
    ("store-x4", "raw-write", 1, Some(7), "v9 v10 v11 v12", "0x11223344 0x55667788 0x0badf00d 0x12345678",
     words!([SIXTEEN], DATA, [0xe078_1000, 0x8002_0504], BACK4)),
    // buffer_store_format_xyzw v[5:8], v4, s[8:11], 0 idxen
    ("store-typed-xyzw", "typed-write", 1, Some(6), "v9 v10 v11 v12", "0x11223344 0x55667788 0x0badf00d 0x12345678",
     words!([SIXTEEN], DATA, [0xe01c_2000, 0x8002_0504], BACK_TYPED)),
    // buffer_store_format_d16_xyzw v[5:6]: four f16 channels, read back as floats.
    ("store-typed-d16-xyzw", "typed-write", 1, Some(6), "v9 v10 v11 v12", "0x3e688000 0x3a244000 0x46f10000 0x42acc000",
     words!([SIXTEEN], DATA, [0xe21c_2000, 0x8002_0504], BACK_TYPED)),
    // buffer_atomic_* v5, v4, s[8:11], 0 offen on word 4, which holds 0x41000404
    ("atomic-add", "raw-write", 1, Some(7), "v9", "0x52223748", words!([SIXTEEN], DATA, [0xe0c8_1000, 0x8002_0504], BACK1)),
    // With GLC the data register receives the word as it was.
    ("atomic-add-return", "raw-write", 1, Some(7), "v9 v5", "0x52223748 p4",
     words!([SIXTEEN], DATA, [0xe0c8_5000, 0x8002_0504], BACK1)),
    ("atomic-umin", "raw-write", 1, Some(7), "v9", "0x11223344", words!([SIXTEEN], DATA, [0xe0d8_1000, 0x8002_0504], BACK1)),
    ("atomic-umax", "raw-write", 1, Some(7), "v9", "0x55667788",
     words!([SIXTEEN], [0x7e0a_02ff, B], [0xe0e0_1000, 0x8002_0504], BACK1)),
    ("atomic-smin", "raw-write", 1, Some(7), "v9", "0xf0000001",
     words!([SIXTEEN], [0x7e0a_02ff, 0xf000_0001], [0xe0d4_1000, 0x8002_0504], BACK1)),
    ("atomic-smax", "raw-write", 1, Some(7), "v9", "0x55667788",
     words!([SIXTEEN], [0x7e0a_02ff, B], [0xe0dc_1000, 0x8002_0504], BACK1)),
    // A negative operand is below a positive word when signed, and above it when unsigned.
    ("atomic-smax-negative", "raw-write", 1, Some(7), "v9", "p4",
     words!([SIXTEEN], [0x7e0a_02ff, 0xf000_0001], [0xe0dc_1000, 0x8002_0504], BACK1)),
    ("atomic-umax-high", "raw-write", 1, Some(7), "v9", "0xf0000001",
     words!([SIXTEEN], [0x7e0a_02ff, 0xf000_0001], [0xe0e0_1000, 0x8002_0504], BACK1)),
    ("atomic-or", "raw-write", 1, Some(7), "v9", "0x51223744", words!([SIXTEEN], DATA, [0xe0e8_1000, 0x8002_0504], BACK1)),
    ("atomic-and", "raw-write", 1, Some(7), "v9", "0x01000004", words!([SIXTEEN], DATA, [0xe0e4_1000, 0x8002_0504], BACK1)),
    ("atomic-swap", "raw-write", 1, Some(7), "v9", "0x11223344", words!([SIXTEEN], DATA, [0xe0c0_1000, 0x8002_0504], BACK1)),
    // buffer_atomic_cmpswap v[5:6]: v5 is stored where the word equals v6.
    ("atomic-cmpswap-equal", "raw-write", 1, Some(7), "v9", "0x11223344",
     words!([SIXTEEN], [0x7e0a_02ff, A, 0x7e0c_02ff, 0x4100_0404], [0xe0c4_1000, 0x8002_0504], BACK1)),
    ("atomic-cmpswap-differs", "raw-write", 1, Some(7), "v9", "p4",
     words!([SIXTEEN], DATA, [0xe0c4_1000, 0x8002_0504], BACK1)),
];

/// The programs `tools/check_buffer_loads.py` runs on the GPU.
#[test]
fn buffer_loads_for_the_gpu_check() {
    let mut text = String::new();
    for &(name, kind, range_type, view, results, expected, words) in CASES {
        // A write is checked by reading it back, so one thread runs it.
        let threads = if kind.ends_with("-write") { 1 } else { 32 };
        let hlsl = super::translate(&program(range_type, view, threads, words)).unwrap_or_else(|why| panic!("{name}: {why}")).hlsl;
        let declared = match kind {
            "constant" => "cbuffer CB1 : register(b1)",
            "raw" => "ByteAddressBuffer raw_sbuf1 : register(t1);",
            "typed" => "Buffer<float4> stb1 : register(t1);",
            "raw-write" => "RWByteAddressBuffer raw_ubuf1 : register(u1);",
            "root-raw" => "ByteAddressBuffer raw_sbuf5 : register(t5);",
            "code" => "tb_code[",
            _ => "RWBuffer<float4> utb1 : register(u1);",
        };
        assert_eq!(results.split(' ').count(), expected.split(' ').count(), "{name}");
        assert!(hlsl.contains(declared), "{name}: {hlsl}");
        for register in results.split(' ') {
            assert!(hlsl.contains(&format!("{register} = ")), "{name} never writes {register}: {hlsl}");
        }
        text.push_str(&format!("//// {name} {kind} {results} | {expected}\n{hlsl}\n"));
    }
    println!("BEGIN_HLSL\n{text}END_HLSL");
}

#[test]
fn a_load_to_group_memory_is_refused() {
    // flat_load_dword v7, v[4:5] offset:24 from the program's own table, then with LDS.
    let code = |first: u32| program(0, Some(CODE_TABLE), 32, &[OWN_ADDRESS[0], OWN_ADDRESS[1], OWN_ADDRESS[2], first, 0x077d_0004]);
    assert!(super::translate(&code(0xdc30_0018)).is_ok());
    let error = super::translate(&code(0xdc30_2018)).err().unwrap();
    assert!(error.contains("FLAT LDS"), "{error}");
}


#[test]
fn halfword_lookup_encodings() {
    let words = [&HALFWORD_BASE[..], &[0xbe8a_0382], &HALFWORD_LOAD[..], &[END]].concat();
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let names: Vec<String> = crate::rdna2::disassemble(&bytes).into_iter().map(|i| i.mnemonic).collect();
    assert_eq!(names, ["s_getpc_b64", "s_add_u32", "s_addc_u32", "s_mov_b32", "s_mov_b32", "s_lshl_b64", "s_add_u32",
        "s_addc_u32", "v_mov_b32", "v_mov_b32", "flat_load_ushort", "s_endpgm"]);
    // The table starts where the code ends.
    assert_eq!(bytes.len(), 4 + 48);
}
