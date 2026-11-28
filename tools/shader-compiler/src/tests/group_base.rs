use super::*;

// The entry the compute emitter writes (`emitter/output.rs`).
const EMITTED: &str = "RWByteAddressBuffer u0 : register(u0);
[numthreads(64, 1, 1)]
void main(uint3 gid : SV_GroupID, uint3 gtid : SV_GroupThreadID) {
    uint s5 = 0u, s6 = 0u, s7 = 0u;
    s5 = gid.x; s6 = gid.y; s7 = gid.z;
}
";

#[test]
fn the_group_id_becomes_the_console_value_before_the_body() {
    let out = offset(EMITTED).unwrap();
    assert!(out.starts_with("cbuffer TBGroupBase : register(b0, space32766) { uint3 tb_group_base; };\n"), "{out}");
    assert!(out.contains("void main(uint3 tb_launch_gid : SV_GroupID, uint3 gtid : SV_GroupThreadID) {\n    uint3 gid = tb_launch_gid + tb_group_base;\n    uint s5"), "{out}");
    // The body is unchanged and reads the corrected name.
    assert!(out.contains("    s5 = gid.x; s6 = gid.y; s7 = gid.z;\n}\n"), "{out}");
    assert_eq!(semantic_uses(&out, "SV_GroupID"), 1);
}

#[test]
fn a_dispatch_thread_id_moves_by_the_base_times_the_group_size() {
    // A hand-written kernel's entry, as in `compute_hlsl`.
    let kernel = "[numthreads(8, 4, 2)]\nvoid main(uint lane : SV_GroupIndex, uint3 tid : SV_DispatchThreadID, uint3 group : SV_GroupID)\n{\n    store(tid, group);\n}\n";
    let out = offset(kernel).unwrap();
    assert!(out.contains("void main(uint lane : SV_GroupIndex, uint3 tb_launch_tid : SV_DispatchThreadID, uint3 tb_launch_group : SV_GroupID)\n{\n    uint3 tid = tb_launch_tid + tb_group_base * uint3(8u, 4u, 2u);\n    uint3 group = tb_launch_group + tb_group_base;\n    store(tid, group);"), "{out}");
    // Unspaced parameters, as `persistent.rs` writes them.
    let out = offset("[numthreads(64,1,1)]\nvoid main(uint3 gid:SV_GroupID, uint groupIndex:SV_GroupIndex) {\n}\n").unwrap();
    assert!(out.contains("void main(uint3 tb_launch_gid : SV_GroupID, uint groupIndex:SV_GroupIndex) {\n    uint3 gid = tb_launch_gid + tb_group_base;\n}"), "{out}");
}

#[test]
fn a_program_without_group_ids_is_unchanged_apart_from_the_constants() {
    let program = "[numthreads(64, 1, 1)]\nvoid main(uint3 gtid : SV_GroupThreadID) {\n}\n";
    let out = offset(program).unwrap();
    assert_eq!(out.lines().skip(1).collect::<Vec<_>>(), program.lines().collect::<Vec<_>>());
}

#[test]
fn refuses_a_group_id_it_cannot_correct() {
    let refused = [
        // A scalar or vector the base cannot be added to as a whole.
        "[numthreads(64, 1, 1)]\nvoid main(uint gid : SV_GroupID) {\n}\n",
        // The ID read through a structure.
        "struct In { uint3 gid : SV_GroupID; };\n[numthreads(64, 1, 1)]\nvoid main(In input) {\n}\n",
        // A thread ID with no literal group size.
        "[numthreads(X, 1, 1)]\nvoid main(uint3 tid : SV_DispatchThreadID) {\n}\n",
        "void main(uint3 tid : SV_DispatchThreadID) {\n}\n",
        // Two entries, or none.
        "[numthreads(1, 1, 1)]\nvoid main(uint3 gid : SV_GroupID) {\n}\nvoid main(uint3 g : SV_GroupID) {\n}\n",
        "[numthreads(1, 1, 1)]\nvoid entry(uint3 gid : SV_GroupID) {\n}\n",
        // A name of its own already in use.
        "uint tb_group_base;\n[numthreads(1, 1, 1)]\nvoid main(uint3 gid : SV_GroupID) {\n}\n",
    ];
    for program in refused {
        assert!(offset(program).is_err(), "{program}");
    }
    // Letter case does not hide a semantic.
    assert!(offset("struct In { uint3 g : sv_groupid; };\n[numthreads(1, 1, 1)]\nvoid main(In i) {\n}\n").is_err());
}

#[test]
fn takes_the_group_base_only_from_a_compute_root() {
    use tuxbox_dxbc::group_base::{REGISTER, SPACE, WORDS};
    use tuxbox_dxbc::rootsig::{RootParam, RootParamKind, RootSig};
    let base = RootParam { visibility: 0, kind: RootParamKind::Constants { register: REGISTER, space: SPACE, num32: WORDS } };
    let root = RootSig { version: 2, flags: 0, params: vec![base], num_static_samplers: 0, static_samplers: Vec::new() };
    let mut shader = compute_extract();
    shader.root_sig = Some(root.clone());
    assert!(take(&mut shader));
    assert!(shader.root_sig.as_ref().unwrap().params.is_empty());
    shader.stage = Stage::Pixel;
    shader.root_sig = Some(root);
    assert!(!take(&mut shader));
    assert_eq!(shader.root_sig.unwrap().params.len(), 1);
}

fn compute_extract() -> ShaderExtract {
    ShaderExtract {
        stage: Stage::Compute,
        shader_model: (6, 0),
        required_features: None,
        bitcode_stripped: true,
        fspx_config: None,
        ps09_pixel_inputs: None,
        fssm: Vec::new(),
        entry: Some("main".into()),
        isa_data: Vec::new(),
        isa: Vec::new(),
        isa_offset_in_fssm: 0,
        psv_threads: None,
        flat_inputs: Vec::new(),
        flat_outputs: Vec::new(),
        isa_preamble: [0, 64, 1, 1],
        has_input_sig: false,
        has_output_sig: false,
        has_psv0: false,
        has_root_sig: true,
        input_sig: Vec::new(),
        output_sig: Vec::new(),
        resources: Vec::new(),
        gs_info: None,
        mesh_info: None,
        primitive_sig: Vec::new(),
        root_sig: None,
    }
}
