// Synthetic compute DS_SWIZZLE_B32, shared with the translator regression.
use super::{ShaderExtract, Stage};

/// ds_swizzle_b32 v0, v1 offset:<offset>; s_waitcnt lgkmcnt(0); s_endpgm
/// in one 64-lane group. As in LLVM's encoding, the source occupies the ADDR field.
pub fn shader(offset: u32, word0_extra: u32) -> ShaderExtract {
    let words = [0xd8d4_0000 | offset | word0_extra, 0x0000_0001, 0xbf8c_c07f, 0xbf81_0000];
    build(words.to_vec(), [0, 64, 1, 1])
}

/// The same exchange, wrapped in a backward branch that keeps it in a loop.
/// Operand is the SOP1 back-edge: op 5 is a uniform SCC skip (s_cbranch_scc1),
/// op 7 a per-lane VCC one (s_cbranch_vccnz). Both jump back to 0x00.
/// In a single 32-lane wave, the group is one host wave, which is what makes a
/// uniform back-edge safe.
pub fn shader_loop(offset: u32, word0_extra: u32, op: u32) -> ShaderExtract {
    let words = [
        0xd8d4_0000 | offset | word0_extra,
        0x0000_0001,
        0xbf80_0000 | (op << 16) | 0xfffd, // s_cbranch_scc1/vccnz -> 0x00 (back-edge)
        0xbf8c_c07f,                        // s_waitcnt
        0xbf81_0000,                        // s_endpgm
    ];
    build(words.to_vec(), [0, 32, 1, 1])
}

/// The same exchange in a 64-thread group (two host waves), wrapped in the same
/// SCC back-edge: unsafe even though every thread in the group shares the SCC.
pub fn shader_loop_64(offset: u32, word0_extra: u32) -> ShaderExtract {
    let words = [
        0xd8d4_0000 | offset | word0_extra,
        0x0000_0001,
        0xbf85_fffd, // s_cbranch_scc1 -> 0x00 (back-edge)
        0xbf8c_c07f,
        0xbf81_0000,
    ];
    build(words.to_vec(), [0, 64, 1, 1])
}

/// The exchange in a loop that retires lanes from a two-word EXEC, as only a
/// 64-lane console wave keeps it. `threads` is the group size.
pub fn shader_loop_wave64(offset: u32, threads: u32) -> ShaderExtract {
    let words = [
        0xbe8a_047e,           // s_mov_b64 s[10:11], exec
        0xd8d4_0000 | offset,
        0x0000_0001,
        0xbe8a_3a0a,           // s_andn2_wrexec_b64 s[10:11], s[10:11]
        0xbf89_fffc,           // s_cbranch_execnz -> 0x04 (back-edge)
        0xbf8c_c07f,           // s_waitcnt
        0xbf81_0000,           // s_endpgm
    ];
    build(words.to_vec(), [0, threads, 1, 1])
}

/// A loop whose head is entered through a per-thread EXEC-empty exit: the guest
/// may leave the loop from one thread while others stay. One host wave, one
/// exchange, one unconditional s_branch back-edge.
pub fn shader_loop_exit_guard(offset: u32) -> ShaderExtract {
    let words = [
        0xbf88_0003,           // s_cbranch_execz -> 0x10 (leaves the loop)
        0xd8d4_0000 | offset,
        0x0000_0001,
        0xbf82_fffc,           // s_branch -> 0x00 (back to the start)
        0xbf8c_c07f,           // s_waitcnt
        0xbf81_0000,           // s_endpgm
    ];
    build(words.to_vec(), [0, 32, 1, 1])
}

fn build(words: Vec<u32>, preamble: [u32; 4]) -> ShaderExtract {
    let isa: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    ShaderExtract { flat_inputs: Vec::new(), flat_outputs: Vec::new(),
        stage: Stage::Compute,
        shader_model: (6, 0),
        required_features: None,
        bitcode_stripped: true,
        fspx_config: None,
        ps09_pixel_inputs: None,
        fssm: Vec::new(),
        entry: Some("main".into()),
        isa_data: isa.clone(),
        isa,
        isa_offset_in_fssm: 0,
        psv_threads: None,
        isa_preamble: preamble,
        has_input_sig: false,
        has_output_sig: false,
        has_psv0: false,
        has_root_sig: false,
        input_sig: Vec::new(),
        output_sig: Vec::new(),
        resources: Vec::new(),
        gs_info: None,
        mesh_info: None,
        primitive_sig: Vec::new(),
        root_sig: None,
    }
}

/// A countdown loop around the exchange with a per-thread early exit.
///
/// The loop head is the ds_swizzle (two dwords); after it, the count is
/// decremented and the SCC taken to the back-edge, so the loop runs exactly
/// four iterations. The execz exit branch points past the loop to s_endpgm, so
/// a lane that is not active leaves the loop on its first iteration while the
/// remaining lanes run to completion. 32-lane group (single host wave).
pub fn shader_loop_exit_countdown(offset: u32) -> ShaderExtract {
    let words = [
        0xbe8a_0384, // s_mov_b32 s10, 4 (loop counter)
        0xbf88_0006, // s_cbranch_execz -> 0x0020 (per-thread exit past the loop)
        0xd8d4_0000 | offset, // ds_swizzle_b32 (2 dwords)
        0x0000_0001,
        0x818a_810a, // s_sub_i32 s10, s10, 1
        0xbf07_800a, // s_cmp_lg_u32 s10, 0 (scc = s10 != 0)
        0xbf85_fffb, // s_cbranch_scc1 -> 0x0008 (back-edge, -5 dwords)
        0xbf8c_c07f, // s_waitcnt
        0xbf81_0000, // s_endpgm
    ];
    build(words.to_vec(), [0, 32, 1, 1])
}
