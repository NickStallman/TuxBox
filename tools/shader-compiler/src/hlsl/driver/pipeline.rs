//! Prepares a program, picks its lowering and emits it.
use super::*;

pub(in crate::hlsl) fn compute_group(ex: &ShaderExtract) -> [u32; 3] {
    if let Some(threads) = ex.psv_threads { return threads; }
    let [_, x, y, z] = ex.isa_preamble;
    [x.max(1), y.max(1), z.max(1)]
}

/// Decide whether the loop closing at the backward branch `index` keeps a shared
/// exchange safe.
///
/// Every invocation in the group must execute the loop the same number of times,
/// or the group would dead-lock at the exchange's barriers (or observe half-written
/// shared memory). That holds for a single 32-lane host wave with a uniform
/// back-edge (unconditional or SCC) and no other branch in the loop body:
/// SCC is then identical for every thread in the wave, and no per-invocation
/// exit exists.
///
/// Refusals kept for everything else:
/// - VCC branches (per-lane exit) and EXEC branches (per-thread bit in this emitter),
/// - inverted conditions (s_cbranch_execz / s_cbranch_vccz): the console takes the
///   branch only when the whole wave's mask is zero, but a vote over a per-lane
///   condition repeats while any single lane's bit is clear, so it needs a
///   uniform condition rather than a vote it cannot keep in step,
/// - multi-wave groups (independent SCC per host wave),
/// - any other branch inside the body (forward rejoins, continues, nested loops),
///   since any of those can skip the exchange or a barrier on a per-invocation basis.
fn exchange_loop_safe(insns: &[crate::rdna2::Insn], count: u32, branch_index: usize, head: i64) -> bool {
    if count > 32 {
        return false;
    }
    let branch = &insns[branch_index];
    // Unconditional (2) and SCC branches (4, 5) are uniform for the wave. Anything
    // else - per-lane VCC/EXEC, or the inverted (taken-when-zero) polarities - is refused.
    if !matches!(branch.op, 2 | 4 | 5) {
        return false;
    }
    let back = branch.offset as i64;
    for (index, ins) in insns.iter().enumerate() {
        if index == branch_index {
            continue;
        }
        let from = ins.offset as i64;
        if from < head || from > back {
            continue;
        }
        if ins.enc == Enc::Sopp && (Emitter::branch_cond(ins.op).is_some() || ins.op == 1) {
            return false;
        }
    }
    true
}

pub fn translate(ex: &ShaderExtract) -> Result<Translated, String> {
    translate_linked(ex, None)
}

/// [`translate`] for a host whose waves are at most 32 lanes. Only a program
/// whose ordinary translation needs 64-lane waves differs.
pub fn translate_for_wave32_host(ex: &ShaderExtract) -> Result<Translated, String> {
    let ordinary = translate(ex)?;
    if ex.stage != Stage::Compute || ordinary.required_wave_size != Some(64) {
        return Ok(ordinary);
    }
    let paired = translate_paired_wave64(ex).and_then(|paired| match paired.hlsl.len() {
        size if size > MAX_HLSL_BYTES => Err(format!("paired wave64: {} MiB of HLSL", size >> 20)),
        _ => Ok(paired),
    });
    paired.or_else(|error| {
        // Whole lane masks need the guest's wave width. Where the paired form cannot run
        // the program, a narrow host keeps the translation it had without them.
        match mask_ballots::without(|| translate(ex)) {
            Ok(narrow) if narrow.required_wave_size != Some(64) => Ok(narrow),
            _ => Err(error),
        }
    })
}

/// [`translate`] for the last stage before the rasteriser (VS or GS), given the
/// [`Translated::ps_input_interp`] of the pixel shader it feeds: the outputs that pixel
/// shader reads get the same interpolation modifiers, so both stages pack their signatures
/// into the same registers. Ignored for other stages.
pub fn translate_linked(ex: &ShaderExtract, feeds_ps: Option<&InputInterp>) -> Result<Translated, String> {
    translate_ordered(ex, feeds_ps, &[])
}

/// Preserve every output and its assignment. Declare shared fields in the pixel
/// signature's order so producer-only fields cannot displace rasterizer inputs.
pub fn translate_ordered(
    ex: &ShaderExtract, feeds_ps: Option<&InputInterp>, output_order: &[String],
) -> Result<Translated, String> {
    translate_outputs(ex, feeds_ps, output_order, None)
}

/// Redirect the pixel stencil reference to an unused integer render target.
/// The host binds a mirror of the original stencil plane at this index.
pub fn translate_stencil_target(ex: &ShaderExtract, slot: u32) -> Result<Translated, String> {
    if ex.stage != Stage::Pixel || slot >= 8
        || ex.output_sig.iter().filter(|e| e.sysval == 69 && e.comp_type == 1 && e.mask == 1).count() != 1
        || ex.output_sig.iter().any(|e| e.sysval == 64 && e.index >= slot)
    {
        return Err("invalid stencil render target".into());
    }
    translate_outputs(ex, None, &[], Some(slot))
}

pub(in crate::hlsl) fn translate_outputs(ex: &ShaderExtract, feeds_ps: Option<&InputInterp>, output_order: &[String],
    stencil_target: Option<u32>) -> Result<Translated, String> {
    let translated = translate_outputs_per_lane(ex, feeds_ps, output_order, stencil_target);
    // Where no one-lane-per-thread form runs the program, the two-lane form may: it
    // keeps both halves of a console wave and branches once for the whole wave.
    match translated {
        Err(reason) if stencil_target.is_none() && paired_wave64::takes_over(ex, &reason) => {
            translate_paired_wave64(ex).ok().filter(|paired| paired.hlsl.len() <= MAX_HLSL_BYTES).ok_or(reason)
        }
        other => other,
    }
}

