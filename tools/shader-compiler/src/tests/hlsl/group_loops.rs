use super::*;
use crate::{hlsl::translate, meta::{RootSig, SigElement}, psv::MeshInfo, ShaderExtract, Stage};

#[test]
fn a_loop_is_grouped_when_it_or_a_loop_inside_it_holds_a_rendezvous() {
    let loops = [(2, 9), (4, 6), (12, 15)];
    assert_eq!(select(&loops, |i| i == 5), BTreeSet::from([6, 9]));
    assert_eq!(select(&loops, |i| i == 8), BTreeSet::from([9]));
    assert!(select(&loops, |i| i == 11).is_empty());
}

#[test]
fn a_hoisted_vote_is_false_where_the_guard_skips() {
    assert_eq!(guarded(&[], "v"), "v");
    assert_eq!(guarded(&[SKIP_ALL, 7, 9], "v"), "(tb_skip < 7u) && v");
}

/// The `loop-67` program of tools/check_translated_mesh_gpu.py, assembled with
/// llvm-mc for gfx1030 wave64. Its guest waves run the loop at word 37 once
/// and nine times; the loop leaves by `s_andn2_b64` and the `s_cbranch_scc1`
/// that is instruction 27.
const LOOP_67: [u32; 113] = [
    0xd7650000, 0x000100c1, 0x7e000505, 0x9381ff03, 0x00040018, 0xd7660000, 0x000200c1,
    0x900a8700, 0xd7460001, 0x04010c01, 0xbe9403ff, 0x00000043, 0x7e2802ff, 0x80000000,
    0x7e3602ff, 0x04210440, 0x7d840280, 0x02283714, 0x7d8402ff, 0x00000042, 0xd5010015,
    0x01a9fef3, 0x40400000, 0x7d8402ff, 0x00000041, 0xd5010016, 0x01a9fef3, 0x40400000,
    0x7e2e02f0, 0x7e3002f2, 0x7d8202ff, 0x000003ff, 0xbf870001, 0xbf800000, 0x8f2e8301,
    0xbeaf0380, 0xbeac047e, 0x802f812f, 0xbf082e2f, 0x85ea807e, 0x8aac6a2c, 0xbf85fffb,
    0x80b02e2f, 0xbf068130, 0x8531f480, 0x062a2a31, 0xbf068014, 0x8580807e, 0xb5940080,
    0x8582807e, 0x88800000, 0x88820202, 0x88800002, 0xd501000a, 0x00010280, 0x7e00050a,
    0x87000081, 0xbf068100, 0xbe80047e, 0x8582807e, 0xd501000b, 0x00090214, 0x8984c102,
    0x7e0c050b, 0x7da40280, 0xbf88000d, 0x8f078c06, 0x8aea027e, 0x88060607, 0xbefc0306,
    0xbf900009, 0xbf8c0000, 0xbf870006, 0xbf8a0000, 0x7e147081, 0xf80008c0, 0x00000000,
    0xf8000941, 0x0000000a, 0xbf8cff0f, 0xbefe0400, 0xbf8a0000, 0x7d880214, 0x8780046a,
    0xbe862400, 0xbf880012, 0xd548000c, 0x02291514, 0xd548000d, 0x02292914, 0x361c28ff,
    0x000003ff, 0x7d861814, 0xd4c30002, 0x00021a14, 0xd4c30000, 0x00021c14, 0x8882026a,
    0x88800200, 0xd501000f, 0x0001ff14, 0x80000000, 0xf8000941, 0x0000000f, 0xbf8cff0f,
    0xbefe0406, 0x7d880214, 0x8780046a, 0xbe802400, 0xbf880002, 0xf80008cf, 0x18171615,
    0xbf810000,
];
const ANDN2: usize = 40; // s_andn2_b64 s[44:45], s[44:45], vcc

