use super::*;

mod fixture {
    include!("../../../examples/support/wave_guard_fixture.rs");
}

/// Build native masked reductions with optional pre-guard edges and an
/// independent tail EXECZ; width selects the verified 32/64-lane fixture.
fn guarded_shader(width: u32, prefix: &[u32]) -> ShaderExtract {
    let (mut shader, mut words, end) = fixture::shader(width);
    words.truncate(end);
    let skip = words.len() as u32;
    words.splice(..0, prefix.iter().copied().chain([0xbe87_0391, 0xbf88_0000 | skip]));
    words.extend([0xbf88_0001, 0xbf80_0000, 0xbf81_0000]);
    shader.isa = words.into_iter().flat_map(u32::to_le_bytes).collect();
    shader.isa_data = shader.isa.clone();
    shader
}

#[test]
fn every_execz_around_a_reduction_votes() {
    for width in [32, 64] {
        // The guard and the independent tail branch are both the whole wave's.
        let translated = translate(&guarded_shader(width, &[])).expect("guarded reduction");
        assert_eq!(translated.required_wave_size, Some(width));
        assert_eq!(translated.hlsl.matches("!WaveActiveAnyTrue((exec_lo & 1u) != 0u)").count(), 2);
        assert!(!translated.hlsl.contains("uint((exec_lo & 1u) == 0u) * "));
    }
}

#[test]
fn reduction_execz_accepts_direct_rejoin_at_vote() {
    for width in [32, 64] {
        // Skip one local vector instruction and rejoin exactly at the guard.
        let translated = translate(&guarded_shader(width, &[0xbf88_0002, 0x4a00_0081]))
            .expect("direct rejoin before wave vote");
        assert_eq!(translated.hlsl.matches("!WaveActiveAnyTrue((exec_lo & 1u) != 0u)").count(), 3);
        assert!(!translated.hlsl.contains("uint((exec_lo & 1u) == 0u) * "));
    }
}

#[test]
fn reduction_execz_behind_a_bypass_branches_with_the_whole_wave() {
    for width in [32, 64] {
        let (mut shader, mut words, end) = fixture::shader(width);
        words.truncate(end);
        let skip = words.len() as u32 + 1;
        // The first edge bypasses the vote but lands before the prologue.
        words.splice(..0, [0xbf88_0002, 0xbe87_0391, 0xbf88_0000 | skip, 0xbf80_0000]);
        words.push(0xbf81_0000);
        shader.isa = words.into_iter().flat_map(u32::to_le_bytes).collect();
        shader.isa_data = shader.isa.clone();
        // One targeted vote cannot cover this shape, so every branch votes.
        let translated = translate(&shader).expect("whole-wave branches");
        assert_eq!(translated.hlsl.matches("!WaveActiveAnyTrue((exec_lo & 1u) != 0u)").count(), 2);
        assert!(!translated.hlsl.contains("if ((exec_lo & 1u) == 0u)"));
    }
}

#[test]
fn dispatched_lanes_wait_for_their_wave_at_a_barrier() {
    let op = |offset, op, mnemonic: &str, imm| Insn {
        offset, dwords: 1, enc: Enc::Sopp, op, raw: vec![0xbf80_0000 | op << 16], mnemonic: mnemonic.into(),
        dst: Vec::new(), src: Vec::new(), aux: tuxbox_dxbc::rdna2::InsnAux { imm, ..Default::default() },
    };
    let emit = |barrier: bool| {
        let shader = guarded_shader(32, &[]);
        // A lane leaves the loop at its first EXECZ and may reach the barrier before the others.
        let mut emitter = Emitter::new(&shader, vec![
            op(0, 8, "s_cbranch_execz", 1),
            op(4, 2, "s_branch", -2),
            if barrier { op(8, 0x0a, "s_barrier", 0) } else { op(8, 0, "s_nop", 0) },
            op(12, 1, "s_endpgm", 0),
        ]);
        emitter.emit_body_dispatch(false, 0).unwrap();
        emitter.body
    };
    let body = emit(true);
    assert!(body.contains("bool tb_wait = tb_pc == 2u;"), "{body}");
    let block = body.find("if (tb_pc == 2u && tb_sel == 2u) {").expect("the barrier starts a block");
    let wait = body.find("WaveActiveAnyTrue(!tb_wait)").expect("waits for its wave");
    let sync = body.find("GroupMemoryBarrierWithGroupSync();").unwrap();
    assert!(wait < block && block < sync, "{body}");
    assert!(body.contains("uint tb_sel = tb_wait ? 0xfffffffeu : tb_pc;\n"), "{body}");
    let body = emit(false);
    assert!(!body.contains("tb_wait") && !body.contains("tb_sel"), "{body}");
}

#[test]
fn dispatched_blocks_run_in_program_order_within_ranges() {
    let op = |offset, op, mnemonic: &str, imm| Insn {
        offset, dwords: 1, enc: Enc::Sopp, op, raw: vec![0xbf80_0000 | op << 16], mnemonic: mnemonic.into(),
        dst: Vec::new(), src: Vec::new(), aux: tuxbox_dxbc::rdna2::InsnAux { imm, ..Default::default() },
    };
    // Each branch skips the next one, so every instruction starts a block.
    let emit = |branches: usize| {
        let shader = guarded_shader(32, &[]);
        let mut insns: Vec<_> = (0..branches).map(|i| op(4 * i, 8, "s_cbranch_execz", 1)).collect();
        insns.extend((0..2).map(|i| op(4 * (branches + i), 0, "s_nop", 0)));
        insns.push(op(4 * (branches + 2), 1, "s_endpgm", 0));
        let mut emitter = Emitter::new(&shader, insns);
        emitter.emit_body_dispatch(false, 0).unwrap();
        emitter.body
    };
    for (branches, ranges) in [(10, 0), (40, 2), (600, 2 + 26)] {
        let body = emit(branches);
        let blocks: Vec<_> = (0..branches + 2).map(|k| body.find(&format!("if (tb_pc == {k}u) {{")).expect("block")).collect();
        // A forward jump lands in a block that is still to come in the same pass.
        assert!(blocks.is_sorted(), "{branches}");
        assert_eq!(body.matches("if (tb_pc - ").count(), ranges, "{branches}");
        assert_eq!(body.matches('{').count(), body.matches('}').count(), "{branches}");
        assert!(!body.contains("0xffffffffu"), "{branches}");
        // project_tuxbox/runtime_shaders.py picks its DXC options by this text.
        assert!(body.contains("while (tb_pc != "), "{branches}");
    }
}

#[test]
fn reduction_execz_refuses_block_dispatch() {
    let shader = guarded_shader(32, &[]);
    let mut emitter = Emitter::new(&shader, Vec::new());
    emitter.reduction_execz_guards.insert(0);
    assert!(emitter.emit_body_dispatch(false, 0).unwrap_err().contains("EXECZ vote"));
}
