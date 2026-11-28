//! Emit synthetic RDNA2 constant and indexed `v_readlane_b32` compute shaders through
//! the real HLSL translator for DXC/DXV validation and D3D12 GPU readback.
use tuxbox_xshader::{hlsl, ShaderExtract, Stage};

fn shader(group: [u32; 3], words: &[u32]) -> ShaderExtract {
    let isa: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    ShaderExtract {
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
        psv_threads: Some(group),
        flat_inputs: Vec::new(),
        flat_outputs: Vec::new(),
        isa_preamble: [0, group[0], group[1], group[2]],
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

const fn readlane(dst: u8, vsrc: u8, lane: u32) -> [u32; 2] {
    [
        0xd760_0000 | dst as u32,
        ((128 + lane) << 9) | 256 | vsrc as u32,
    ]
}

const fn readlane_sgpr(dst: u8, vsrc: u8, s_lane: u8) -> [u32; 2] {
    [
        0xd760_0000 | dst as u32,
        ((s_lane as u32) << 9) | 256 | vsrc as u32,
    ]
}

const fn writelane(vdst: u8, ssrc: u8, lane: u32) -> [u32; 2] {
    [
        0xd761_0000 | vdst as u32,
        ((128 + lane) << 9) | ssrc as u32,
    ]
}

fn instrument(hlsl: &str, lanes: u32, end_marker: &str, result_reg: &str) -> String {
    let init_src = "    v0 = gtid.x; v1 = gtid.y; v2 = gtid.z;";
    // Load per-lane v1 value from output[gtid.x] so the host verifies exact
    // arbitrary bit patterns (not predictable thread IDs alone).
    let init_dst = "    v0 = gtid.x; v1 = output[gtid.x]; v2 = gtid.z;";
    assert_eq!(hlsl.matches(init_src).count(), 1, "missing init in:\n{hlsl}");
    assert_eq!(hlsl.matches(end_marker).count(), 1, "missing {end_marker} in:\n{hlsl}");
    let store = if lanes <= 64 {
        format!("    output[{lanes}u + gtid.x] = {result_reg};\n{end_marker}")
    } else {
        format!("    GroupMemoryBarrierWithGroupSync();\n    output[gtid.x] = {result_reg};\n{end_marker}")
    };
    let body = hlsl
        .replace(init_src, init_dst)
        .replace(end_marker, &store);
    format!("RWBuffer<uint> output : register(u0);\n{body}")
}

fn main() {
    let case = std::env::args().nth(1).expect("case name");
    let mixed_lane = matches!(case.as_str(), "wave32_mixed_dynamic_11" | "wave32_mixed_empty_exec" | "wave32_mixed_lds_dpp_11");
    let (group, words, result_reg, expect_wave32): ([u32; 3], Vec<u32>, &str, bool) = match case.as_str() {
        "wave32_active_0" => {
            let [r0, r1] = readlane(7, 1, 0);
            ([32, 1, 1], vec![r0, r1, 0xbf81_0000], "s7", true)
        }
        "wave32_active_15" => {
            let [r0, r1] = readlane(7, 1, 15);
            ([32, 1, 1], vec![r0, r1, 0xbf81_0000], "s7", true)
        }
        "wave32_active_31" => {
            let [r0, r1] = readlane(7, 1, 31);
            ([32, 1, 1], vec![r0, r1, 0xbf81_0000], "s7", true)
        }
        "wave32_masked_31" => {
            // Mask lane 31 OFF in EXEC, overwrite active lanes' v1 with sentinel
            // 0xdead0000, skip lane 31 via s_cbranch_execz, then read lane 31 of v1.
            // Restore EXEC afterwards so ALL 32 lanes store s7 (or store s7 directly).
            let [r0, r1] = readlane(7, 1, 31);
            (
                [32, 1, 1],
                vec![
                    0x7e04_029f,             // v_mov_b32 v2, 31
                    0x7d82_0500,             // v_cmp_lt_u32 vcc, v0, v2
                    0xbe84_3c6a,             // s_and_saveexec_b32 s4, vcc_lo (lane 31 EXEC = 0)
                    0x7e02_02ff, 0xdead0000, // v_mov_b32 v1, 0xdead0000 (lane 31 keeps input!)
                    r0, r1,                  // v_readlane_b32 s7, v1, 31 (MUST read pre-mask v1!)
                    0xbefe_0304,             // s_mov_b32 exec_lo, s4 (restore EXEC)
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave32_masked_branch_31" => {
            // Same as above, PLUS s_cbranch_execz skips lane 31 around the readlane!
            // Active lanes 0..30 MUST still receive lane 31's pre-mask v1 in s7;
            // lane 31 took the skip branch so its s7 stays 0.
            let [r0, r1] = readlane(7, 1, 31);
            (
                [32, 1, 1],
                vec![
                    0xbe87_0380,             // s_mov_b32 s7, 0
                    0x7e04_029f,             // v_mov_b32 v2, 31
                    0x7d82_0500,             // v_cmp_lt_u32 vcc, v0, v2
                    0xbe84_3c6a,             // s_and_saveexec_b32 s4, vcc_lo
                    0x7e02_02ff, 0xdead0000, // v_mov_b32 v1, 0xdead0000
                    0xbf88_0002,             // s_cbranch_execz +2 (skip r0, r1)
                    r0, r1,                  // v_readlane_b32 s7, v1, 31
                    0xbefe_0304,             // s_mov_b32 exec_lo, s4
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave32_64t_masked_31" => {
            // 64 threads, Wave32 (two independent 32-lane waves):
            // Wave 0 (0..31) reads thread 31's pre-mask v1; Wave 1 (32..63) reads thread 63's!
            let [r0, r1] = readlane(7, 1, 31);
            (
                [64, 1, 1],
                vec![
                    0x3604_009f,             // v_and_b32 v2, 31, v0 (lane in wave32)
                    0x7e06_029f,             // v_mov_b32 v3, 31
                    0x7d82_0702,             // v_cmp_lt_u32 vcc, v2, v3 (lane 31 of EACH wave -> 0)
                    0xbe84_3c6a,             // s_and_saveexec_b32 s4, vcc_lo
                    0x7e02_02ff, 0xbad00000, // v_mov_b32 v1, 0xbad00000
                    r0, r1,                  // v_readlane_b32 s7, v1, 31
                    0xbefe_0304,             // s_mov_b32 exec_lo, s4
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave64_64t_lane31" => {
            let [r0, r1] = readlane(7, 1, 31);
            ([64, 1, 1], vec![r0, r1, 0xbf81_0000], "s7", false)
        }
        "wave64_64t_lane47" => {
            let [r0, r1] = readlane(7, 1, 47);
            ([64, 1, 1], vec![r0, r1, 0xbf81_0000], "s7", false)
        }
        "wave64_64t_masked_lane63" => {
            let [r0, r1] = readlane(7, 1, 63);
            (
                [64, 1, 1],
                vec![
                    0x7e04_02bf,             // v_mov_b32 v2, 63
                    0x7d82_0500,             // v_cmp_lt_u32 vcc, v0, v2
                    0xbe84_246a,             // s_and_saveexec_b64 s[4:5], vcc (lane 63 EXEC = 0)
                    0x7e02_02ff, 0xdeadbeef, // v_mov_b32 v1, 0xdeadbeef (lane 63 keeps input!)
                    r0, r1,                  // v_readlane_b32 s7, v1, 63
                    0xbefe_0404,             // s_mov_b64 exec, s[4:5]
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave64_128t_lane31" => {
            let [r0, r1] = readlane(7, 1, 31);
            ([128, 1, 1], vec![r0, r1, 0xbf81_0000], "s7", false)
        }
        "wave64_128t_lane63" => {
            let [r0, r1] = readlane(7, 1, 63);
            ([128, 1, 1], vec![r0, r1, 0xbf81_0000], "s7", false)
        }
        "wave64_64t_spill_and_loop_lane31" => {
            // SGPR spill on v72 + uniform loop reading lane 31 of v1, adding spilled value.
            let [wl0, wl1] = writelane(72, 8, 0);
            let [rl_s0, rl_s1] = readlane(9, 72, 0);
            let [rl_l0, rl_l1] = readlane(7, 1, 31);
            (
                [64, 1, 1],
                vec![
                    0xbe88_038a,          // s_mov_b32 s8, 10
                    wl0, wl1,             // v_writelane_b32 v72, s8, 0
                    0xbe8a_0383,          // s_mov_b32 s10, 3
                    rl_l0, rl_l1,         // L_loop: v_readlane_b32 s7, v1, 31
                    0x818a_810a,          // s_sub_i32 s10, s10, 1
                    0xbf07_800a,          // s_cmp_lg_u32 s10, 0
                    0xbf85_fffb,          // s_cbranch_scc1 L_loop
                    rl_s0, rl_s1,         // v_readlane_b32 s9, v72, 0
                    0x8007_0907,          // s_add_u32 s7, s7, s9 (lane31(v1) + 10)
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave32_ctl_dep_guard_31" => {
            // F2 live-GPU repro: lane 31 skips overwrite+readlane via control-dependent SCC.
            // Lanes 0..30 MUST read lane 31's original input, NOT 0xdead0000.
            let [r0, r1] = readlane(7, 1, 31);
            (
                [32, 1, 1],
                vec![
                    0xbe87_0380,             // s_mov_b32 s7, 0
                    0xbe8a_0380,             // s_mov_b32 s10, 0
                    0x7d84_009f,             // v_cmp_eq_u32 vcc, 31, v0
                    0xbf87_0001,             // s_cbranch_vccnz +1 (lane 31 keeps s10=0)
                    0xbe8a_0381,             // s_mov_b32 s10, 1 (lanes 0..30 get s10=1)
                    0xbf06_800a,             // s_cmp_eq_u32 s10, 0
                    0xbf85_0004,             // s_cbranch_scc1 +4 (lane 31 skips)
                    0x7e02_02ff, 0xdead0000, // v_mov_b32 v1, 0xdead0000
                    r0, r1,                  // v_readlane_b32 s7, v1, 31
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave32_64t_and_exec_31" => {
            // Wave32 [64, 1, 1] proved by VOP3 odd-SGPR v_cmp_lt_u32 s5, using s_and_b32/s_or_b32 on exec_lo.
            // Wave 0 (0..31) reads thread 31; Wave 1 (32..63) reads thread 63.
            let [r0, r1] = readlane(7, 1, 31);
            (
                [64, 1, 1],
                vec![
                    0x3604_009f,             // v_and_b32 v2, 31, v0
                    0x7e06_029f,             // v_mov_b32 v3, 31
                    0xd4c1_0005, 0x0002_0702, // v_cmp_lt_u32 s5, v2, v3 (odd SGPR -> Wave32)
                    0x877e_057e,             // s_and_b32 exec_lo, exec_lo, s5
                    0x7e02_02ff, 0xbad00000, // v_mov_b32 v1, 0xbad00000
                    r0, r1,                  // v_readlane_b32 s7, v1, 31
                    0x887e_c17e,             // s_or_b32 exec_lo, exec_lo, -1
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave32_mbcnt_exec_31" => {
            // F5 live-GPU repro: [32, 1, 1] Wave32 combining s_and_saveexec_b32,
            // v_mbcnt_lo_u32_b32 v2, exec_lo, 0, and v_readlane_b32 s7, v1, 31.
            // Stores `(v2 << 24) | (s7 & 0x00ffffff)` to verify both prefix count and readlane!
            let [r0, r1] = readlane(7, 1, 31);
            (
                [32, 1, 1],
                vec![
                    0x7e04_029f,              // v_mov_b32 v2, 31
                    0x7d82_0500,              // v_cmp_lt_u32 vcc, v0, v2 (lanes 0..30 active)
                    0xbe84_3c6a,              // s_and_saveexec_b32 s4, vcc_lo
                    0xd765_0002, 0x0001_007e, // v_mbcnt_lo_u32_b32 v2, exec_lo, 0 (0..30)
                    r0, r1,                   // v_readlane_b32 s7, v1, 31
                    0xbefe_0304,              // s_mov_b32 exec_lo, s4 (restore all 32 lanes)
                    0x3404_0498,              // v_lshlrev_b32 v2, 24, v2
                    0x7e06_0207,              // v_mov_b32 v3, s7
                    0x3606_06ff, 0x00ffffff,  // v_and_b32 v3, 0x00ffffff, v3
                    0x3804_0702,              // v_or_b32 v2, v2, v3
                    0xbf81_0000,
                ],
                "v2",
                true,
            )
        }
        "wave64_swizzle_branch_dyn_63" => {
            // Wave64 [64, 1, 1] ds_swizzle_b32 + dynamic SGPR v_readlane_b32 inside a forward
            // s_cbranch_execz skip branch that lane 63 is not active for.
            // Every lane MUST receive lane 63's original pre-mask v1 (not 0) in s7.
            let [r0, r1] = readlane_sgpr(7, 1, 5);
            (
                [64, 1, 1],
                vec![
                    0xbe87_0380,              // s_mov_b32 s7, 0
                    0x7e08_02ff, 0x0000_003f, // v_mov_b32 v4, 63
                    0x7e0a_0504,              // v_readfirstlane_b32 s5, v4 (unproven dynamic SGPR = 63)
                    0x7e04_02bf,              // v_mov_b32 v2, 63
                    0x7d82_0500,              // v_cmp_lt_u32 vcc, v0, v2 (lanes 0..62 active)
                    0xbe88_246a,              // s_and_saveexec_b64 s[8:9], vcc
                    0x7e02_02ff, 0xdead_0000, // v_mov_b32 v1, 0xdead0000 (lane 63 keeps input!)
                    0xd8d4_041f, 0x0300_0001, // ds_swizzle_b32 v3, v1 offset:0x041f
                    0xbf88_0002,              // s_cbranch_execz +2 (lane 63 skips readlane)
                    r0, r1,                   // v_readlane_b32 s7, v1, s5 (reads skipped lane 63!)
                    0xbefe_0408,              // s_mov_b64 exec, s[8:9]
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave64_dyn_sgpr_high_bits_47" => {
            // Standalone (no ds_swizzle) Wave64 [64, 1, 1] unproven dynamic SGPR v_readlane_b32
            // where s5 = 0x102f (high bits above 63 set; RDNA2 masks s5 & 63 = 47).
            let [r0, r1] = readlane_sgpr(7, 1, 5);
            (
                [64, 1, 1],
                vec![
                    0x7e08_02ff, 0x0000_102f, // v_mov_b32 v4, 0x102f
                    0x7e0a_0504,              // v_readfirstlane_b32 s5, v4 (unproven dynamic SGPR)
                    r0, r1,                   // v_readlane_b32 s7, v1, s5 (must mask & 63 -> lane 47)
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave32_64t_dyn_sgpr_31" => {
            // Standalone Wave32 [64, 1, 1] (two 32-lane waves) unproven dynamic SGPR v_readlane_b32
            // where s5 = 0x201f (RDNA2 Wave32 masks s5 & 31 = 31: wave 0 -> thread 31, wave 1 -> thread 63).
            let [r0, r1] = readlane_sgpr(7, 1, 5);
            (
                [64, 1, 1],
                vec![
                    0x7e08_02ff, 0x0000_201f, // v_mov_b32 v4, 0x201f
                    0x7e0a_0504,              // v_readfirstlane_b32 s5, v4
                    0x3604_009f,              // v_and_b32 v2, 31, v0
                    0x7e06_029f,              // v_mov_b32 v3, 31
                    0x7d82_0702,              // v_cmp_lt_u32 vcc, v2, v3
                    0xbe84_3c6a,              // s_and_saveexec_b32 s4, vcc_lo (Wave32)
                    0x7e02_02ff, 0xbad0_0000, // v_mov_b32 v1, 0xbad00000
                    r0, r1,                   // v_readlane_b32 s7, v1, s5
                    0xbefe_0304,              // s_mov_b32 exec_lo, s4
                    0xbf81_0000,
                ],
                "s7",
                false,
            )
        }
        "wave32_mixed_dynamic_11" | "wave32_mixed_empty_exec" | "wave32_mixed_lds_dpp_11" => {
            // Assemble for gfx1030 wave32. Flatten the 2D group, seed v9 with
            // distinct values, and let the first active lane choose (12 + 63)
            // & 31 = 11. The read and write both see lane 11 with EXEC clear.
            // The empty-mask case must skip the lane write and keep s7.
            let threshold = if case == "wave32_mixed_empty_exec" { 32 } else { 12 };
            // The LDS and DPP case then gives every lane its neighbour's v6 through
            // group memory and a quad permute, and long-jumps over a move that would
            // clear the result: v15 = v6 ^ (lds << 1) ^ (dpp << 2).
            let extra: &[u32] = if case == "wave32_mixed_lds_dpp_11" { &[
                0x3414_0882,              // v_lshlrev_b32 v10, 2, v4
                0xd834_0000, 0x0000_060a, // ds_write_b32 v10, v6
                0x3a16_1484,              // v_xor_b32 v11, 4, v10
                0xbf8c_c07f,              // s_waitcnt lgkmcnt(0)
                0xd8d8_0000, 0x0c00_000b, // ds_read_b32 v12, v11
                0x7e1a_02fa, 0xff0c_b106, // v_mov_b32_dpp v13, v6 quad_perm:[1,0,3,2] bound_ctrl:1 fi:1
                0xbf8c_c07f,              // s_waitcnt lgkmcnt(0)
                0x3418_1881,              // v_lshlrev_b32 v12, 1, v12
                0x341a_1a82,              // v_lshlrev_b32 v13, 2, v13
                0x3a1e_1b0c,              // v_xor_b32 v15, v12, v13
                0xbe94_1f00,              // s_getpc_b64 s[20:21]
                0x8014_9014,              // s_add_u32 s20, s20, 16
                0x8215_8015,              // s_addc_u32 s21, s21, 0
                0xbe80_2014,              // s_setpc_b64 s[20:21]
                0x7e1e_0280,              // v_mov_b32 v15, 0 (jumped over)
                0x3a1e_0d0f,              // v_xor_b32 v15, v15, v6
            ] } else { &[] };
            (
                [8, 4, 1],
                [vec![
                    0x3408_0283,             // v_lshlrev_b32 v4, 3, v1
                    0x3808_0900,             // v_or_b32 v4, v0, v4
                    0x4a12_0891,             // v_add_nc_u32 v9, 17, v4
                    0x4a0a_08bf,             // v_add_nc_u32 v5, 63, v4
                    0xbe87_0383,             // s_mov_b32 s7, 3
                    0xd4c6_000b, 0x0201_0104 | ((128 + threshold) << 9), // v_cmp_ge_u32 s11, v4, threshold
                    0xbe84_3c0b,             // s_and_saveexec_b32 s4, s11
                    0xbf88_0007,             // s_cbranch_execz done
                    0xbe88_03ff, 0x5a5a_31f2, // s_mov_b32 s8, 0x5a5a31f2
                    0xd761_0009, 0x0201_1608, // v_writelane_b32 v9, s8, 11
                    0x7e0e_0505,             // v_readfirstlane_b32 s7, v5
                    0xd760_0007, 0x0200_0f09, // v_readlane_b32 s7, v9, s7
                    0xbefe_0304,             // done: s_mov_b32 exec_lo, s4
                    0x3a0c_1207,             // v_xor_b32 v6, s7, v9
                ], extra.to_vec(), vec![0xbf81_0000]].concat(), // s_endpgm
                if extra.is_empty() { "v6" } else { "v15" },
                true,
            )
        }
        other => panic!("unknown readlane probe case: {other}"),
    };
    let end_off = (words.len() - 1) * 4;
    let end_marker = format!("    // {end_off:#06x} s_endpgm");
    let tr = hlsl::translate(&shader(group, &words)).unwrap_or_else(|e| panic!("{case}: {e}"));
    let check_wave = std::env::args().nth(2).as_deref() != Some("--no-assert-wave");
    if check_wave {
        if expect_wave32 {
            assert_eq!(tr.required_wave_size, Some(32), "{case} must use WaveSize(32)");
        } else if case == "wave64_swizzle_branch_dyn_63" {
            // Its EXECZ branch is decided by the whole wave, which needs the guest's width.
            assert_eq!(tr.required_wave_size, Some(64), "{case} must use WaveSize(64)");
        } else {
            assert_eq!(tr.required_wave_size, None, "{case} must be portable (no WaveSize)");
        }
    }
    if mixed_lane {
        // Use the same logical lane mapping for the readback slot, independent
        // of how D3D12 assigns SV_GroupThreadID to the hardware lanes.
        assert_eq!(tr.hlsl.matches(&end_marker).count(), 1);
        let store = format!("    output[32u + WaveGetLaneIndex()] = {result_reg};\n{end_marker}");
        print!("RWBuffer<uint> output : register(u0);\n{}", tr.hlsl.replace(&end_marker, &store));
    } else {
        print!("{}", instrument(&tr.hlsl, group[0], &end_marker, result_reg));
    }
}
