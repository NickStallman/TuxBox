//! DS_SWIZZLE_B32 in an ordinary compute shader, exchanged through group shared memory.
//!
//! RDNA2 ISA: offset[15] selects QDMode, where lane i of each quad reads lane
//! offset[2*i+1 : 2*i] of the same quad. Otherwise offset[14:0] holds the
//! and/or/xor bit masks applied within each group of 32 lanes.
//! `tools/check_compute_swizzle.py` runs the same fixture on a GPU.
use super::translate;
use crate::rdna2::disassemble;
use crate::{ShaderExtract, Stage};

mod fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/support/compute_swizzle_fixture.rs"));
}

fn swizzle(offset: u32, word0_extra: u32) -> Result<String, String> {
    let ex = fixture::shader(offset, word0_extra);
    assert_eq!(disassemble(&ex.isa)[0].mnemonic, "ds_swizzle_b32");
    translate(&ex).map(|t| t.hlsl)
}

#[test]
fn quad_permute_mode_reads_the_selected_lane_of_each_quad() {
    // swizzle(QUAD_PERM, 3, 2, 1, 0): 0b00_01_10_11 reverses every quad.
    let hlsl = swizzle(0x801b, 0).unwrap();
    assert!(hlsl.contains("tb_swizzle[tb_lane] = ((exec_lo & 1u) != 0u) ? v1 : 0u;"), "{hlsl}");
    assert!(hlsl.contains("tb_swizzle[(tb_lane & ~3u) | ((0x1bu >> (2u * (tb_lane & 3u))) & 3u)]"), "{hlsl}");
    assert!(!hlsl.contains("& ~31u"), "{hlsl}");
}

#[test]
fn bit_mask_mode_is_unchanged() {
    // swizzle(BITMASK_PERM, "01pip"): and 0x1f, or 0, xor 1.
    let hlsl = swizzle(0x041f, 0).unwrap();
    assert!(hlsl.contains("tb_swizzle[(tb_lane & ~31u) | (((tb_lane & 31u) | 0u) ^ 1u)]"), "{hlsl}");
}

#[test]
fn other_high_offset_bits_and_gds_stay_refused() {
    for (offset, extra) in [(0x8100, 0), (0xe000, 0), (0xffff, 0), (0x801b, 1 << 16), (0x041f, 1 << 17)] {
        let error = swizzle(offset, extra).unwrap_err();
        let reason = if extra == 1 << 17 { "GDS is unsupported" } else { "unsupported compute DS swizzle encoding" };
        assert!(error.contains(reason), "{offset:#x}/{extra:#x}: {error}");
    }
}

#[test]
fn uniform_scc_loop_around_the_exchange_translates() {
    // A 32-thread, single-wave group: an SCC back-edge (uniform, never per-lane)
    // means every invocation runs the loop and the shared exchange the same
    // number of times, so the loop is accepted.
    let ex = fixture::shader_loop(0x041f, 0, 5);
    assert_eq!(disassemble(&ex.isa)[0].mnemonic, "ds_swizzle_b32");
    let hlsl = translate(&ex).map(|t| t.hlsl)
        .expect("uniform SCC loop over a compute swizzle translates");
    assert!(hlsl.contains("tb_swizzle"), "{hlsl}");
    assert!(hlsl.contains("GroupMemoryBarrierWithGroupSync()"), "{hlsl}");
}

#[test]
fn unconditional_loop_around_the_exchange_translates() {
    let ex = fixture::shader_loop(0x041f, 0, 2);
    let hlsl = translate(&ex).map(|t| t.hlsl)
        .expect("unconditional loop over a compute swizzle translates");
    assert!(hlsl.contains("tb_swizzle"), "{hlsl}");
}

#[test]
fn divergent_vcc_loop_around_the_exchange_is_refused() {
    // A VCC back-edge can run lane-selectively: lanes in the loop body run the
    // exchange a different number of times, which is unsafe.
    let error = translate(&fixture::shader_loop(0x041f, 0, 7))
        .map(|t| t.hlsl)
        .unwrap_err();
    assert!(error.contains("compute swizzle inside a loop"), "{error}");
}

#[test]
fn per_thread_execz_back_edge_is_refused() {
    // This emitter keeps EXEC as a per-thread bit, so an execz back-edge is
    // per-invocation: threads can leave the loop on different iterations.
    let error = translate(&fixture::shader_loop(0x041f, 0, 8))
        .map(|t| t.hlsl)
        .unwrap_err();
    assert!(error.contains("compute swizzle inside a loop"), "{error}");
}

