//! Synthetic pixel programs that read interpolated attributes, for GPU readback.
//!
//! Each program reads channels of two four-component attributes and exports
//! their bits through an unsigned `SV_Target`, so the check compares the
//! floats the rasterizer delivered. Encodings are from llvm-mc `-mcpu=gfx1030`.
use tuxbox_xshader::meta::SigElement;
use tuxbox_xshader::{hlsl, ShaderExtract, Stage};

/// `smooth` interpolates with the two barycentrics in v0 and v1. `flat` reads
/// the first vertex's value and the two differences. `half` interpolates
/// half-float attributes.
fn shader(variant: &str) -> ShaderExtract {
    let (words, half): (&[u32], bool) = match variant {
        "smooth" => (&[
            0xc810_0000, 0xc811_0001, // v_interp_p1_f32 v4, v0, attr0.x; v_interp_p2_f32 v4, v1, attr0.x
            0xc814_0100, 0xc815_0101, // v5 = attr0.y
            0xc818_0600, 0xc819_0601, // v6 = attr1.z
            0xc81c_0700, 0xc81d_0701, // v7 = attr1.w
            0xf800_180f, 0x0706_0504, // exp mrt0 v4, v5, v6, v7 done vm
        ], false),
        "flat" => (&[
            0xc812_0002, // v_interp_mov_f32 v4, p0, attr0.x
            0xc816_0702, // v5 = p0 of attr1.w
            0xc81a_0700, // v6 = p10 of attr1.w
            0xc81e_0701, // v7 = p20 of attr1.w
            0xf800_180f, 0x0706_0504,
        ], false),
        "half" => (&[
            0xd742_0004, 0x0202_0000, // v_interp_p1ll_f16 v4, v0, attr0.x
            0xd75a_0004, 0x0412_0200, // v_interp_p2_f16 v4, v1, attr0.x, v4
            0x7e0a_0280, 0x7e0c_0280, 0x7e0e_0280, // v5 = v6 = v7 = 0
            0xf800_180f, 0x0706_0504,
        ], true),
        _ => panic!("variant must be smooth, flat or half"),
    };
    let isa: Vec<u8> = words.iter().chain(&[0xbf81_0000]).flat_map(|w| w.to_le_bytes()).collect();
    let attribute = |index: u32| SigElement {
        name: "TEXCOORD".into(), index, sysval: 0, comp_type: if half { 6 } else { 3 }, register: 1 + index, mask: 15,
        rw_mask: 15,
    };
    ShaderExtract {
        stage: Stage::Pixel, shader_model: (6, 6), required_features: None, bitcode_stripped: true,
        fspx_config: None, ps09_pixel_inputs: None, fssm: Vec::new(), entry: Some("main".into()),
        isa_data: isa.clone(), isa, isa_offset_in_fssm: 0, psv_threads: None, flat_inputs: Vec::new(),
        flat_outputs: Vec::new(), isa_preamble: [0, 64, 1, 1], has_input_sig: true, has_output_sig: true,
        has_psv0: false, has_root_sig: false,
        input_sig: vec![
            SigElement { name: "SV_Position".into(), index: 0, sysval: 1, comp_type: 3, register: 0, mask: 15, rw_mask: 15 },
            attribute(0), attribute(1),
        ],
        output_sig: vec![SigElement { name: "SV_Target".into(), index: 0, sysval: 64, comp_type: 1, register: 0,
            mask: 15, rw_mask: 15 }],
        resources: Vec::new(), gs_info: None, mesh_info: None, primitive_sig: Vec::new(), root_sig: None,
    }
}

fn main() {
    let variant = std::env::args().nth(1).expect("variant: smooth, flat or half");
    match hlsl::translate(&shader(&variant)) {
        Ok(translated) => print!("{}", translated.hlsl),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(65);
        }
    }
}
