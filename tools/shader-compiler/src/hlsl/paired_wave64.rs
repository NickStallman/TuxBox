//! Console wave64 compute on 32-lane hardware: each host thread runs two console lanes.
//!
//! Thread `n` of a host wave holds console lanes `n` and `n + 32`. Scalar registers
//! and lane masks keep their full console values, the same in every thread of the
//! wave, so branches and scalar instructions need no rewriting. Vector registers are
//! stored once per lane. A vector instruction is emitted once and run for each lane
//! with that lane's registers and mask bits, and a mask it produces is rebuilt from
//! all 64 results. No part of this needs a barrier.
//!
//! A group of exactly 32 threads is half a console wave: lanes 0..32 run the threads
//! and lanes 32..64 never start. Each host thread still holds two lanes. The upper one
//! begins outside EXEC, so it takes part only where the program widens EXEC itself, as
//! a reduction over all 64 lanes does. Its thread ID, which the console leaves
//! unspecified, continues the lower half's numbering.

use std::collections::BTreeSet;

use super::*;

/// Translate a wave64 compute program for hosts with 32-lane waves.
pub fn translate_paired_wave64(ex: &ShaderExtract) -> Result<Translated, String> {
    let first = paired_once(ex);
    if first.is_ok() {
        return first;
    }
    // As for the ordinary translation, the per-key count rewrite is a last resort.
    let mut insns = disassemble(&ex.isa);
    if lower_local_pc_transfers(&mut insns).is_err() {
        return first;
    }
    match with_key_counts(&insns, || paired_once(ex)) {
        Some(Ok(retried)) => Ok(retried),
        _ => first,
    }
}

fn paired_once(ex: &ShaderExtract) -> Result<Translated, String> {
    let threads = compute_group(ex).into_iter().try_fold(1u32, u32::checked_mul).unwrap_or(0);
    if threads == 0 || !(threads % 64 == 0 || threads == HALF_WAVE) || threads > 2048 {
        return Err("paired wave64 needs whole console waves, or half of one, and at most 2048 threads".into());
    }
    let mut insns = disassemble(&ex.isa);
    lower_local_pc_transfers(&mut insns)?;
    let key_counts = if rewriting() { lower_key_counts(&mut insns) } else { Vec::new() };
    let mut em = Emitter::new(ex, insns);
    em.paired = true;
    em.required_wave_size = Some(32);
    em.vreg_array = false;
    if !em.resolve_packed(None)? {
        em.resolve_resources()?;
    }
    counters_are_private(&em, &key_counts)?;
    em.infer_tbuf_types();
    if em.packed.is_none() && !em.uav_uint.is_empty() {
        em.resolve_wide_image_atomics()?;
    }
    em.resolve_structured_strides();
    let live_in = em.live_in();
    em.live = live_in.clone();
    em.emit_body(false)?;
    // The console differences the four lanes of a quad. In a group that is one row those
    // are four threads in a row, and the host's quads of a one-row group are the same four
    // lanes. In a group with rows the console numbers a derivative-taking program's threads
    // two by two (measured on RDNA2), which `prologue` follows for even widths and heights.
    let [x, y, z] = compute_group(ex);
    if em.compute_derivatives && !(z == 1 && (y == 1 || (x % 2 == 0 && y % 2 == 0))) {
        return Err("paired wave64: implicit derivatives".into());
    }
    let mut hlsl = em.finish(false, &[], None)?;
    hlsl.insert_str(0, &format!("// entry-live registers (read before written): {}\n", live_in.join(" ")));
    Ok(Translated {
        hlsl,
        entry: "main",
        profile: "cs_6_6",
        needs_barycentrics: false,
        packed_dot_product: false,
        typed_int64_atomics: !em.wide_visibility_uavs.is_empty() || em.packed.as_ref().is_some_and(|p| !p.wide_images.is_empty()),
        required_wave_size: Some(32),
        portable_compute_group: None,
        ps_input_interp: InputInterp::default(),
    })
}

/// The group size that fills the lower half of one console wave and one host wave.
pub(super) const HALF_WAVE: u32 = 32;

