//! What each host profile receives for a fixed set of synthetic programs.
//!
//! A change to the translator shows here which hosts it affects. When the change
//! is meant, update the record and include it in the pull request:
//! `TUXBOX_BLESS_PROFILES=1 cargo test ... host_profiles`.
use super::*;

#[path = "host_profile_programs.rs"]
mod programs;

const RECORD: &str = include_str!("host_profiles.txt");

fn program(threads: u32, words: &[u32]) -> ShaderExtract {
    let isa: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    ShaderExtract {
        flat_inputs: Vec::new(), flat_outputs: Vec::new(), stage: Stage::Compute, shader_model: (6, 0),
        required_features: None, bitcode_stripped: true, fspx_config: None, ps09_pixel_inputs: None,
        fssm: Vec::new(), entry: Some("main".into()), isa_data: isa.clone(), isa, isa_offset_in_fssm: 0,
        psv_threads: None, isa_preamble: [0, threads, 1, 1], has_input_sig: false, has_output_sig: false,
        has_psv0: false, has_root_sig: false, input_sig: Vec::new(), output_sig: Vec::new(), resources: Vec::new(),
        gs_info: None, mesh_info: None, primitive_sig: Vec::new(), root_sig: None,
    }
}

/// FNV-1a, enough to notice that a translation changed.
fn summary(translated: &Result<Translated, String>) -> String {
    match translated {
        Ok(t) => {
            let hash = t.hlsl.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            });
            let waves = t.required_wave_size.map_or("any".into(), |width| width.to_string());
            format!("{} waves={waves} {hash:016x}", t.profile)
        }
        Err(_) => "refused".into(),
    }
}

#[test]
fn every_host_profile_receives_the_recorded_translation() {
    let mut now = String::new();
    let mut narrowed = 0;
    for &(name, threads, words) in programs::PROGRAMS {
        let shader = program(threads, words);
        let ordinary = translate(&shader);
        let narrow = translate_for_wave32_host(&shader);
        // Only a program that needs 64-lane waves may differ on a narrower host.
        if matches!(&ordinary, Ok(t) if t.required_wave_size == Some(64)) {
            narrowed += 1;
        } else {
            assert_eq!(summary(&narrow), summary(&ordinary), "{name}");
        }
        writeln!(now, "{name} any-host {}", summary(&ordinary)).unwrap();
        writeln!(now, "{name} wave32-host {}", summary(&narrow)).unwrap();
        writeln!(now, "{name} two-lanes {}", summary(&translate_paired_wave64(&shader))).unwrap();
    }
    assert!(narrowed > 0, "no program exercises the narrow-wave path");
    if std::env::var_os("TUXBOX_BLESS_PROFILES").is_some() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tests/hlsl/host_profiles.txt");
        std::fs::write(path, &now).unwrap();
        return;
    }
    let recorded = RECORD.replace("\r\n", "\n");
    let changed: Vec<&str> = now.lines().filter(|line| !recorded.lines().any(|old| old == *line)).collect();
    assert!(
        now == recorded,
        "translations differ for these programs and host profiles:\n{}\nIf this is intended, rerun with \
         TUXBOX_BLESS_PROFILES=1 and commit host_profiles.txt.",
        changed.join("\n")
    );
}

/// RDNA2 reads 0 past a group's LDS allocation and drops the writes. An HLSL
/// groupshared index past the array is undefined and can fault on NVIDIA, so
/// every access must be checked against the declared size.
#[test]
fn group_shared_accesses_are_checked_against_the_declared_size() {
    let &(_, threads, words) = programs::PROGRAMS.iter().find(|program| program.0 == "shared").unwrap();
    let shader = program(threads, words);
    let mut checked = 0;
    for translated in [translate(&shader), translate_for_wave32_host(&shader), translate_paired_wave64(&shader)] {
        let Ok(translated) = translated else { continue };
        let hlsl = &translated.hlsl;
        assert!(hlsl.contains("#define TB_LDS_WORDS "), "{hlsl}");
        assert!(!hlsl.contains("8191u"), "an access still wraps instead of being checked:\n{hlsl}");
        for line in hlsl.lines().filter(|line| line.contains("tb_lds[")) {
            assert!(line.contains("TB_LDS_WORDS"), "unchecked group shared access: {line}");
        }
        checked += 1;
    }
    assert!(checked > 0, "no host profile translated the shared-memory program");
}

