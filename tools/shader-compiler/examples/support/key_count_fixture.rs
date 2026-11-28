// A synthetic compute program that counts its lanes by key, as ray binning
// does: one pass per key, a 64-bit sum over the wave, and one lane adding the
// count to a typed uint UAV. Lane keys are in v3. Key 0xffff does not count.
// Lane masks take register pairs, as a 64-lane wave needs.
use super::{PsvResource, RootParam, RootParamKind, RootRange, RootSig, ShaderExtract, Stage};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Valid {
    /// A mask computed before the loop, in s12.
    Before,
    /// A comparison inside the loop.
    Inside,
}

const ENDPGM: u32 = 0xbf81_0000;

fn smov(wide: bool, dst: u32, src: u32) -> u32 {
    0xbe80_0300 + (u32::from(wide) << 8) | dst << 16 | src
}

fn vmov(dst: u32, src: u32) -> u32 {
    0x7e00_0200 | dst << 17 | src
}

/// `words[at]` becomes a branch with opcode `op` to instruction word `to`.
fn branch(words: &mut [u32], at: usize, op: u32, to: usize) {
    words[at] = 0xbf80_0000 | op << 16 | (to as isize - at as isize - 1) as u16 as u32;
}

/// The 64-bit sum of v10:v9 over the wave with v11 and v12 as temporaries,
/// read into s20 and s21.
fn wave_sum(wide: bool) -> Vec<u32> {
    let (a, b, t, u) = (9, 10, 11, 12);
    let add = |low: u32, high: u32| [0xd70f_6a00 | a, (256 | a) << 9 | low, 0x5000_0000 | b << 17 | b << 9 | high];
    let mut words = Vec::new();
    for select in [0xde54c1, 0x936012, 0x000924] {
        words.extend([0x7e00_02e9 | t << 17, select << 8 | a, 0x7e00_02e9 | u << 17, select << 8 | b]);
        words.extend(add(256 | t, 256 | u));
    }
    words.extend([0x7e00_02fa | t << 17, 0xff09_4000 | a, 0x7e00_02fa | u << 17, 0xff09_4000 | b]);
    words.extend(add(256 | t, 256 | u));
    words.extend([0xd778_0000 | t, 0x0201_0100 | a, 0xd778_0000 | u, 0x0201_0100 | b]);
    words.extend(add(256 | t, 256 | u));
    let read = |dst: u32, source: u32, lane: u32| [0xd760_0000 | dst, (128 + lane) << 9 | 256 | source];
    if wide {
        words.extend(read(20, a, 32));
        words.extend(read(21, b, 32));
        words.extend(add(20, 21));
    }
    words.extend(read(20, a, 0));
    words.extend(read(21, b, 0));
    words
}