/// Whether the program keeps EXEC in two words, as only a 64-lane console wave does.
fn console_wave64(insns: &[Insn]) -> bool {
    let exec = |i: &&Insn| i.mnemonic.starts_with("s_") && (i.mnemonic.contains("saveexec") || i.mnemonic.contains("wrexec")
        || i.dst.iter().chain(&i.src).any(|o| matches!(o, Opnd::ExecLo | Opnd::ExecHi)));
    let narrow_compare = |i: &Insn| i.enc == Enc::Vop3 && i.mnemonic.starts_with("v_cmp_")
        && matches!(i.dst.as_slice(), [Opnd::Sgpr(n)] if n & 1 != 0);
    insns.iter().filter(exec).any(|i| i.mnemonic.ends_with("_b64"))
        && !insns.iter().filter(exec).any(|i| i.mnemonic.ends_with("_b32"))
        && !insns.iter().any(narrow_compare)
}

/// Whether the two-lane form can stand in for a one-lane-per-thread translation
/// refused for `reason`, on any host.
///
/// - A group of 32 threads is the lower half of one 64-lane console wave. A
///   reduction over the whole wave reads the lanes that never start.
/// - A lane exchange inside a loop needs every thread to take the loop in step.
///   Here a 64-lane console wave already branches as one.
pub(super) fn takes_over(ex: &ShaderExtract, reason: &str) -> bool {
    if ex.stage != Stage::Compute { return false; }
    let threads = compute_group(ex).into_iter().try_fold(1u32, u32::checked_mul).unwrap_or(0);
    match reason {
        "wave operation requires complete 64-lane compute waves" => threads == HALF_WAVE,
        "compute swizzle inside a loop" => threads != 0 && threads % 64 == 0 && console_wave64(&disassemble(&ex.isa)),
        _ => false,
    }
}

/// A branch on a lane mask tests the whole console mask.
pub(super) fn branch_condition(op: u32) -> Option<&'static str> {
    Some(match op {
        6 => "(vcc_lo | vcc_hi) == 0u",
        7 => "(vcc_lo | vcc_hi) != 0u",
        8 => "(exec_lo | exec_hi) == 0u",
        9 => "(exec_lo | exec_hi) != 0u",
        _ => return None,
    })
}

/// The compute prologue: registers, the two lanes this thread runs, and the host group.
pub(super) fn prologue(em: &Emitter<'_>, header: &mut String, group_register: usize) {
    let [x, y, z] = compute_group(em.ex);
    let threads = x * y * z;
    // Half a console wave is one whole host wave, whose upper lanes start inactive.
    let (host, upper) = if threads == HALF_WAVE { (threads, "0u") } else { (threads / 2, "0xffffffffu") };
    let _ = writeln!(header, "[WaveSize(32)]\n[numthreads({host}, 1, 1)]");
    let _ = writeln!(header, "void main(uint3 gid : SV_GroupID, uint3 gtid : SV_GroupThreadID) {{");
    let _ = writeln!(header, "    uint exec_lo = 0xffffffffu, exec_hi = {upper}, vcc_lo = 0u, vcc_hi = 0u, scc = 0u, m0 = 0u, nul = 0u;");
    em.heap_sampler_locals(header);
    // A register-relative move picks its register at run time, so such a program
    // keeps every vector register in one table.
    let table = em.insns.iter().any(relative_move);
    let mut names = Vec::new();
    for register in &em.regs {
        if is_vector_register(register) {
            if !table {
                names.push(format!("{register}__[2] = {{0u, 0u}}"));
            }
        } else if register == "tb_fp_mode" {
            names.push(format!("{register} = 0xc0u"));
        } else {
            names.push(format!("{register} = 0u"));
        }
    }
    let _ = writeln!(header, "    uint {};", names.join(", "));
    if table {
        let _ = writeln!(header, "    uint tb_v[256][2] = (uint[256][2])0;");
        for n in 0..256 {
            let _ = writeln!(header, "#define v{n}__ tb_v[{n}]");
        }
    }
    // Full subgroups follow the thread index, so each run of 32 threads is one wave.
    let _ = writeln!(header, "    uint tb_pair = WaveGetLaneIndex(), tb_first = (gtid.x / 32u) * 64u;");
    // A program that takes derivatives in a group with rows has its threads two by two:
    // lanes 4k to 4k+3 are the block k, left to right and then down.
    let blocks = em.compute_derivatives && y > 1;
    for lane in 0..2 {
        let flat = format!("(tb_first + tb_pair + {}u)", 32 * lane);
        if blocks {
            let across = x / 2;
            let _ = writeln!(header, "    v0__[{lane}] = 2u * (({flat} / 4u) % {across}u) + ({flat} & 1u); v1__[{lane}] = 2u * (({flat} / 4u) / {across}u) + (({flat} >> 1u) & 1u); v2__[{lane}] = 0u;");
            continue;
        }
        let _ = writeln!(header, "    v0__[{lane}] = {flat} % {x}u; v1__[{lane}] = ({flat} / {x}u) % {y}u; v2__[{lane}] = {flat} / {}u;", x * y);
    }
    let g = group_register;
    let _ = writeln!(header, "    s{} = gid.x; s{} = gid.y; s{} = gid.z;", g, g + 1, g + 2);
}