#[test]
fn multi_wave_group_is_refused() {
    // A 64-thread group runs two 32-lane waves with independent SCC registers,
    // so a uniform-SCC back-edge is no longer group-uniform.
    let error = translate(&fixture::shader_loop_64(0x041f, 0))
        .map(|t| t.hlsl)
        .unwrap_err();
    assert!(error.contains("compute swizzle inside a loop"), "{error}");
}

#[test]
fn forward_exit_inside_the_loop_is_refused() {
    // A per-thread forward exit out of the loop body leaves the barrier count
    // mismatched: threads that leave stop reaching the exchange while others
    // keep iterating.
    let error = translate(&fixture::shader_loop_exit_guard(0x041f))
        .map(|t| t.hlsl)
        .unwrap_err();
    assert!(error.contains("compute swizzle inside a loop"), "{error}");
}

#[test]
fn countdown_loop_with_early_exit_translates() {
    // A countdown loop around the exchange with a per-thread early exit before
    // the loop: the emitted loop keeps every lane together (a whole-wave vote),
    // masks the publish on the exit flag and keeps the finished lanes' exchange
    // contributions at zero. This is the GPU control driver.
    let ex = fixture::shader_loop_exit_countdown(0x041f);
    let hlsl = translate(&ex).map(|t| t.hlsl)
        .expect("countdown loop with per-thread exit translates");
    assert!(hlsl.contains("tb_swizzle"), "{hlsl}");
    assert!(hlsl.contains("WaveActiveAnyTrue(tb_c"), "{hlsl}");
    assert!(hlsl.contains("(tb_skip < 7u)"), "{hlsl}"); // masked publish
    // One variable holds every pending forward branch.
    assert_eq!(hlsl.matches("uint tb_skip = 0u;").count(), 1, "{hlsl}");
    assert!(!hlsl.contains("bool j"), "{hlsl}");
    assert!(hlsl.contains("GroupMemoryBarrierWithGroupSync()"), "{hlsl}");
}

#[test]
fn paired_wave64_lowers_quad_and_bitmask_swizzles_without_shared_memory() {
    use crate::hlsl::translate_paired_wave64;

    let quad = translate_paired_wave64(&fixture::shader(0x801b, 0)).unwrap().hlsl;
    assert!(quad.contains("(tb_pair + 0u) & ~3u"), "{quad}");
    assert!(quad.contains("(tb_pair + 32u) & ~3u"), "{quad}");
    assert!(!quad.contains("tb_swizzle"), "{quad}");
    assert!(!quad.contains("GroupMemoryBarrierWithGroupSync"), "{quad}");

    let bitmask = translate_paired_wave64(&fixture::shader(0x041f, 0)).unwrap().hlsl;
    assert!(bitmask.contains("(((tb_pair + 0u) & 31u) | 0u) ^ 1u"), "{bitmask}");
    assert!(bitmask.contains("(((tb_pair + 32u) & 31u) | 0u) ^ 1u"), "{bitmask}");

    for (offset, extra) in [(0x8100, 0), (0xe000, 0), (0x801b, 1 << 16), (0x041f, 1 << 17)] {
        let error = translate_paired_wave64(&fixture::shader(offset, extra)).map(|t| t.hlsl).unwrap_err();
        assert!(error.contains("unsupported compute DS swizzle encoding"), "{offset:#x}/{extra:#x}: {error}");
    }
}

#[test]
fn loop_in_whole_console_waves_runs_two_lanes_per_thread() {
    // A 64-lane console wave branches once for all its lanes. The two-lane form
    // keeps that, so the exchange needs no barrier and the loop no vote.
    for threads in [64, 128] {
        let translated = translate(&fixture::shader_loop_wave64(0x801b, threads)).unwrap();
        assert_eq!(translated.required_wave_size, Some(32));
        let hlsl = &translated.hlsl;
        assert!(hlsl.contains(&format!("[WaveSize(32)]\n[numthreads({}, 1, 1)]", threads / 2)), "{hlsl}");
        assert!(hlsl.contains("(exec_lo | exec_hi) != 0u"), "{hlsl}");
        assert!(!hlsl.contains("GroupMemoryBarrierWithGroupSync"), "{hlsl}");
    }
    // Part of a console wave, or no sign of its width, stays refused.
    for shader in [fixture::shader_loop_wave64(0x801b, 32), fixture::shader_loop_wave64(0x801b, 96), fixture::shader_loop_64(0x801b, 0)] {
        assert_eq!(translate(&shader).map(|t| t.hlsl).unwrap_err(), "compute swizzle inside a loop");
    }
}