/// A group of 32 threads fills lanes 0..32 of one console wave. A sum over the
/// whole wave reads the lanes that never start, so each host thread keeps one.
#[test]
fn a_group_of_32_threads_runs_as_the_lower_half_of_a_console_wave() {
    let &(_, _, words) = programs::PROGRAMS.iter().find(|program| program.0 == "lane-sum-half-wave").unwrap();
    for translated in [translate(&program(32, words)), translate_for_wave32_host(&program(32, words))] {
        let translated = translated.unwrap();
        assert_eq!(translated.required_wave_size, Some(32));
        let hlsl = &translated.hlsl;
        assert!(hlsl.contains("[WaveSize(32)]\n[numthreads(32, 1, 1)]"), "{hlsl}");
        // The upper lanes start outside EXEC and are read as lanes of their own.
        assert!(hlsl.contains("exec_lo = 0xffffffffu, exec_hi = 0u,"), "{hlsl}");
        assert!(hlsl.contains("WaveReadLaneAt(v2__[1], tb_leader - 32u)"), "{hlsl}");
    }
    // Other incomplete waves have no host wave that holds exactly their lanes.
    for threads in [16, 48, 96] {
        assert_eq!(translate(&program(threads, words)).map(|t| t.hlsl).unwrap_err(),
            "wave operation requires complete 64-lane compute waves");
    }
    // A whole wave keeps its two-lane form: every lane starts active.
    let whole = translate_paired_wave64(&program(64, words)).unwrap().hlsl;
    assert!(whole.contains("[numthreads(32, 1, 1)]") && whole.contains("exec_hi = 0xffffffffu,"), "{whole}");
}

/// `v_permlanex16_b32 v4, v3, -1, -1` with rows 1 and 3 active, as a prefix sum
/// over a wave hands each row's total to the next:
/// s_or_saveexec_b64 s[10:11], -1; s_mov_b32 exec_lo, 0xffff0000; s_mov_b32 exec_hi, 0xffff0000;
/// the permute; s_mov_b64 exec, s[10:11].
fn row_permute(op_sel: u32) -> ShaderExtract {
    program(64, &[0xbe8a25c1, 0xbefe03ff, 0xffff0000, 0xbeff03ff, 0xffff0000, 0xd7780004 | op_sel << 11, 0x03058303,
        0xbefe040a, 0xbf810000])
}

#[test]
fn a_row_permute_treats_an_inactive_source_as_its_modifiers_say() {
    let hlsl = |op_sel| translate_paired_wave64(&row_permute(op_sel)).map(|t| t.hlsl);
    // Neither modifier: the destination keeps its value.
    assert!(hlsl(0).unwrap().contains(" && (((tb_from0 < 32u ? exec_lo : exec_hi) >> (tb_from0 & 31u)) & 1u) != 0u) v4__[0] = tbp_0;"));
    // Fetch inactive, with and without the bit the console compiler also sets.
    for op_sel in [1, 9] {
        let hlsl = hlsl(op_sel).unwrap();
        assert!(hlsl.contains("if (((exec_lo >> tb_pair) & 1u) != 0u) v4__[0] = tbp_0;"), "{hlsl}");
        assert!(hlsl.contains("if (((exec_hi >> tb_pair) & 1u) != 0u) v4__[1] = tbp_1;"), "{hlsl}");
    }
    // Bound control: an inactive source supplies zero.
    assert!(hlsl(2).unwrap().contains(" != 0u ? tbp_1 : 0u;"));
    // Fetch inactive wins over bound control.
    assert!(hlsl(3).unwrap().contains("if (((exec_lo >> tb_pair) & 1u) != 0u) v4__[0] = tbp_0;"));
    assert_eq!(hlsl(4).unwrap_err(), "paired wave64: permlane modifiers [v_permlanex16_b32 @ 0x14]");
}

/// s_mov_b32 m0, s20; v_movrels_b32 v10, v20; v_movreld_b32 v24, v0; v_movrelsd_b32 v30, v20.
#[test]
fn register_relative_moves_index_one_table_of_lane_pairs() {
    let hlsl = translate_paired_wave64(&program(64, &[0xbefc0314, 0x7e148714, 0x7e308500, 0x7e3c8914, 0xbf810000])).unwrap().hlsl;
    assert!(hlsl.contains("uint tb_v[256][2] = (uint[256][2])0;") && hlsl.contains("#define v10__ tb_v[10]"), "{hlsl}");
    // Source plus M0, destination plus M0, and both.
    assert!(hlsl.contains("uint tbr_1 = tb_v[(20u + m0) & 255u][1];"), "{hlsl}");
    assert!(hlsl.contains("if (((exec_lo >> tb_pair) & 1u) != 0u) v10__[0] = tbr_0;"), "{hlsl}");
    assert!(hlsl.contains("uint tbr_0 = v0__[0];"), "{hlsl}");
    assert!(hlsl.contains("if (((exec_hi >> tb_pair) & 1u) != 0u) tb_v[(24u + m0) & 255u][1] = tbr_1;"), "{hlsl}");
    assert!(hlsl.contains("tb_v[(30u + m0) & 255u][0] = tbr_0;"), "{hlsl}");
    // A program without such a move keeps separate registers.
    let plain = translate_paired_wave64(&program(64, &[0x7e140300, 0xbf810000])).unwrap().hlsl;
    assert!(!plain.contains("tb_v"), "{plain}");
}