/// `code` with each vector register replaced by the copy of it that lane `tb_h` holds.
fn lane_registers(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + code.len() / 4);
    for line in code.lines() {
        let mut at = 0;
        // `identifiers` yields each name with the rest of its line.
        for (word, after) in identifiers(line) {
            let end = line.len() - after.len();
            out.push_str(&line[at..end]);
            if is_vector_register(word) {
                out.push_str("__[tb_h]");
            }
            at = end;
        }
        out.push_str(&line[at..]);
        out.push('\n');
    }
    out
}

fn relative_move(ins: &Insn) -> bool {
    matches!(ins.mnemonic.as_str(), "v_movrels_b32" | "v_movreld_b32" | "v_movrelsd_b32")
}

/// A move whose source, destination or both are the named register plus M0.
fn move_relative(em: &mut Emitter<'_>, ins: &Insn) -> Result<(), String> {
    let (Some(Opnd::Vgpr(to)), Some(Opnd::Vgpr(from))) = (ins.dst.first(), ins.src.first()) else {
        return Err("paired wave64: register-relative move with a non-VGPR operand".into());
    };
    let register = |n: u8, relative: bool| if relative { format!("tb_v[({n}u + m0) & 255u]") } else { format!("v{n}__") };
    let source = register(*from, ins.mnemonic != "v_movreld_b32");
    let destination = register(*to, ins.mnemonic != "v_movrels_b32");
    em.line("{");
    // Both lanes are read before either is written.
    for lane in 0..2u32 {
        em.line(&format!("uint tbr_{lane} = {source}[{lane}];"));
    }
    for lane in 0..2u32 {
        let active = lane_bit("exec_lo", "exec_hi", lane);
        em.line(&format!("if ({active} != 0u) {destination}[{lane}] = tbr_{lane};"));
    }
    em.line("}");
    Ok(())
}

fn is_vector_register(name: &str) -> bool {
    name.len() > 1 && name.starts_with('v') && name[1..].bytes().all(|b| b.is_ascii_digit())
}

fn per_lane(encoding: Enc) -> bool {
    matches!(encoding, Enc::Vop1 | Enc::Vop2 | Enc::Vop3 | Enc::Vopc | Enc::Mubuf | Enc::Mtbuf
        | Enc::Mimg | Enc::Flat | Enc::Ds)
}

/// Identifiers in a piece of emitted code, ignoring comment lines.
fn identifiers(code: &str) -> impl Iterator<Item = (&str, &str)> {
    code.lines().filter(|line| !line.trim_start().starts_with("//")).flat_map(|line| {
        let mut found = Vec::new();
        let bytes = line.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at].is_ascii_alphabetic() || bytes[at] == b'_' {
                let start = at;
                while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                    at += 1;
                }
                // A member such as `.x` or a literal suffix is not a register.
                if start == 0 || (bytes[start - 1] != b'.' && !bytes[start - 1].is_ascii_digit()) {
                    found.push((&line[start..at], &line[at..]));
                }
            } else if bytes[at].is_ascii_digit() {
                while at < bytes.len() && bytes[at].is_ascii_alphanumeric() {
                    at += 1;
                }
            } else {
                at += 1;
            }
        }
        found
    })
}

/// The two words of the lane mask a vector instruction writes through `operand`.
fn mask_words(operand: &Opnd) -> Option<[String; 2]> {
    match operand {
        Opnd::Sgpr(n) => Some([format!("s{n}"), format!("s{}", n + 1)]),
        Opnd::VccLo => Some(["vcc_lo".into(), "vcc_hi".into()]),
        Opnd::ExecLo => Some(["exec_lo".into(), "exec_hi".into()]),
        _ => None,
    }
}

/// `true` in the bit of `word_lo`/`word_hi` belonging to console lane `lane` (an expression).
fn lane_bit(low: &str, high: &str, lane: u32) -> String {
    let word = if lane == 0 { low } else { high };
    format!("(({word} >> tb_pair) & 1u)")
}