/// A 128-vertex mesh program with a position output and an empty root signature,
/// in the FSSM image examples/lanes/raster_fixture.py builds.
fn mesh(words: &[u32]) -> ShaderExtract {
    let mut image = vec![0u8; 0x280];
    image[..8].copy_from_slice(&6u64.to_le_bytes());
    for n in 0..12 {
        let descriptor: u64 = if n == 7 { 0x68 } else { 0xdead_beef_dead_beef };
        image[8 + 8 * n..16 + 8 * n].copy_from_slice(&descriptor.to_le_bytes());
    }
    image[0x68..0x70].copy_from_slice(&0x280u64.to_le_bytes());
    image[0x70..0x78].copy_from_slice(&(4 * words.len() as u64).to_le_bytes());
    image.extend(words.iter().flat_map(|w| w.to_le_bytes()));
    let mut fssm = b"FSSM".to_vec();
    fssm.extend(0x101u32.to_le_bytes());
    fssm.extend((image.len() as u64).to_le_bytes());
    fssm.extend(image);
    let program = tuxbox_dxbc::fssm::decode(&fssm).unwrap();
    let position = SigElement { name: "SV_Position".into(), index: 0, sysval: 1, comp_type: 3, register: 0, mask: 15, rw_mask: 0 };
    ShaderExtract {
        stage: Stage::Mesh, shader_model: (6, 5), required_features: None, entry: program.entry,
        isa: program.isa, isa_data: program.isa_data, isa_offset_in_fssm: program.isa_offset,
        isa_preamble: program.preamble, fssm, bitcode_stripped: true, fspx_config: None,
        ps09_pixel_inputs: None, flat_inputs: Vec::new(), flat_outputs: Vec::new(),
        psv_threads: Some([128, 1, 1]), input_sig: Vec::new(), output_sig: vec![position],
        primitive_sig: Vec::new(), has_input_sig: false, has_output_sig: true, has_root_sig: true,
        has_psv0: true, resources: Vec::new(), gs_info: None,
        mesh_info: Some(MeshInfo { vertices: 128, primitives: 128, topology: 2, threads: [128, 1, 1], payload: 0, shared_bytes: 0 }),
        root_sig: Some(RootSig { version: 1, flags: 0, params: Vec::new(), num_static_samplers: 0, static_samplers: Vec::new() }),
    }
}

/// Each call in `evaluate` that meets the whole group, with the headers of the
/// blocks around it.
fn rendezvous(hlsl: &str) -> Vec<(String, Vec<String>)> {
    let body = &hlsl[hlsl.find("MeshLane evaluate(").unwrap()..];
    let body = &body[..body.find("\n}\n").unwrap()];
    let mut blocks: Vec<String> = Vec::new();
    let mut found = Vec::new();
    for line in body.lines().skip(1) {
        let call = ["tb_page_ballot(", "tb_page_read(", "tb_group_any("].iter().filter_map(|c| line.find(c)).min();
        for (at, c) in line.char_indices() {
            if call == Some(at) {
                found.push((line.trim().to_string(), blocks.clone()));
            }
            match c {
                '{' => blocks.push(line.trim().to_string()),
                '}' => { blocks.pop(); }
                _ => {}
            }
        }
    }
    found
}

#[test]
fn a_mesh_loop_that_votes_over_its_wave_iterates_as_a_group() {
    let hlsl = translate(&mesh(&LOOP_67)).unwrap().hlsl;
    // The loop's exit vote is taken outside the guest branch guard, and a thread
    // the guard skips votes false, as when it did not call.
    assert!(hlsl.contains("uint2 mask_scc27 = tb_page_ballot(tb_wave_tid, (tb_skip < 4294967295u) && (s44 & 1u) != 0u);"), "{hlsl}");
    assert!(hlsl.contains("scc = any(mask_scc27);"));
    // Every thread stays in the loop until no thread of the group wants another
    // iteration, with a count that is the same in all of them.
    assert!(hlsl.contains("} while (tb_group_any(tb_live27) && ++tb_round27 < 65536u);"));
    assert!(hlsl.contains("if (tb_live27 && !(tb_c27)) { tb_live27 = false; tb_rest27 = tb_skip; tb_skip = 4294967295u; }"));
    assert!(hlsl.contains("if (!tb_live27) tb_skip = tb_rest27;"));
    assert!(hlsl.contains("bool tb_group_any(bool vote)"));
    // No barrier is inside a guest branch guard, and every loop around one votes as a group.
    let calls = rendezvous(&hlsl);
    assert!(calls.len() >= 3, "{calls:?}");
    for (call, blocks) in &calls {
        assert!(blocks.iter().all(|b| b == "do {"), "{call} inside {blocks:?}");
    }
    assert_eq!(hlsl.matches("do {").count(), hlsl.matches("} while (tb_group_any(").count());
}

#[test]
fn a_mesh_loop_without_a_rendezvous_keeps_each_lane_s_own_exit() {
    // s_cmp_le_u32 s47, s46: the same trip counts from a scalar comparison, with no vote.
    let mut words = LOOP_67;
    assert_eq!(words[ANDN2], 0x8aac6a2c);
    words[ANDN2] = 0xbf0b2e2f;
    let hlsl = translate(&mesh(&words)).unwrap().hlsl;
    assert!(!hlsl.contains("tb_group_any"));
    // Leaving early skips no barrier, so the loop has the iteration guard.
    assert!(hlsl.contains("} while (tb_c27 && ++tb_guard < 65536u);"), "{hlsl}");
}
