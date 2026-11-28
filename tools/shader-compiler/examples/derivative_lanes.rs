//! Translate a synthetic compute program that samples at its own thread coordinates with
//! an implicit derivative, then exchanges within its quad and reads one lane.
//! Argument: the thread group as `XxY`.
use tuxbox_xshader::{ShaderExtract, Stage, hlsl};

mod fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/support/wave_guard_fixture.rs"));
}

/// v_readfirstlane_b32 s24, v2; s_lshl_b32 s24, s24, 5; s_load_dwordx8 s[32:39], s[10:11], s24;
/// s_lshl_b32 s25, s24, 4; s_load_dwordx4 s[40:43], s[14:15], s25;
/// v_cvt_f32_u32 v4, v0; v_cvt_f32_u32 v5, v1;
/// image_sample v10, v[4:5], s[32:39], s[40:43] dmask:0x1 dim:SQ_RSRC_IMG_2D;
/// ds_swizzle_b32 v11, v0 offset:0x801b; v_readlane_b32 s20, v0, 5; v_mov_b32 v12, s20; s_endpgm
const WORDS: [u32; 17] = [
    0x7e300502, 0x8f188518, 0xf40c0805, 0x30000000, 0x8f198418, 0xf4080a07, 0x32000000, 0x7e080d00, 0x7e0a0d01,
    0xf0800108, 0x01480a04, 0xd8d4801b, 0x0b000000, 0xd7600014, 0x00010b00, 0x7e180214, 0xbf810000,
];

fn main() {
    let group = std::env::args().nth(1).expect("thread group as XxY");
    let (x, y) = group.split_once('x').expect("XxY");
    let (x, y): (u32, u32) = (x.parse().expect("integer"), y.parse().expect("integer"));
    let (mut shader, _, _): (ShaderExtract, _, _) = fixture::shader(64);
    assert!(matches!(shader.stage, Stage::Compute));
    shader.isa = WORDS.iter().flat_map(|word| word.to_le_bytes()).collect();
    shader.isa_data = shader.isa.clone();
    shader.isa_preamble = [0, x, y, 1];
    shader.psv_threads = Some([x, y, 1]);
    // The resource heap's base in s10 and the sampler heap's in s14, with a root that indexes the heaps.
    let mut fssm = vec![0u8; 0x100];
    fssm[..8].copy_from_slice(b"FSSM\x01\x01\0\0");
    fssm[0xe8..0xf0].copy_from_slice(&0xf0u64.to_le_bytes());
    fssm[0xf8..0xfc].copy_from_slice(&2u32.to_le_bytes());
    fssm.extend([5u32, 0, 1, 0, 10, 6, 0, 1, 0, 14].into_iter().flat_map(u32::to_le_bytes));
    let size = (fssm.len() - 16) as u32;
    fssm[8..12].copy_from_slice(&size.to_le_bytes());
    shader.fssm = fssm;
    shader.shader_model = (6, 6);
    shader.required_features = Some(1 << 25);
    shader.has_root_sig = true;
    shader.root_sig = Some(tuxbox_dxbc::rootsig::RootSig {
        version: 1, flags: 0x400, params: Vec::new(), num_static_samplers: 0, static_samplers: Vec::new(),
    });
    match hlsl::translate(&shader) {
        Ok(result) => print!("{}", result.hlsl),
        Err(reason) => {
            eprintln!("{reason}");
            std::process::exit(65);
        }
    }
}