fn first_lane(destination: &str) -> [String; 1] {
    [format!("uint tb_leader = exec_lo != 0u ? firstbitlow(exec_lo) : (exec_hi != 0u ? 32u + firstbitlow(exec_hi) : 0u); {destination}")]
}

/// Read vector register or scalar `source` of console lane `tb_leader` into `destination`.
fn read_lane(em: &mut Emitter<'_>, destination: &Opnd, source: &Opnd, leader: &str) -> Result<(), String> {
    // The destination field of a lane read numbers a scalar register.
    let destination = &match *destination {
        Opnd::Vgpr(n @ 0..=105) => Opnd::Sgpr(n),
        Opnd::Vgpr(106) => Opnd::VccLo,
        Opnd::Vgpr(107) => Opnd::VccHi,
        Opnd::Vgpr(124) => Opnd::M0,
        Opnd::Vgpr(126) => Opnd::ExecLo,
        Opnd::Vgpr(127) => Opnd::ExecHi,
        Opnd::Vgpr(n) => return Err(format!("paired wave64: lane read into scalar code {n}")),
        other => other,
    };
    em.touch(destination);
    em.touch(source);
    let to = name(destination)?;
    let value = match source {
        Opnd::Vgpr(n) => format!("{to} = tb_leader < 32u ? WaveReadLaneAt(v{n}__[0], tb_leader) : WaveReadLaneAt(v{n}__[1], tb_leader - 32u);"),
        other => format!("{to} = {};", r(other)?),
    };
    // A heap index read from a wave with no active lane becomes slot 0.
    let value = if leader.is_empty() && em.heap_index_reads.contains(&em.cur_idx) {
        format!("{value} if ((exec_lo | exec_hi) == 0u) {to} = 0u;")
    } else { value };
    let first = if leader.is_empty() { first_lane(&value)[0].clone() } else { format!("uint tb_leader = {leader}; {value}") };
    em.line(&format!("{{ {first} }}"));
    Ok(())
}

/// The temporary that stands for a source moved from another lane.
const MOVED: u16 = 60000;

/// Statements that set `{prefix}{lane}` to vector register `v{register}` as console lane
/// `partner` (an expression in `tb_from`) holds it. Both host reads run in every thread.
fn from_lane(register: u32, prefix: &str, lane: u32, partner: &str) -> [String; 3] {
    [
        format!("uint tb_from{lane} = {partner};"),
        format!("uint tb_low{lane} = WaveReadLaneAt(v{register}__[0], tb_from{lane} & 31u), tb_high{lane} = WaveReadLaneAt(v{register}__[1], tb_from{lane} & 31u);"),
        format!("uint {prefix}{lane} = tb_from{lane} < 32u ? tb_low{lane} : tb_high{lane};"),
    ]
}

/// Whether console lane `tb_from{lane}` is active.
fn source_active(lane: u32) -> String {
    format!("(((tb_from{lane} < 32u ? exec_lo : exec_hi) >> (tb_from{lane} & 31u)) & 1u) != 0u")
}

fn dpp8(em: &mut Emitter<'_>, ins: &Insn, fetch_inactive: bool) -> Result<Vec<String>, String> {
    if !matches!(ins.enc, Enc::Vop1 | Enc::Vop2 | Enc::Vopc) {
        return Err("paired wave64: DPP8 outside a short encoding".into());
    }
    let control = *ins.raw.get(1).ok_or("paired wave64: DPP8 without its control word")?;
    let (register, selectors) = (control & 0xff, control >> 8);
    em.regs.insert(format!("v{register}"));
    let mut lines = Vec::new();
    for lane in 0..2u32 {
        let me = format!("(tb_pair + {}u)", 32 * lane);
        let partner = format!("({me} & ~7u) | (({selectors:#x}u >> (3u * ({me} & 7u))) & 7u)");
        lines.extend(from_lane(register, "tbd_", lane, &partner));
        if !fetch_inactive {
            lines.push(format!("if (!({})) tbd_{lane} = 0u;", source_active(lane)));
        }
    }
    Ok(lines)
}