fn translate_outputs_per_lane(ex: &ShaderExtract, feeds_ps: Option<&InputInterp>, output_order: &[String],
    stencil_target: Option<u32>) -> Result<Translated, String> {
    let first = translate_outputs_before_key_counts(ex, feeds_ps, output_order, stencil_target);
    if first.is_ok() || ex.stage != Stage::Compute || stencil_target.is_some() {
        return first;
    }
    // Only a program refused every other way takes the per-key count rewrite.
    let mut insns = disassemble(&ex.isa);
    if lower_local_pc_transfers(&mut insns).is_err() {
        return first;
    }
    match with_key_counts(&insns, || translate_outputs_before_key_counts(ex, feeds_ps, output_order, stencil_target)) {
        Some(Ok(retried)) => Ok(retried),
        _ => first,
    }
}

fn translate_outputs_before_key_counts(ex: &ShaderExtract, feeds_ps: Option<&InputInterp>, output_order: &[String],
    stencil_target: Option<u32>) -> Result<Translated, String> {
    let mut lane_read_failed = false;
    let first = translate_outputs_once_impl(ex, feeds_ps, output_order, stencil_target, &mut lane_read_failed);
    // Interleaved loops go through the block dispatcher, where only a program whose
    // every branch is the whole wave's keeps its lanes together at a reduction.
    let first = match first {
        Err(reason) if reason == lockstep::DISPATCHED_VOTE => lockstep::without_guards(|| {
            translate_outputs_once_impl(ex, feeds_ps, output_order, stencil_target, &mut false)
        }).or(Err(reason)),
        other => other,
    };
    if lane_read_failed && ex.stage == Stage::Compute && stencil_target.is_none()
        && first.as_ref().is_err_and(|reason| reason.contains("inside non-uniform loop"))
    {
        // Keeping the wave together makes every loop one the reads can use.
        return lockstep::retry(|| translate_outputs_once(ex, feeds_ps, output_order, stencil_target)).or_else(|e| {
            if std::env::var_os("TB_DFDEBUG").is_some() { eprintln!("{e}"); }
            first
        });
    }
    if first.is_ok() || lane_read_failed || ex.stage != Stage::Compute || stencil_target.is_some() { return first; }
    // Once this exact exchange subset is recognized, a later binding or
    // emitter error must not silently replace its collectives with copies.
    let mut original = disassemble(&ex.isa);
    if original.iter().any(is_binary_quad_perm) || group_extrema::prepare_exchanges(ex, &mut original).unwrap_or(true) {
        // This standalone exchange observes its source lane even if the exact
        // translation failed elsewhere. A per-lane retry would drop that operand.
        return first;
    }
    if (ex.root_sig.is_some() || original.iter().all(|i| !matches!(i.enc, Enc::Smem | Enc::Mubuf)))
        && wave_sum::straight_exchange(&mut original, compute_group(ex)) {
        return first;
    }
    // Appends only: each lane may allocate for itself.
    let alone = lone_lane::retry(|| translate_outputs_once(ex, feeds_ps, output_order, stencil_target))
        .and_then(|t| lone_lane::check(&t.hlsl).map(|()| t));
    match alone {
        Ok(translated) => Ok(translated),
        Err(e) => {
            if std::env::var_os("TB_DFDEBUG").is_some() { eprintln!("{e}"); }
            first
        }
    }
}

pub(in crate::hlsl) fn translate_outputs_once(ex: &ShaderExtract, feeds_ps: Option<&InputInterp>, output_order: &[String],
    stencil_target: Option<u32>) -> Result<Translated, String> {
    let mut lane_read_failed = false;
    translate_outputs_once_impl(ex, feeds_ps, output_order, stencil_target, &mut lane_read_failed)
}

