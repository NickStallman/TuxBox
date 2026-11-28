//! Image samples, gathers, loads, queries and atomics, for `tools/check_image_ops.py`.
//! Encodings are from llvm-mc `-mcpu=gfx1030`.
use crate::meta::{RootParam, RootParamKind, RootRange, RootSig};
use crate::{ShaderExtract, Stage};

const END: u32 = 0xbf81_0000;
/// `s_load_dwordx8 s[8:15], s[4:5], 0x20` and `s_load_dwordx4 s[16:19], s[6:7], 0x10`:
/// the image in slot 1 of the first table and the sampler in slot 1 of the second.
const DESCRIPTORS: [u32; 4] = [0xf40c_0202, 0xfa00_0020, 0xf408_0403, 0xfa00_0010];

/// `v_mov_b32 v{4 + n}, literal`.
const fn set(n: u32, value: u32) -> [u32; 2] {
    [0x7e08_02ff + (n << 17), value]
}

const U: u32 = 0x3ea0_0000; // 0.3125: texel 2 of 8, texel 1 of 4
const V: u32 = 0x3f10_0000; // 0.5625: texel 4 of 8, texel 2 of 4
const CORNER_U: u32 = 0x3ec0_0000; // 0.375: between texels 2 and 3
const CORNER_V: u32 = 0x3f20_0000; // 0.625: between texels 4 and 5
const ONE: u32 = 0x3f80_0000;

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