/// The sixteen-lane-row controls: which lane each lane reads, and which lanes write.
fn dpp16(em: &mut Emitter<'_>, ins: &Insn) -> Result<Vec<String>, String> {
    if !matches!(ins.enc, Enc::Vop1 | Enc::Vop2) {
        return Err("paired wave64: DPP16 outside a short arithmetic encoding".into());
    }
    let control = *ins.raw.get(1).ok_or("paired wave64: DPP16 without its control word")?;
    let (register, select) = (control & 0xff, (control >> 8) & 0x1ff);
    let (fetch_inactive, zero_invalid) = (control & (1 << 18) != 0, control & (1 << 19) != 0);
    let (banks, rows) = ((control >> 24) & 0xf, control >> 28);
    if control & 0x00f0_0000 != 0 {
        return Err("paired wave64: DPP16 source modifiers".into());
    }
    em.regs.insert(format!("v{register}"));
    let mut lines = Vec::new();
    for lane in 0..2u32 {
        let me = format!("(tb_pair + {}u)", 32 * lane);
        let count = select & 15;
        // (the lane read, whether it lies inside the row)
        let (partner, inside) = match select {
            0..=0xff => (format!("({me} & ~3u) | (({select:#x}u >> (2u * ({me} & 3u))) & 3u)"), "true".to_string()),
            0x101..=0x10f => (format!("({me} + {count}u) & 63u"), format!("({me} & 15u) + {count}u < 16u")),
            0x111..=0x11f => (format!("({me} - {count}u) & 63u"), format!("({me} & 15u) >= {count}u")),
            0x121..=0x12f => (format!("({me} & ~15u) | (({me} - {count}u) & 15u)"), "true".to_string()),
            0x140 => (format!("({me} & ~15u) | (15u - ({me} & 15u))"), "true".to_string()),
            0x141 => (format!("({me} & ~7u) | (7u - ({me} & 7u))"), "true".to_string()),
            0x150..=0x15f => (format!("({me} & ~15u) | {count}u"), "true".to_string()),
            0x160..=0x16f => (format!("{me} ^ {count}u"), "true".to_string()),
            _ => return Err(format!("paired wave64: DPP16 control {select:#x}")),
        };
        lines.extend(from_lane(register, "tbd_", lane, &partner));
        let valid = if fetch_inactive { format!("({inside})") } else { format!("({inside}) && {}", source_active(lane)) };
        lines.push(format!("bool tbo_{lane} = {valid};"));
        lines.push(format!("if (!tbo_{lane}) tbd_{lane} = 0u;"));
        let enabled = format!("((({rows}u >> ({me} >> 4)) & ({banks}u >> ({me} & 3u))) & 1u)");
        let writes = if zero_invalid { enabled } else { format!("({enabled} & (tbo_{lane} ? 1u : 0u))") };
        lines.push(format!("uint tbw_{lane} = {writes};"));
    }
    Ok(lines)
}

/// Rows of sixteen lanes exchange values, within a row or with the neighbouring one.
fn permlane(em: &mut Emitter<'_>, ins: &Insn) -> Result<(), String> {
    // OP_SEL[0] fetches from inactive lanes and OP_SEL[1] zeroes what an inactive lane
    // would have supplied. OP_SEL[3] is set by the console compiler and selects nothing
    // here; LLVM reads the same words as `op_sel:[1,0]`.
    let flags = (ins.raw[0] >> 11) & 0xf;
    if flags & 4 != 0 || ins.aux.mask != 0 || ins.aux.imm != 0 || ins.aux.imm2 != 0 {
        return Err("paired wave64: permlane modifiers".into());
    }
    let (fetch_inactive, zero_inactive) = (flags & 1 != 0, flags & 2 != 0);
    let (Some(Opnd::Vgpr(to)), Some(Opnd::Vgpr(from))) = (ins.dst.first(), ins.src.first()) else {
        return Err("paired wave64: permlane operands".into());
    };
    let (low, high) = (r(&ins.src[1])?, r(&ins.src[2])?);
    let across = if ins.mnemonic == "v_permlanex16_b32" { " ^ 16u" } else { "" };
    em.line("{");
    for lane in 0..2u32 {
        let me = format!("(tb_pair + {}u)", 32 * lane);
        let selector = format!("((({me} & 8u) != 0u ? {high} : {low}) >> (4u * ({me} & 7u))) & 15u");
        let partner = format!("(({me} & ~15u){across}) | ({selector})");
        for line in from_lane(u32::from(*from), "tbp_", lane, &partner) {
            em.line(&line);
        }
    }
    // Both lanes are read before either is written. An inactive source supplies its
    // value, zero, or nothing, in which case the destination keeps what it had.
    for lane in 0..2u32 {
        let active = lane_bit("exec_lo", "exec_hi", lane);
        let source = source_active(lane);
        em.line(&if fetch_inactive {
            format!("if ({active} != 0u) v{to}__[{lane}] = tbp_{lane};")
        } else if zero_inactive {
            format!("if ({active} != 0u) v{to}__[{lane}] = {source} ? tbp_{lane} : 0u;")
        } else {
            format!("if ({active} != 0u && {source}) v{to}__[{lane}] = tbp_{lane};")
        });
    }
    em.line("}");
    Ok(())
}