fn translate_outputs_once_impl(ex: &ShaderExtract, feeds_ps: Option<&InputInterp>, output_order: &[String],
    stencil_target: Option<u32>, lane_read_failed: &mut bool) -> Result<Translated, String> {
    if stencil_target.is_none() {
        if let Some(hlsl) = crate::independent_hlsl(ex) {
            let compute = ex.stage == Stage::Compute;
            return Ok(Translated {
                hlsl, entry: "main", profile: if crate::requires_inline_ray_query(ex) { "cs_6_5" } else if compute { "cs_5_0" } else { "ps_5_0" },
                needs_barycentrics: false, typed_int64_atomics: false, packed_dot_product: false, required_wave_size: None,
                portable_compute_group: if compute { ex.psv_threads } else { None },
                ps_input_interp: InputInterp::default(),
            });
        }
    }
    if ex.stage == Stage::Mesh {
        return mesh::translate(ex, feeds_ps, output_order);
    }
    if persistent::applies(ex) {
        return persistent::translate(ex);
    }
    let (profile, is_ps) = match ex.stage {
        Stage::Vertex => ("vs_5_0", false),
        Stage::Pixel => ("ps_5_0", true),
        Stage::Compute => ("cs_5_0", false),
        Stage::Geometry => ("gs_5_0", false),
        other => return Err(format!("stage {other:?} not supported yet")),
    };
    let mut insns = disassemble(&ex.isa);
    // A console ray walk is replaced before any lane analysis: its shared
    // stack and cross-lane traffic leave with it.
    let scarlett_walk = scarlett_rays::identify(ex);
    if let Some(walk) = scarlett_walk { scarlett_rays::prepare(walk, &mut insns); }
    if lone_lane::on() { lone_lane::prepare(&mut insns)?; }
    // Mixed lane storage uses the full-mask emitter after structural validation.
    let mixed_lane_wave32 = !lone_lane::on()
        && mixed_lane::prepare(ex, &mut insns).inspect_err(|_| *lane_read_failed = true)?;
    *lane_read_failed |= mixed_lane_wave32;
    let resolve_wave32 = mixed_lane_wave32 || !lone_lane::on() && ray_resolve::wave32(ex, &insns)?;
    let probe = radiance_probe::identify(ex);
    let probe_trace = probe.is_some() || scarlett_walk.is_some();
    if let Some(probe) = probe { radiance_probe::prepare(probe, &mut insns); }
    // Slot 80 explicitly selects a wave width. Only opt in straight-line
    // loads, stores and lane-local arithmetic with ordinary operands.
    let portable_compute_group = (ex.stage == Stage::Compute && insns.iter().all(|ins| {
        if ins.mnemonic == "v_nop" { return true; } // driver hash marker
        matches!(ins.mnemonic.as_str(), "s_endpgm" | "s_waitcnt" | "s_load_dwordx4"
            | "buffer_load_dword" | "buffer_store_dword" | "v_mov_b32" | "v_add_nc_u32"
            | "v_lshlrev_b32" | "v_sub_nc_u32" | "v_mul_f32" | "v_add_f32" | "v_and_b32"
            | "v_or_b32" | "v_xor_b32")
            && ins.src.iter().chain(&ins.dst).all(|op| matches!(op, Opnd::Sgpr(_) | Opnd::Vgpr(_)
                | Opnd::ConstInt(_) | Opnd::ConstF32(_) | Opnd::Literal(_)))
    })).then(|| compute_group(ex));
    lower_local_pc_transfers(&mut insns)?;
    // A lane kept to itself cannot stand in for these whole-wave loops.
    let key_counts = if ex.stage == Stage::Compute && !lone_lane::on() && rewriting() { lower_key_counts(&mut insns) } else { Vec::new() };
    let entry_fallback = ex.stage == Stage::Compute && indexed_lane::skip_empty_wave_fallback(&mut insns);
    let single_wave32 = (entry_fallback || probe_trace) && compute_group(ex).into_iter().fold(1u32, u32::saturating_mul) == 32
        && insns.iter().any(|op| op.mnemonic == "s_and_saveexec_b32")
        && insns.iter().any(|op| op.mnemonic.starts_with("v_cmp_") && matches!(op.dst[..], [Opnd::Sgpr(n)] if n & 1 != 0));
    let particle_sort = particle_sort::prepare(ex, &mut insns);
    let hair_cluster_export = hair_cluster_export::prepare(ex, &mut insns);
    let hair_cluster = hair_cluster::prepare(ex, &mut insns);
    let instance_cull = instance_cull::prepare(ex, &mut insns);
    let hzb_cull = instance_cull_hzb::prepare(ex, &mut insns);
    let view_cull = instance_cull_views::prepare(ex, &mut insns);
    let post_cull = instance_cull_post::prepare(ex, &mut insns);
    if !post_cull && !instance_cull && !hzb_cull && view_cull.is_none() && ex.stage == Stage::Compute
        && ex.entry.as_deref() == Some("InstanceCull") && compute_group(ex) == [64, 1, 1]
    {
        return persistent::translate(ex);
    }
    let hair_visibility_indirect = hair_visibility_indirect::prepare(ex, &mut insns);
    let hair_visibility_single = hair_visibility_single::prepare(ex, &mut insns);
    let hair_compact = hair_compact::prepare(ex, &mut insns);
    let subsurface_setup = subsurface_setup::prepare(ex, &mut insns);
    let material_classify = ps09_classify::prepare(ex, &mut insns);
    let classify_tiles = classify_materials::prepare(ex, &mut insns);
    singleton_wave::prepare(ex, &mut insns);
    let hair_multisample = hair_multisample::recognize(ex);
    let fspx_wave = fspx_wave::width(ex, &insns);
    let wave_cull = fspx_wave.is_some() || hair_multisample || wave_cull::recognize(ex);
    let page_waves = page_waves::recognize(ex);
    let generic_waves = !lone_lane::on() && !wave_cull && !page_waves;
    let election = if generic_waves && !resolve_wave32 { straight_election(ex, &insns) } else { None };
    let allocation_wave64 = generic_waves && mark_wave_allocations(&mut insns, resolve_wave32, election)?;
    let wave_alloc_width = if ex.stage == Stage::Pixel || ex.stage == Stage::Compute {
        wave_alloc::prepare(&mut insns, ex.stage)?
    } else {
        None
    };
    let reflection = reflection_compact::prepare(ex, &mut insns);
    group_extrema::prepare_exchanges(ex, &mut insns)?;
    let group_extrema = group_extrema::prepare(ex, &mut insns);
    let straight_exchange = ex.stage == Stage::Compute && !lone_lane::on()
        && !wave_cull && !page_waves && !resolve_wave32 && !allocation_wave64
        && (ex.root_sig.is_some() || insns.iter().all(|i| !matches!(i.enc, Enc::Smem | Enc::Mubuf)))
        && wave_sum::straight_exchange(&mut insns, compute_group(ex));
    let required_wave_size = if wave_cull {
        fspx_wave.filter(|&width| width == 32)
    } else if straight_exchange || lockstep::for_lane_reads() { None }
      else { lower_wave_reductions(&mut insns)? };
    // A masked DPP prefix/total scan is a whole-wave collective: the host wave must
    // match the guest wave width (32 or 64 lanes). Only compute shaders need this
    // constraint — a pixel is a wave of its own.
    let required_wave_size = if ex.stage == Stage::Compute {
        match (required_wave_size, wave_alloc_width) {
            (None, w) => w,
            (Some(a), Some(b)) if a != b => return Err("conflicting wave widths for wave allocation and reduction".into()),
            (a, _) => a,
        }
    } else {
        required_wave_size
    };
    let required_wave_size = if resolve_wave32 {
        // Mixed lane storage pins host lanes to the guest's, so a quad exchange reads the
        // lane it names. The ray resolve has no verified exchange.
        if matches!(required_wave_size, Some(64)) || !mixed_lane_wave32 && insns.iter().any(is_quad_exchange) {
            return Err("resolve Wave32 requires a compatible wave reduction".into());
        }
        Some(32)
    } else { required_wave_size };
    let uses_bpermute = insns.iter().any(|ins| ins.mnemonic == "ds_bpermute_b32");
    if uses_bpermute && insns.iter().any(|ins| ins.enc == Enc::Sopp && Emitter::branch_cond(ins.op).is_some()) {
        return Err("backward permute with scalar branches needs verified convergent control flow".into());
    }
    let uses_quad_add = insns.iter().any(is_quad_exchange);
    let scan = if wave_cull { None } else { wave_scan::prepare(ex, &mut insns)? };
    let scan_width = scan.as_ref().map(|s| s.width);
    let ordinary_wave = !wave_cull && !page_waves && !lone_lane::on();
    // A waterfall loop is one per-lane read of a lane; exchange it once.
    let waterfall_gathers = ex.stage == Stage::Compute && !resolve_wave32 && !lockstep::for_lane_reads()
        && lower_waterfall_gathers(&mut insns) > 0;
    let group = compute_group(ex);
    let group_count = group.into_iter().try_fold(1u32, u32::checked_mul).unwrap_or(0);
    let allow_group_read_fallback = ex.stage == Stage::Compute
        && !lockstep::for_lane_reads()
        && !resolve_wave32
        && ordinary_wave
        && scan_width.is_none()
        && group_count > 0
        && group_count <= 1024
        && !insns.iter().any(|i| i.enc == Enc::Ds && i.mnemonic != "ds_swizzle_b32" && i.mnemonic.starts_with("ds_ordered"));
    let prepared_election_lane = !resolve_wave32 && ex.stage == Stage::Compute
        && indexed_lane::prepare(group, &mut insns);
    let fspx_wave32 = ex.fspx_config.is_some_and(|cfg| cfg[4] == 0x100001);
    let read_plan = if lockstep::for_lane_reads() {
        if !ordinary_wave || resolve_wave32 || scan_width.is_some() || required_wave_size.is_some_and(|w| w != 32) {
            return Err("whole-wave lane reads conflict with another wave lowering".into());
        }
        lockstep::lower_lane_reads(group, &mut insns)?;
        indexed_lane::ConstantReadPlan::default()
    } else if !resolve_wave32 && ordinary_wave && ex.stage == Stage::Compute {
        indexed_lane::lower_constant_reads(
            group,
            &mut insns,
            required_wave_size.or(scan_width),
            fspx_wave32,
            ordinary_wave,
            allow_group_read_fallback,
        )
        .inspect_err(|_| *lane_read_failed = true)?
    } else {
        indexed_lane::ConstantReadPlan::default()
    };
    let indexed_lane = !resolve_wave32
        && (election == Some(ElectionLowering::Shared64)
            || group_extrema
            || prepared_election_lane
            || read_plan.uses_shared);
    let indexed_lane_wave32 = read_plan.uses_shared
        && read_plan.shared_wave_width == Some(32)
        && election != Some(ElectionLowering::Shared64)
        && !group_extrema
        && !prepared_election_lane;
    let wave32_readlane = read_plan.uses_native_wave32;
    let has_group_read_fallback = read_plan.group_read_width.is_some();
    if !wave_cull && !page_waves { {
        // Complete-wave compute groups can exchange unproven dynamic SGPR lane reads
        // through group memory with wave-width masking (`& 31u` or `& 63u`).
        if ex.stage == Stage::Pixel { lower_first_lane_reads(&mut insns); }
        if !mixed_lane_wave32 {
            lower_lane_spills(&mut insns, resolve_wave32, has_group_read_fallback)?;
        }
        if let Some(width) = read_plan.group_read_width {
            for ins in insns.iter_mut().filter(|i| i.mnemonic == GROUP_READ) {
                ins.aux.imm = i64::from(width - 1);
            }
        }
    } }
    let mut em = Emitter::new(ex, insns);
    if let Some(mode) = election {
        if compute_group(ex).into_iter().product::<u32>() <= 32 || em.insns.iter().any(|i| matches!(i.mnemonic.as_str(), "tb_first_exec_lane" | "s_ff1_i32_b64") && i.src == [Opnd::ExecLo]) {
            em.exec_snapshots = exec_snapshot::analyze(&em.insns, mode);
        }
    }
    let shared_mask = em.exec_snapshots.sites.values().any(|site| matches!(site, exec_snapshot::Site::Capture(ElectionLowering::Shared64)));
    em.page_waves = page_waves;
    em.wave_cull = wave_cull;
    em.fspx_wave = fspx_wave;
    em.hair_multisample = hair_multisample;
    em.hoist_wave_exchanges = wave_cull && (compute_group(ex) == [16, 16, 1]
        || ex.entry.as_deref() == Some("GenerateReflectionRaysCS")
        || (ex.entry.as_deref() == Some("MainCS") && [
            (2356, 0xe4995b0e2291e61e), (2884, 0x1c9821104747da89),
        ].into_iter().any(|(size, hash)| packed_root::matches_program(&ex.isa, size, hash))));
    em.indexed_lane = indexed_lane;
    em.indexed_lane_wave32 = indexed_lane_wave32;
    em.single_wave32 = single_wave32;
    em.resolve_wave32 = resolve_wave32;
    em.wave32_compare_masks |= reflection && matches!(ex.isa.len(), 1024 | 1028);
    if matches!(ex.stage, Stage::Vertex | Stage::Geometry) {
        em.out_interp = feeds_ps.cloned().unwrap_or_default();
    }
    em.vreg_array = em.insns.iter().any(|i| i.mnemonic.starts_with("v_movrel"));
    for count in 0..em.insns.len() {
        if em.insns[count].mnemonic != COUNT_LANES { continue; }
        let mask = em.insns[count].src[0];
        let def = (0..count).rev().find(|&j| em.insns[j].dst.first() == Some(&mask) && !em.insns[j].mnemonic.starts_with("v_cmpx"))
            .ok_or("lane count without its predicate")?;
        em.vote_defs.insert(def);
        em.vote_uses.insert(count, def);
        em.regs.insert(format!("tb_votes{def}"));
    }
    // Ray tracing resolve keeps whole masks already.
    if ex.stage == Stage::Compute && !lone_lane::on() && !wave_cull && !page_waves && !indexed_lane
        && em.fspx_wave.is_none() && !hair_multisample && !resolve_wave32 && !mask_ballots::off() {
        em.mask_ballots = mask_ballots::find(&em.insns);
    }
    // Lane-spill slots beyond the tracked range are function-scope temporaries.
    for ins in &em.insns {
        for o in ins.dst.iter().chain(ins.src.iter()) {
            if let Opnd::Temp(n) = o {
                if *n >= LANE_TEMP_BASE {
                    em.regs.insert(format!("tb_s{n}"));
                }
            }
        }
    }
    if !em.resolve_packed(scan.as_ref())? {
        if scan_width.is_some() { return Err("wave scan needs a verified packed root layout".into()); }
        em.resolve_resources()?;
    }
    counters_are_private(&em, &key_counts)?;
    if allocation_wave64 {
        // Both following stores write list data to the same typed view: the light
        // index and the previous uint head returned by the indexed atomic exchange.
        // Validate the producer and addresses instead of inferring the element
        // type from the PSV kind alone.
        let mut list_slots = BTreeSet::new();
        for (i, _) in em.insns.iter().enumerate().filter(|(_, i)| i.mnemonic == "tb_allocation_prefix_hi") {
            let tail = em.insns.get(i + 9..i + 14).ok_or("missing allocated list writes")?;
            if tail[0].raw != [0xe0c06000, 0x80080801] || tail[1].raw != [0x38140e81]
                || tail[2].raw != [0xe0102000, 0x80060907] || tail[3].raw != [0xbf8c3f70]
                || tail[4].raw != [0xe0102000, 0x8006080a] {
                return Err("unverified list stores after wave allocation".into());
            }
            let Some([Res::TBuf(slot, true)]) = em.res.get(&(i + 11)).map(Vec::as_slice) else {
                return Err("missing list UAV for wave allocation".into());
            };
            if em.res.get(&(i + 13)) != em.res.get(&(i + 11)) || !em.ex.resources.iter().any(|r|
                r.space == 0 && r.res_type == 6 && r.lower <= *slot && *slot <= r.upper) {
                return Err("conflicting list UAV bindings".into());
            }
            list_slots.insert(*slot);
        }
        em.tbuf_uint.extend(list_slots);
    }
    let light_grid = allocation_wave64 && light_grid::prepare(&mut em)?;
    let requires_ballot = (allocation_wave64 && !light_grid) || em.packed.as_ref().is_some_and(|p| !p.ballots.is_empty());
    if (requires_ballot && scan_width.or(required_wave_size) == Some(32))
        || matches!((scan_width, required_wave_size), (Some(a), Some(b)) if a != b) {
        return Err("conflicting wave widths for ballot and reduction".into());
    }
    let register_wave_size = em.packed.as_ref().and_then(|plan| plan.register_wave_size);
    // A pixel is a wave of its own here (see `v_readfirstlane_b32`), so a reduction
    // yields that pixel's value.
    em.pixel_wave = ex.stage == Stage::Pixel && em.pixel_heap && scan_width.is_none() && register_wave_size.is_none()
        && em.insns.iter().all(|i| i.op != wave_sum::DPP_REDUCE || i.enc != Enc::Vop2
            || matches!(i.mnemonic.as_str(), "v_and_b32" | "v_or_b32" | "v_min_u32" | "v_max_u32" | "v_min_f32" | "v_max_f32" | "v_add_nc_u32" | "v_add_f32"));
    em.straight_pixel_quad = (ex.stage == Stage::Pixel && !em.pixel_heap)
        .then(|| pixel_quad::straight_prefix(&em.insns)).flatten();
    let required_wave_size = if em.pixel_wave { None } else { required_wave_size };
    let required_wave_size = required_wave_size.or(scan_width).or(register_wave_size).or((requires_ballot || !em.vote_uses.is_empty()).then_some(64));
    // With more than 32 threads, rows 2 and 3 could instead be rows 0 and 1 of
    // a second wave. The fallback host width cannot resolve an asymmetric guest mask.
    if uses_quad_add && required_wave_size.is_none() && !em.one_word_vector_mask()
        && compute_group(ex).into_iter().fold(1u32, u32::saturating_mul) > 32
        && em.insns.iter().filter(|i| is_quad_exchange(i) && i.enc == Enc::Vop1).any(|i| {
            let rows = i.raw[1] >> 28;
            i.raw[1] & 0x0f00_0000 != 0 && rows & 3 != rows >> 2
        })
    {
        return Err("asymmetric DPP16 row mask requires a known guest wave width".into());
    }
    let wave32_mbcnt_mask = indexed_lane_wave32
        && em.insns.iter().any(|i| {
            i.mnemonic == "v_mbcnt_lo_u32_b32"
                && matches!(i.src.first(), Some(Opnd::ExecLo | Opnd::ExecHi))
        });
    if (wave32_readlane || wave32_mbcnt_mask) && required_wave_size == Some(64) {
        *lane_read_failed = true;
        return Err("conflicting wave widths for Wave32 lane read".into());
    }
    let required_wave_size = required_wave_size.or((uses_quad_add && em.straight_pixel_quad.is_none() && (!em.pixel_wave || em.insns.iter().any(is_row_move)) || uses_bpermute || group_extrema || wave32_readlane || wave32_mbcnt_mask || lockstep::for_lane_reads()).then_some(32));
    // A ballot or minimum over active invocations also works in the final
    // partial subgroup. Keep the full-wave requirement for every other user.
    let partial_election = required_wave_size.is_none() && election == Some(ElectionLowering::Native32);
    let required_wave_size = required_wave_size.or_else(|| {
        (election == Some(ElectionLowering::Native32) && (!em.exec_snapshots.sites.is_empty() || em.insns.iter().any(|i|
            i.mnemonic == "tb_first_active_lane" || i.mnemonic == "s_ff1_i32_b64"))).then_some(32)
    });
    // A ballot holds the guest's own lanes, so it needs whole waves of the guest's width.
    let reductions_wave_size = required_wave_size;
    let required_wave_size = match em.mask_ballots.as_ref().map(|b| b.width) {
        Some(width) if reductions_wave_size.is_none_or(|w| w == width)
            && compute_group(ex).into_iter().try_fold(1u32, u32::checked_mul).is_some_and(|n| n > 0 && n % width == 0) => Some(width),
        _ => {
            em.mask_ballots = None;
            reductions_wave_size
        }
    };
    em.required_wave_size = required_wave_size;
    // DS bit-mask mode operates on the low five lane bits in wave32 and wave64.
    // Map guest lanes by workgroup index, independently of the Vulkan subgroup layout.
    em.compute_swizzle = ex.stage == Stage::Compute && scan_width.is_none() && !resolve_wave32
        && (em.insns.iter().any(|i| i.mnemonic == "ds_swizzle_b32") || has_group_read_fallback || waterfall_gathers);
    if em.compute_swizzle {
        let [x, y, z] = compute_group(ex);
        let count = x.checked_mul(y).and_then(|n| n.checked_mul(z)).unwrap_or(0);
        if count == 0 || count > 1024 || count % 32 != 0 {
            return Err("compute swizzle requires complete blocks of 32 lanes".into());
        }
        let last = em.insns.iter().rposition(exchanges).unwrap_or(0);
        if em.insns[..last].iter().any(|i| i.mnemonic == "s_endpgm") {
            return Err("compute swizzle following an early exit".into());
        }
        // Moving a shared exchange out of a forward guest branch is safe once every
        // invocation rejoins. A backward branch could execute it different numbers of times.
        // Only the loops proven safe here keep a whole-wave vote in the emitter; every
        // other loop emits the per-thread condition it always has.
        let mut accepted_swizzle_loops = std::collections::BTreeSet::new();
        for (index, branch) in em.insns.iter().enumerate().filter(|(_, i)| i.enc == Enc::Sopp && Emitter::branch_cond(i.op).is_some() && i.aux.imm < 0) {
            let head = branch.offset as i64 + 4 + 4 * branch.aux.imm;
            if exchange_loop_safe(&em.insns, count, index, head) {
                accepted_swizzle_loops.insert(index);
                continue;
            }
            if em.insns.iter().any(|i| exchanges(i) && (head..=branch.offset as i64).contains(&(i.offset as i64))) {
                return Err("compute swizzle inside a loop".into());
            }
        }
        em.accepted_swizzle_loops = accepted_swizzle_loops;
    }
    if (em.compute_swizzle || em.indexed_lane) && wave_cull::lds_words(&em).is_none()
        && em.insns.iter().any(|i| i.enc == Enc::Ds && i.mnemonic != "ds_swizzle_b32") {
        let bytes = tuxbox_dxbc::fssm::compute_lds_bytes(&ex.fssm)
            .ok_or("shared exchange requires the native guest LDS allocation")?;
        let threads = compute_group(ex).into_iter().try_fold(1u32, u32::checked_mul).ok_or("invalid group size")?;
        if bytes == 0 || bytes + 4*threads > 32768 { return Err("guest LDS and exchange exceed the group shared budget".into()); }
        em.reflected_lds_words = Some(bytes/4);
    }
    // Mixed lane storage requires uniform host participation even without reductions.
    if mixed_lane_wave32 && !wave_loops::uniform_dispatch(&em) {
        return Err("mixed lane Wave32 requires proven whole-wave scalar dispatch".into());
    }
    if lockstep::for_lane_reads() {
        if required_wave_size != Some(32) || em.compute_swizzle || em.indexed_lane {
            return Err("whole-wave lane reads conflict with another wave lowering".into());
        }
        em.lockstep = Some(lockstep::prove(&em.insns, 32).ok_or("whole-wave lane reads: masks and numbers mix")?);
    }
    if let Some(width) = required_wave_size.filter(|_| !lockstep::for_lane_reads()) {
        // A guard's vote does not survive the block dispatcher (see `translate_outputs`).
        let checked = wave_sum::check_control_flow(&em.insns, width).and_then(|guards| {
            if lockstep::instead_of_guards() && !guards.is_empty() { Err(lockstep::DISPATCHED_VOTE.into()) } else { Ok(guards) }
        });
        match checked {
            Ok(guards) => em.reduction_execz_guards = guards,
            Err(reason) if em.resolve_wave32 && !requires_ballot && scan_width.is_none() => {
                // A termination proof lets bounded loops drop the watchdog. Without
                // one, uniform scalars still keep the lanes together.
                match wave_loops::bounded_latches(&em) {
                    Some(latches) => em.wave_bounded_latches = latches,
                    None if wave_loops::uniform_dispatch(&em) => {}
                    None => return Err(reason),
                }
            }
            Err(reason) => {
                let ordinary = ex.stage == Stage::Compute && !em.wave_cull && !em.page_waves
                    && !em.resolve_wave32 && !lone_lane::on();
                if !ordinary {
                    return Err(reason);
                }
                match lockstep::prove(&em.insns, width) {
                    Some(proof) => em.lockstep = Some(proof),
                    None => {
                        // Lockstep cannot prove this program; lower the totals that
                        // wave_sum rejects on control flow to WaveActiveSum, a
                        // whole-wave op that is safe in divergent control flow, and
                        // re-validate the reductions that remain. When the new
                        // lowering cannot prove the program, fall back to the
                        // previous behaviour: reject with the wave_sum reason.
                        if wave_alloc::lower_problematic_totals(&mut em.insns, width).is_err() {
                            return Err(reason);
                        }
                        match wave_sum::check_control_flow(&em.insns, width) {
                            Ok(guards) => em.reduction_execz_guards = guards,
                            Err(_) => return Err(reason),
                        }
                    }
                }
            }
        }
        let [x, y, z] = compute_group(ex);
        if ex.stage != Stage::Compute || !x.checked_mul(y).and_then(|n| n.checked_mul(z)).is_some_and(|n| n % width == 0 || partial_election && n > 0 && n < width) {
            return Err(format!("wave operation requires complete {width}-lane compute waves"));
        }
    }
    em.portable_wave64 = (straight_exchange || (reductions_wave_size == Some(64)
        && compute_group(ex).into_iter().try_fold(1u32, u32::checked_mul) == Some(64)))
        && em.lockstep.is_none()
        && ex.stage == Stage::Compute
        && !wave_cull && !page_waves && !allocation_wave64 && !requires_ballot
        && !em.compute_swizzle && !em.indexed_lane && !em.compute_heap
        && em.packed.is_none() && !uses_quad_add && !uses_bpermute && !probe_trace
        && scan_width.is_none() && !particle_sort && !hair_cluster_export && !hair_cluster
        && !instance_cull && !hzb_cull && view_cull.is_none() && !post_cull
        && !hair_visibility_indirect && !hair_visibility_single && hair_compact.is_none()
        && !subsurface_setup && !material_classify && !classify_tiles && !reflection && !group_extrema
        && (straight_exchange || wave_sum::portable_single_wave(&em.insns));
    // Lockstep counts, reads and branches on masks across the wave itself.
    if em.portable_wave64 || em.lockstep.is_some() { em.mask_ballots = None; }
    em.regs.extend(em.mask_ballots.iter().flat_map(mask_ballots::Ballots::names).collect::<Vec<_>>());
    if straight_exchange && !em.portable_wave64 {
        return Err("straight DPP8 exchange conflicts with a specialized lowering".into());
    }
    if is_ps {
        em.find_export_epilogs();
        em.classify_interpolation();
    }
    em.infer_tbuf_types();
    if matches!(probe, Some(radiance_probe::Probe::ReflectionFar)) {
        // Compacted reflection work reads a count, packed pixel coordinates,
        // and a trace-distance texture with flags in its high bits.
        em.tbuf_ty.insert((false, 13), 1);
        em.tbuf_ty.insert((false, 14), 1);
        em.img_ty.insert((false, 12), 1);
    }
    if wave_cull && ex.entry.as_deref() == Some("WaterTileCatergorisationCS") {
        // Both the dispatch count and the packed XY tile list use R32_UINT.
        em.tbuf_uint.extend([0, 1]);
    }
    if particle_sort {
        // R32_UINT particle count, sort keys and particle indices.
        em.tbuf_ty.extend([((false, 3), 1), ((true, 0), 1), ((true, 1), 1)]);
    }
    // Both 512-thread hair scans load integer counts from R32_UINT and store
    // (exclusive, inclusive) offsets to R32G32_UINT. Their additions pass
    // through LDS, beyond the local register type-propagation window.
    if compute_group(ex) == [512, 1, 1]
        && (packed_root::matches_program(&ex.isa, 444, 0xf9b02aae1588e09b)
            || packed_root::matches_program(&ex.isa, 588, 0x4884ae622354895b))
    {
        for &binding in &em.tbuf {
            em.tbuf_ty.insert(binding, 1);
        }
    }
    if subsurface_setup || (wave_cull && ex.entry.as_deref() == Some("SetupIndirectCS")) {
        // These two R32_UINT UAVs hold both the atomic head and the XY rows.
        em.tbuf_uint.extend([0, 1]);
    }
    if wave_cull && ex.entry.as_deref() == Some("MarkValid_PrepareCS") {
        // t2 contains the integer page-grid dimensions and base. u2 receives
        // pairs of packed coordinates for the deferred page-marking pass.
        em.tbuf_ty.extend([((false, 2), 1), ((true, 2), 1)]);
    }
    if hair_multisample { hair_multisample::resources(&mut em)?; }
    if let Some(layout) = hair_compact { hair_compact::resources(&mut em, layout)?; }
    if hair_visibility_single { hair_visibility_single::resources(&mut em)?; }
    if hair_visibility_indirect { hair_visibility_indirect::resources(&mut em)?; }
    if material_classify { ps09_classify::resources(&mut em)?; }
    if classify_tiles { classify_materials::resources(&mut em)?; }
    if instance_cull { instance_cull::resources(&mut em)?; }
    if post_cull { instance_cull_post::resources(&mut em)?; }
    if hzb_cull { instance_cull_hzb::resources(&mut em)?; }
    if ex.stage == Stage::Compute && ex.entry.as_deref() == Some("InstanceCullVSM") {
        // Scalar loads address individual words of the 800-byte packed views,
        // and the compacted ranges, counters and scene streams have distinct strides.
        for (&(uav, slot), words) in &mut em.sbuf {
            *words = fssm_bindings::structured_words(ex, slot, uav)?;
        }
    }
    if matches!(probe, Some(radiance_probe::Probe::ScreenGather | radiance_probe::Probe::ScreenGatherFar)) {
        // The screen trace packs distance and flags into the integer hit target.
        em.uav_uint.insert(1);
    }
    if let Some(program) = view_cull { program.resources(&mut em)?; }
    if matches!(ex.stage, Stage::Compute | Stage::Pixel)
        && em.packed.is_none() && !em.uav_uint.is_empty() {
        em.resolve_wide_image_atomics()?;
    }
    if ex.entry.as_deref() == Some("MicropolyRasterize") && ex.stage == Stage::Compute {
        micropoly::prepare(&mut em)?;
    }
    em.resolve_structured_strides();
    let live_in = em.live_in();
    em.live = live_in.clone();
    em.view_cull = view_cull;
    em.light_grid = light_grid;
    em.emit_body(is_ps).inspect_err(|e| {
        if e.starts_with("indexed lane read ")
            || e.starts_with("wave32 lane read ")
            || e.starts_with("group lane read ")
        {
            *lane_read_failed = true;
        }
    })?;
    // Each guarded VGPR write would otherwise be its own branch. The region
    // closes before anything other than pure register arithmetic, so this is
    // the same program in every stage, with fewer blocks for DXC and dxil-spirv.
    em.body = exec_group::group(&em.body);
    if hair_cluster_export {
        em.body.insert_str(0, "    uint4 hair_record = 0;\n");
        em.body.push_str(hair_cluster_export::EPILOGUE);
    }
    if hair_cluster {
        em.body.insert_str(0, "    uint4 hair_record = 0;
");
        em.body.push_str(hair_cluster::EPILOGUE);
    }
    if instance_cull {
        em.body.insert_str(0, "    bool cull_candidate = false; uint4 cull_record = 0;\n");
        em.body.push_str(instance_cull::EPILOGUE);
    }
    if post_cull {
        em.body.insert_str(0, "    bool cull_candidate = false; uint4 cull_record = 0;\n");
        em.body.push_str(instance_cull_post::APPEND);
    }
    if hzb_cull {
        em.body.insert_str(0, "    bool cull_candidate = false, hidden = false; uint4 cull_record = 0; uint hidden_id = 0; const uint hidden_view = 0u;\n");
        em.body.push_str(instance_cull_hzb::HIDDEN_APPEND);
        em.body.push_str(instance_cull::EPILOGUE);
    }
    if particle_sort { em.body.push_str(particle_sort::EPILOGUE); }
    let mut hlsl = em.finish(is_ps, output_order, stencil_target)?;
    if hzb_cull {
        hlsl.insert_str(0, &instance_cull::queue_declarations(3));
        hlsl.insert_str(0, instance_cull_hzb::HIDDEN_DECLARATIONS);
    }
    if light_grid { hlsl.insert_str(0, light_grid::DECLARATIONS); }
    if em.group_votes { hlsl.insert_str(0, group_loops::GROUP_ANY); }
    if em.portable_wave64 { hlsl.insert_str(0, &wave_sum::portable_declarations(compute_group(ex))); }
    if shared_mask {
        hlsl.insert_str(0, &page_waves::ballot_declarations(compute_group(ex).into_iter().product()));
    }
    if page_waves || wave_cull {
        if fspx_wave == Some(32) {
            hlsl.insert_str(0, fspx_wave::WAVE32);
        } else {
            let branchless = wave_cull && wave_cull::needs_branchless_ballots(ex);
            hlsl.insert_str(0, &page_waves::declarations(compute_group(ex), branchless));
        }
    }
    if scarlett_walk.is_some() { let d = scarlett_rays::declarations(&hlsl); hlsl.insert_str(0, &d); }
    if let Some(probe) = probe {
        hlsl.insert_str(0, radiance_probe::DECLARATIONS);
        if matches!(probe, radiance_probe::Probe::ReflectionFar | radiance_probe::Probe::Reflection | radiance_probe::Probe::ScreenGather | radiance_probe::Probe::ScreenGatherFar) { hlsl.insert_str(0, "#define TB_SCREEN_GATHER 1\n"); }
        if matches!(probe, radiance_probe::Probe::ReflectionFar | radiance_probe::Probe::FarField | radiance_probe::Probe::ScreenGatherFar) { hlsl.insert_str(0, "#define TB_PROBE_FAR_FIELD 1\n"); }
    }
    if particle_sort { hlsl.insert_str(0, particle_sort::DECLARATIONS); }
    if hair_cluster { hlsl.insert_str(0, hair_cluster::DECLARATIONS); }
    if hair_cluster_export { hlsl.insert_str(0, hair_cluster_export::DECLARATIONS); }
    if instance_cull { hlsl.insert_str(0, &instance_cull::queue_declarations(1)); }
    if post_cull { hlsl.insert_str(0, "groupshared uint cull_mask[2], cull_base;\n"); }
    if let Some(program) = view_cull { hlsl.insert_str(0, &program.declarations()); }
    if material_classify { hlsl.insert_str(0, ps09_classify::DECLARATIONS); }
    hlsl.insert_str(0, &format!("// entry-live registers (read before written): {}
", live_in.join(" ")));
    // A host lays a derivative-taking group out two by two, so a wave of a group wider
    // than 16 threads holds two runs of thread numbers and need not start on a wave
    // boundary. No recorded program is one; refuse instead of trusting the wave to be
    // the console's.
    if ex.stage == Stage::Compute && em.compute_derivatives {
        let [x, y, z] = compute_group(ex);
        let threads = x.saturating_mul(y).saturating_mul(z);
        if x > 16 && (y > 1 || z > 1) && em.required_wave_size.is_some_and(|width| threads > width) {
            return Err("derivatives in a group wider than 16 threads that spans several waves".into());
        }
    }
    // ps_5_0 forces UAV slots to start after the render-target outputs (FXC X4509);
    // SM5.1 has no such rule, and D3D12 accepts SM5.1 bytecode.
    let profile = if em.compute_derivatives || em.resolve_wave32 {
        "cs_6_6"
    } else if em.compute_heap && !is_ps {
        "cs_6_6"
    } else if em.uses_heap_sampler || em.resource_heap_base.is_some() || em.pixel_heap || em.straight_pixel_quad.is_some() {
        "ps_6_6"
    } else if probe_trace || em.mask_ballots.is_some() {
        "cs_6_5"
    } else if is_ps && !em.uav.is_empty() {
        "ps_5_1"
    } else {
        profile
    };
    Ok(Translated {
        hlsl,
        entry: "main",
        profile,
        needs_barycentrics: !em.pulled_attrs.is_empty(),
        packed_dot_product: fspx_wave.is_some(),
        typed_int64_atomics: !em.wide_visibility_uavs.is_empty() || em.packed.as_ref().is_some_and(|p| !p.wide_images.is_empty()),
        required_wave_size: if em.portable_wave64 { None } else { required_wave_size },
        portable_compute_group: portable_compute_group.or((light_grid || em.portable_wave64).then(|| compute_group(ex))),
        ps_input_interp: std::mem::take(&mut em.ps_in_interp),
    })
}