/// One program: its name, what the check binds (`float`, `compare` or `uint`),
/// the result registers, what they must hold, and the instructions after the
/// descriptor loads. Results go to v12 on. In the expectation `x,y,m` is the
/// red channel of texel (x, y) of mip m, `g:`, `b:` and `a:` the other
/// channels, and anything else a literal word.
type Case = (&'static str, &'static str, &'static str, &'static str, &'static [u32]);

const CASES: &[Case] = &[
    // image_sample_lz v[12:15], v[4:5]
    ("sample-lz", "float", "v12 v13 v14 v15", "2,4,0 g:2,4,0 b:2,4,0 a:2,4,0",
     words!(set(0, U), set(1, V), [0xf09c_0f08, 0x0082_0c04])),
    // image_sample_l v[12:15], v[4:6]: the third coordinate is the level.
    ("sample-l", "float", "v12 v13 v14 v15", "1,2,1 g:1,2,1 b:1,2,1 a:1,2,1",
     words!(set(0, U), set(1, V), set(2, ONE), [0xf090_0f08, 0x0082_0c04])),
    // image_sample_d v[12:15], v[4:9]: four derivatives, then the coordinates.
    ("sample-d", "float", "v12 v13", "2,4,0 g:2,4,0",
     words!(set(0, 0), set(1, 0), set(2, 0), set(3, 0), set(4, U), set(5, V), [0xf088_0f08, 0x0082_0c04])),
    // image_sample_d_cl v[12:15], v[4:10]: the last is the lowest level allowed.
    ("sample-d-cl", "float", "v12 v13", "1,2,1 g:1,2,1",
     words!(set(0, 0), set(1, 0), set(2, 0), set(3, 0), set(4, U), set(5, V), set(6, ONE), [0xf08c_0f08, 0x0082_0c04])),
    // image_gather4_lz v[12:15], v[4:5] dmask:1: the red channel of the four texels around a corner.
    ("gather4-lz", "float", "v12 v13 v14 v15", "2,5,0 3,5,0 3,4,0 2,4,0",
     words!(set(0, CORNER_U), set(1, CORNER_V), [0xf11c_0108, 0x0082_0c04])),
    // dmask:2 gathers green.
    ("gather4-lz-green", "float", "v12 v13 v14 v15", "g:2,5,0 g:3,5,0 g:3,4,0 g:2,4,0",
     words!(set(0, CORNER_U), set(1, CORNER_V), [0xf11c_0208, 0x0082_0c04])),
    ("gather4-l", "float", "v12 v13 v14 v15", "1,3,1 2,3,1 2,2,1 1,2,1",
     words!(set(0, CORNER_U), set(1, CORNER_V), set(2, ONE), [0xf110_0108, 0x0082_0c04])),
    // The _o forms take a packed texel offset first: one texel to the right.
    ("gather4-lz-o", "float", "v12 v13 v14 v15", "3,5,0 4,5,0 4,4,0 3,4,0",
     words!(set(0, 1), set(1, CORNER_U), set(2, CORNER_V), [0xf15c_0108, 0x0082_0c04])),
    ("gather4-l-o", "float", "v12 v13 v14 v15", "2,3,1 3,3,1 3,2,1 2,2,1",
     words!(set(0, 1), set(1, CORNER_U), set(2, CORNER_V), set(3, ONE), [0xf150_0108, 0x0082_0c04])),
    // image_load v[12:15], v[4:5] and image_load_mip v[12:15], v[4:6] take integer texels.
    ("load", "float", "v12 v13 v14 v15", "2,4,0 g:2,4,0 b:2,4,0 a:2,4,0", words!(set(0, 2), set(1, 4), [0xf000_0f08, 0x0002_0c04])),
    ("load-mip", "float", "v12 v13 v14 v15", "2,1,1 g:2,1,1 b:2,1,1 a:2,1,1",
     words!(set(0, 2), set(1, 1), set(2, 1), [0xf004_0f08, 0x0002_0c04])),
    // image_get_resinfo v[12:14], v4 dmask:0xb: width, height and level count of a level.
    ("resinfo", "float", "v12 v13 v14", "0x8 0x8 0x2", words!(set(0, 0), [0xf038_0b08, 0x0002_0c04])),
    ("resinfo-level-1", "float", "v12 v13 v14", "0x4 0x4 0x2", words!(set(0, 1), [0xf038_0b08, 0x0002_0c04])),
    // image_sample_c_lz v12, v[4:6]: the reference comes first. The sampler passes
    // where the reference is at most the texel, which holds 42.
    ("sample-c-lz-pass", "compare", "v12", "0x3f800000",
     words!(set(0, 0x4224_0000), set(1, U), set(2, V), [0xf0bc_0108, 0x0082_0c04])),
    ("sample-c-lz-fail", "compare", "v12", "0x0",
     words!(set(0, 0x422c_0000), set(1, U), set(2, V), [0xf0bc_0108, 0x0082_0c04])),
    // image_sample_b v[12:15], v[4:6] and image_get_lod v[12:13], v[4:5] take the level
    // from the coordinates of neighbouring threads. Every thread has the same ones,
    // so the level is the most detailed: a bias can't lift it.
    ("implicit-sample-b", "float", "v12 v13", "2,4,0 g:2,4,0",
     words!(set(0, ONE), set(1, U), set(2, V), [0xf094_0f08, 0x0082_0c04])),
    ("implicit-get-lod", "float", "v12", "0x0", words!(set(0, U), set(1, V), [0xf180_0308, 0x0082_0c04])),
    // image_atomic_* v7, v[4:5] on texel (2, 4), which holds 0x1042, then image_load of it.
    ("atomic-or", "uint", "v12", "0x1f4f", words!(set(0, 2), set(1, 4), set(3, 0x0f0f), [0xf064_0108, 0x0002_0704], [0xf000_0f08, 0x0002_0c04])),
    ("atomic-umin", "uint", "v12", "0x10", words!(set(0, 2), set(1, 4), set(3, 0x10), [0xf054_0108, 0x0002_0704], [0xf000_0f08, 0x0002_0c04])),
    ("atomic-umax", "uint", "v12", "0xffff", words!(set(0, 2), set(1, 4), set(3, 0xffff), [0xf05c_0108, 0x0002_0704], [0xf000_0f08, 0x0002_0c04])),
    ("atomic-add", "uint", "v12", "0x1047", words!(set(0, 2), set(1, 4), set(3, 5), [0xf044_0108, 0x0002_0704], [0xf000_0f08, 0x0002_0c04])),
];

fn program(kind: &str, words: &[u32]) -> ShaderExtract {
    let isa: Vec<u8> = [&DESCRIPTORS[..], words, &[END]].concat().iter().flat_map(|w| w.to_le_bytes()).collect();
    // An atomic is checked by loading the texel again, so one thread runs it.
    let (range_type, threads) = if kind == "uint" { (1, 1) } else { (0, 32) };
    // Implicit levels need a thread group the host can split into quads.
    let implicit = words.iter().any(|&word| word == 0xf094_0f08 || word == 0xf180_0308);
    let table = |range_type| RootParam { visibility: 0, kind: RootParamKind::Table(vec![
        RootRange { range_type, num: 4, base_register: 0, space: 0, offset_in_table: 0 }]) };
    ShaderExtract {
        flat_inputs: Vec::new(), flat_outputs: Vec::new(), stage: Stage::Compute, shader_model: (6, 0),
        required_features: None, bitcode_stripped: true, fspx_config: None, ps09_pixel_inputs: None,
        fssm: Vec::new(), entry: Some("main".into()), isa_data: isa.clone(), isa, isa_offset_in_fssm: 0,
        psv_threads: implicit.then_some([threads, 1, 1]), isa_preamble: [0, threads, 1, 1], has_input_sig: false, has_output_sig: false,
        has_psv0: false, has_root_sig: true, input_sig: Vec::new(), output_sig: Vec::new(), resources: Vec::new(),
        gs_info: None, mesh_info: None, primitive_sig: Vec::new(),
        root_sig: Some(RootSig { version: 1, flags: 0, num_static_samplers: 0, static_samplers: Vec::new(),
            params: vec![table(range_type), table(3)] }),
    }
}

/// The programs `tools/check_image_ops.py` runs on the GPU.
#[test]
fn image_ops_for_the_gpu_check() {
    let mut text = String::new();
    for &(name, kind, results, expected, words) in CASES {
        let hlsl = super::translate(&program(kind, words)).unwrap_or_else(|why| panic!("{name}: {why}")).hlsl;
        assert_eq!(results.split(' ').count(), expected.split(' ').count(), "{name}");
        for register in results.split(' ') {
            assert!(hlsl.contains(&format!("{register} = ")), "{name} never writes {register}: {hlsl}");
        }
        text.push_str(&format!("//// {name} {kind} {results} | {expected}\n{hlsl}\n"));
    }
    println!("BEGIN_HLSL\n{text}END_HLSL");
}