/// Permute lanes within each quad (QDMode) or within each 32-lane half-wave.
fn ds_swizzle(em: &mut Emitter<'_>, ins: &Insn) -> Result<(), String> {
    let mode = ins.aux.imm as u32;
    let quad = mode & 0x8000 != 0;
    if ins.raw[0] & 0x3_0000 != 0 || (quad && mode & 0xff00 != 0x8000) {
        return Err("unsupported compute DS swizzle encoding".into());
    }
    let (Some(Opnd::Vgpr(to)), Some(Opnd::Vgpr(from))) = (ins.dst.first(), ins.src.first()) else {
        return Err("paired wave64: ds_swizzle operands".into());
    };
    em.line("{");
    for lane in 0..2u32 {
        let me = format!("(tb_pair + {}u)", 32 * lane);
        let partner = if quad {
            format!("({me} & ~3u) | (({:#x}u >> (2u * ({me} & 3u))) & 3u)", mode & 255)
        } else {
            format!("({me} & ~31u) | ((({me} & {}u) | {}u) ^ {}u)", mode & 31, (mode >> 5) & 31, (mode >> 10) & 31)
        };
        for line in from_lane(u32::from(*from), "tbs_", lane, &partner) {
            em.line(&line);
        }
        em.line(&format!("if (!({})) tbs_{lane} = 0u;", source_active(lane)));
    }
    // Both lanes are read before either is written.
    for lane in 0..2u32 {
        let active = lane_bit("exec_lo", "exec_hi", lane);
        em.line(&format!("if ({active} != 0u) v{to}__[{lane}] = tbs_{lane};"));
    }
    em.line("}");
    Ok(())
}