/// The program. With `masked`, only lanes whose key is not 3 enter the loop.
/// `after` runs between the loop and the end, with EXEC as the loop left it.
pub fn words(wide: bool, valid: Valid, masked: bool, after: &[u32]) -> Vec<u32> {
    let w = u32::from(wide);
    let save = |dst: u32, src: u32| 0xbe80_3c00 - 0x1800 * w | dst << 16 | src; // s_and_saveexec
    let vcc = 106;
    let mut code = vec![
        0xf408_0202, 0xfa00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    if masked {
        code.extend([0x7d8a_0683, save(32, vcc)]); // v_cmp_ne_u32 vcc, 3, v3
    }
    if valid == Valid::Before {
        code.extend([0xd4c5_000c, 0x0002_06ff, 0xffff]); // v_cmp_ne_u32 s12, 0xffff, v3
    }
    code.push(vmov(6, 129));
    let head = code.len();
    code.extend([
        0x7d8a_0c80, // v_cmp_ne_u32 vcc, 0, v6
        vmov(6, 128),
        save(14, vcc),
    ]);
    let skip = code.len();
    code.extend([0, 0x7e36_0503]); // s_cbranch_execz exit; v_readfirstlane_b32 s27, v3
    if valid == Valid::Inside {
        code.extend([0x7d8a_06ff, 0xffff]); // v_cmp_ne_u32 vcc, 0xffff, v3
    }
    code.extend([
        0xd4c2_0010, 0x0002_061b, // v_cmp_eq_u32 s16, s27, v3
        0x871c_1000 | w << 23 | if valid == Valid::Inside { vcc } else { 12 }, // s_and s28, valid, s16
        save(18, 28),
    ]);
    let empty = code.len();
    code.extend([
        0, // s_cbranch_execz tail
        vmov(7, 129),
        vmov(8, 128),
        0xbe9e_3dc1 - 0x1800 * w, // s_or_saveexec s30, -1
        vmov(9, 128),
        vmov(10, 128),
        0xd501_0009, 30 << 18 | 0x107 << 9 | 0x109, // v_cndmask_b32 v9, v9, v7, s30
        0xd501_000a, 30 << 18 | 0x108 << 9 | 0x10a,
    ]);
    code.extend(wave_sum(wide));
    code.extend([
        smov(wide, 126, 30),
        0xd765_000d, 0x0001_00c1,   // v_mbcnt_lo_u32_b32 v13, -1, 0
        0xbe96_137e + (w << 8),     // s_ff1_i32 s22, exec
        smov(wide, 24, 126),
    ]);
    if wide {
        code.extend([0xd766_000d, 0x0002_1ac1]); // v_mbcnt_hi_u32_b32 v13, -1, v13
    }
    code.push(0x7d24_0000 | 13 << 9 | 22); // v_cmpx_eq_i32 s22, v13
    let none = code.len();
    code.extend([
        0, // s_cbranch_execz tail
        vmov(14, 20),
        vmov(15, 27),
        0xbf8c_c07f,              // s_waitcnt
        0xe0c8_2000, 0x8002_0e0f, // buffer_atomic_add v14, v15, s[8:11], idxen
        0xbf80_0000,
    ]);
    let tail = code.len();
    code.extend([
        smov(wide, 126, 18),
        0xd501_0006, 16 << 18 | 0x81 << 9 | 0x80, // v_cndmask_b32 v6, 0, 1, s16
        0x3a0c_0c81,                              // v_xor_b32 v6, 1, v6
    ]);
    let exit = code.len();
    code.extend([
        smov(wide, 126, 14),
        0x7d8a_0c80,
        0xbe9a_0f6a + (w << 8), // s_bcnt1_i32 s26, vcc
        0,
    ]);
    let again = code.len() - 1;
    branch(&mut code, skip, 8, exit);
    branch(&mut code, empty, 8, tail);
    branch(&mut code, none, 8, tail - 1);
    branch(&mut code, again, 5, head);
    code.extend_from_slice(after);
    if masked {
        code.push(smov(wide, 126, 32));
    }
    code.push(ENDPGM);
    code
}

/// `threads` in one row, with a typed UAV at u0 in the table s[4:5] points to.
pub fn shader(threads: u32, words: &[u32]) -> ShaderExtract {
    let isa: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let uav = RootRange { range_type: 1, num: 1, base_register: 0, space: 0, offset_in_table: 0 };
    ShaderExtract {
        flat_inputs: Vec::new(), flat_outputs: Vec::new(),
        stage: Stage::Compute, shader_model: (6, 0), required_features: None,
        bitcode_stripped: true, fspx_config: None, ps09_pixel_inputs: None,
        fssm: Vec::new(), entry: Some("main".into()), isa_data: isa.clone(),
        isa, isa_offset_in_fssm: 0, psv_threads: Some([threads, 1, 1]), isa_preamble: [0, threads, 1, 1],
        has_input_sig: false, has_output_sig: false, has_psv0: false, has_root_sig: true,
        input_sig: Vec::new(), output_sig: Vec::new(),
        resources: vec![PsvResource { res_type: 6, space: 0, lower: 0, upper: 0 }],
        gs_info: None, mesh_info: None, primitive_sig: Vec::new(),
        root_sig: Some(RootSig { version: 1, flags: 0, num_static_samplers: 0, static_samplers: Vec::new(),
            params: vec![RootParam { visibility: 0, kind: RootParamKind::Table(vec![uav]) }] }),
    }
}