pub(super) fn emit(em: &mut Emitter<'_>, idx: usize, ins: &Insn, is_ps: bool, br: Option<usize>) -> Result<(), String> {
    let m = ins.mnemonic.as_str();
    if !per_lane(ins.enc) {
        let start = em.body.len();
        em.emit_insn_body(idx, ins, is_ps, br)?;
        if identifiers(&em.body[start..]).any(|(word, _)| word.starts_with("Wave") || word.starts_with("Quad")) {
            return Err(format!("paired wave64: unhandled wave operation in scalar instruction {m}"));
        }
        return Ok(());
    }
    if em.bsel.contains_key(&idx) {
        // One guarded copy per descriptor the instruction may name, chosen by a scalar
        // every lane shares. Each copy comes back here and is expanded for its lanes.
        return em.emit_insn_body(idx, ins, is_ps, br);
    }
    em.line(&format!("// {:#06x} {m}", ins.offset));
    em.cur_idx = idx;
    if !matches!(m, "v_readfirstlane_b32" | "v_readlane_b32") {
        for operand in ins.src.iter().chain(&ins.dst) {
            em.touch(operand);
        }
    }
    let destination = ins.dst.first();
    match m {
        "v_readfirstlane_b32" => {
            return read_lane(em, destination.ok_or("no destination")?, &ins.src[0], "");
        }
        "v_readlane_b32" => {
            let lane = format!("({}) & 63u", r(&ins.src[1])?);
            return read_lane(em, destination.ok_or("no destination")?, &ins.src[0], &lane);
        }
        "v_writelane_b32" => {
            let Some(Opnd::Vgpr(n)) = destination else { return Err("paired wave64: writelane destination".into()) };
            let (value, lane) = (r(&ins.src[0])?, r(&ins.src[1])?);
            em.line(&format!("if ((({lane}) & 63u) == tb_pair) v{n}__[0] = {value};"));
            em.line(&format!("if ((({lane}) & 63u) == tb_pair + 32u) v{n}__[1] = {value};"));
            return Ok(());
        }
        "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" => {
            // The first source is a whole mask word, not this lane's bit.
            let Some(Opnd::Vgpr(n)) = destination else { return Err("paired wave64: mbcnt destination".into()) };
            let mask = r(&ins.src[0])?;
            for lane in 0..2u32 {
                let add = match &ins.src[1] {
                    Opnd::Vgpr(a) => format!("v{a}__[{lane}]"),
                    other => r(other)?,
                };
                let below = match (m.ends_with("lo_u32_b32"), lane) {
                    (true, 0) => "((1u << tb_pair) - 1u)",
                    (true, _) => "0xffffffffu",
                    (false, 0) => "0u",
                    (false, _) => "((1u << tb_pair) - 1u)",
                };
                let active = lane_bit("exec_lo", "exec_hi", lane);
                em.line(&format!("if ({active} != 0u) v{n}__[{lane}] = {add} + countbits(({mask}) & {below});"));
            }
            return Ok(());
        }
        _ => {}
    }
    if matches!(m, "v_permlane16_b32" | "v_permlanex16_b32") {
        return permlane(em, ins);
    }
    if m == "ds_swizzle_b32" {
        return ds_swizzle(em, ins);
    }
    if relative_move(ins) {
        return move_relative(em, ins);
    }
    // A DPP8 first source reads a neighbour within the lane's group of eight.
    let (mut moved, mut gated) = (None, false);
    match ins.src.first() {
        Some(&Opnd::Raw(code @ (233 | 234))) => moved = Some(dpp8(em, ins, code == 234)?),
        Some(&Opnd::Raw(250)) => (moved, gated) = (Some(dpp16(em, ins)?), true),
        _ => {}
    }
    let mut ins = ins.clone();
    if moved.is_some() {
        ins.src[0] = Opnd::Temp(MOVED);
    }
    let ins = &ins;
    if m.contains("permlane") || m.contains("swizzle") || m.starts_with("tb_") || m.starts_with("ds_append")
        || m.starts_with("ds_consume") || m.starts_with("ds_ordered") {
        return Err(format!("paired wave64: cross-lane instruction {m}"));
    }

    let start = em.body.len();
    em.emit_insn_body(idx, ins, is_ps, br)?;
    // The instruction is named once, above; each lane's copy carries code only.
    let code: String = em.body.split_off(start).lines()
        .filter(|line| !line.trim_start().starts_with("//")).flat_map(|line| [line, "\n"]).collect();
    if let Some((word, _)) = identifiers(&code).find(|(word, _)| {
        word.starts_with("Wave") || word.starts_with("Quad") || word.starts_with("tb_lane") || word.starts_with("tb_swizzle")
    }) {
        return Err(format!("paired wave64: {m} needs {word}"));
    }
    // RDNA2's `v_cmpx` writes EXEC only. The decoder still names VCC as the
    // compare's destination, and rebuilding it would clear the VCC bits of
    // every lane the compare left out.
    let mut written: Vec<[String; 2]> = if m.starts_with("v_cmpx_") { Vec::new() }
        else { ins.dst.iter().filter_map(mask_words).collect() };
    // `v_cmpx` and the two-operand carry forms name their mask destination only in the opcode.
    let implied = if m.starts_with("v_cmpx_") { Some(Opnd::ExecLo) }
        else if ins.enc == Enc::Vop2 && (m.contains("_co_") || m.ends_with("_co_u32")) { Some(Opnd::VccLo) }
        else { None };
    // Three-operand carry and 64-bit multiply-add keep their mask destination in the first word.
    let carries = m.contains("_co_") || m.ends_with("_co_u32") || matches!(m, "v_mad_u64_u32" | "v_mad_i64_i32");
    let scalar = if ins.enc == Enc::Vop3 && carries {
        match (ins.raw[0] >> 8) & 0x7f {
            c @ 0..=105 => Some(Opnd::Sgpr(c as u8)),
            106 => Some(Opnd::VccLo),
            _ => None,
        }
    } else { None };
    if let Some(words) = scalar.as_ref().and_then(mask_words) {
        if !written.contains(&words) {
            written.push(words);
        }
    }
    if let Some(words) = implied.as_ref().and_then(mask_words) {
        if !written.contains(&words) {
            written.push(words);
        }
    }
    let mut read: Vec<[String; 2]> = vec![["vcc_lo".into(), "vcc_hi".into()], ["exec_lo".into(), "exec_hi".into()]];
    if m.starts_with("v_cndmask") || m.contains("_co_ci_") {
        if let Some(words) = ins.src.get(2).and_then(mask_words) {
            if !read.contains(&words) {
                read.push(words);
            }
        }
    }
    // Every scalar the code assigns must be a mask this instruction is known to write.
    for (word, after) in identifiers(&code) {
        let scalar = word == "vcc_lo" || word == "vcc_hi" || word == "exec_lo" || word == "exec_hi"
            || (word.len() > 1 && word.starts_with('s') && word[1..].bytes().all(|b| b.is_ascii_digit()));
        let after = after.trim_start();
        if scalar && after.starts_with('=') && !after.starts_with("==") && !written.iter().any(|w| w.iter().any(|n| n == word)) {
            return Err(format!("paired wave64: {m} writes {word} outside a known mask"));
        }
    }
    let used: BTreeSet<&str> = identifiers(&code).map(|(word, _)| word).collect();
    if used.is_empty() && written.is_empty() {
        return Ok(());
    }
    // A lane needs its bit of a mask only if the code names the mask.
    let mut shadowed: Vec<[String; 2]> = read.iter().filter(|w| w.iter().any(|n| used.contains(n.as_str()))).cloned().collect();
    for words in &written {
        if !shadowed.contains(words) {
            shadowed.push(words.clone());
        }
    }
    let exec = ["exec_lo".to_string(), "exec_hi".to_string()];
    em.line("{");
    if let Some(reads) = &moved {
        for line in reads {
            em.line(line);
        }
    }
    for words @ [low, high] in &shadowed {
        // A mask the instruction only writes starts clear and needs no copy.
        if read.contains(words) {
            em.line(&format!("uint tbx_{low}[2] = {{{low}, {high}}};"));
        }
    }
    if !written.is_empty() && !shadowed.contains(&exec) {
        // Rebuilt masks keep to the lanes that were active.
        em.line("uint tbx_exec_lo[2] = {exec_lo, exec_hi};");
    }
    for [low, _] in &written {
        em.line(&format!("bool tbm_{low}[2] = {{false, false}};"));
    }
    // The code is written once and expanded for each lane the thread holds, on that
    // lane's registers. A loop over the lanes costs DXC far more than two copies do.
    let mut lane = String::new();
    for words @ [low, high] in &shadowed {
        // A mask the instruction only writes starts clear in every lane.
        let bit = format!("(tbx_{low}[tb_h] >> tb_pair) & 1u");
        let value = if gated && low == "exec_lo" {
            format!("{bit} & (tb_h == 0u ? tbw_0 : tbw_1)")
        } else if read.contains(words) {
            bit
        } else {
            "0u".into()
        };
        // The upper word of a lane's own mask is zero; declare it only for code that names it.
        if written.contains(words) || used.contains(high.as_str()) {
            let _ = writeln!(lane, "    uint {low} = {value}, {high} = 0u;");
        } else {
            let _ = writeln!(lane, "    uint {low} = {value};");
        }
    }
    if moved.is_some() {
        let _ = writeln!(lane, "    uint {} = tb_h == 0u ? tbd_0 : tbd_1;", name(&Opnd::Temp(MOVED))?);
    }
    for vector in used.iter().filter(|w| is_vector_register(w)) {
        em.regs.insert((*vector).to_string());
    }
    lane.push_str(&lane_registers(&code));
    for [low, _] in &written {
        let _ = writeln!(lane, "    tbm_{low}[tb_h] = ({low} & 1u) != 0u;");
    }
    if lane.lines().any(|line| line.trim_start().starts_with('#')) {
        // A directive cannot sit inside a macro.
        for index in 0..2 {
            em.line(&format!("{{ const uint tb_h = {index}u;"));
            em.body.push_str(&lane);
            em.line("}");
        }
    } else {
        // A name of its own for each copy, so none needs undefining. An instruction can
        // be written more than once, for each descriptor it may name.
        let idx = em.tmp;
        em.tmp += 1;
        em.line(&format!("#define TB_L{idx}(tb_h) {{ \\"));
        for line in lane.lines() {
            em.body.push_str(line);
            em.body.push_str(" \\\n");
        }
        em.line("}");
        em.line(&format!("TB_L{idx}(0u) TB_L{idx}(1u)"));
    }
    for [low, high] in &written {
        // Inactive lanes write zero. A compare into EXEC already includes the old mask.
        let (a, b) = (format!("WaveActiveBallot(tbm_{low}[0]).x"), format!("WaveActiveBallot(tbm_{low}[1]).x"));
        em.line(&format!("{low} = {a} & tbx_exec_lo[0]; {high} = {b} & tbx_exec_lo[1];"));
    }
    em.line("}");
    Ok(())
}
