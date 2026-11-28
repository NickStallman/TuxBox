use super::*;
use crate::psv::PsvResource;

/// FSSM with user-data rows and used-resource rows.
fn fssm(user: &[[u32; 5]], used: &[[u32; 5]]) -> Vec<u8> {
    let mut program = vec![0u8; 0x110];
    program[..8].copy_from_slice(b"FSSM\x01\x01\0\0");
    for (rows, field, count) in [(user, 0xe8, 0xf8), (used, 0xf0, 0xfc)] {
        let offset = (program.len() - 16) as u64;
        program[field..field + 8].copy_from_slice(&offset.to_le_bytes());
        program[count..count + 4].copy_from_slice(&(rows.len() as u32).to_le_bytes());
        program.extend(rows.iter().flatten().copied().flat_map(u32::to_le_bytes));
    }
    let size = (program.len() - 16) as u32;
    program[8..12].copy_from_slice(&size.to_le_bytes());
    program
}

const CONSTANTS: [u32; 5] = [0, 0, 2, 500, 0x30a]; // two root constants in s10:s11
const RESOURCE_HEAP: [u32; 5] = [5, 0, 1, 0, 0x0e];
const SAMPLER_HEAP: [u32; 5] = [6, 0, 1, 0, 0x10];
const USED: [u32; 5] = [2, 0, 500, 0, 0x30a04];

/// A root-less pixel program with root constants and both heaps.
fn pixel(words: &[u32]) -> ShaderExtract {
    let isa: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut ex = super::super::mesh::heap_extract(&[], 0, &isa);
    ex.stage = Stage::Pixel;
    ex.shader_model = (6, 6);
    ex.required_features = Some(3 << 25);
    ex.root_sig = None;
    ex.has_root_sig = false;
    ex.fssm = fssm(&[CONSTANTS, RESOURCE_HEAP, SAMPLER_HEAP], &[USED]);
    ex.resources = vec![PsvResource { res_type: 2, space: 0, lower: 500, upper: 500 }];
    ex
}

const SAMPLE: [u32; 9] = [
    0x8f00850a, // s0 = s10 << 5
    0x8f01840b, // s1 = s11 << 4
    0xf40c0107, 0x00000000, // s[4:11] = heap[s14:15][s0]
    0xf4080508, 0x02000000, // s[20:23] = samplers[s16:17][s1]
    0xf0800108, 0x00a10704, // v7 = image_sample(v[4:5], s[4:11], s[20:23])
    0xbf810000,
];

fn refusal(ex: &ShaderExtract) -> String {
    match super::super::translate(ex) {
        Ok(_) => panic!("translated"),
        Err(reason) => reason,
    }
}

#[test]
fn pixel_heap_indices_come_from_root_constants() {
    let out = super::super::translate(&pixel(&SAMPLE)).unwrap();
    assert_eq!(out.profile, "ps_6_6");
    for line in [
        "cbuffer TBIndex500_0 : register(b500, space0) { uint2 tb_index500_0; };",
        "s10 = tb_index500_0.x;",
        "s11 = tb_index500_0.y;",
        "s4 = tb_heap_index(s0 >> 5)",
        "Texture2D<float4> tb_heap_t2_4;",
        "tb_heap_t2_4 = ResourceDescriptorHeap[NonUniformResourceIndex(s4)];",
        "s20 = (s1 >> 4)",
        "SamplerDescriptorHeap[NonUniformResourceIndex(s20)]",
    ] {
        assert!(out.hlsl.contains(line), "{line}");
    }
}

#[test]
fn pixel_heap_bases_need_one_row_and_the_feature() {
    let base = super::super::pixel_heap_base;
    let ex = pixel(&SAMPLE);
    assert_eq!((base(&ex, 5), base(&ex, 6)), (Some(14), Some(16)));
    let mut samplers = ex.clone();
    samplers.required_features = Some(1 << 26);
    assert_eq!((base(&samplers, 5), base(&samplers, 6)), (None, Some(16)));
    let mut twice = ex.clone();
    twice.fssm = fssm(&[RESOURCE_HEAP, RESOURCE_HEAP], &[]);
    assert_eq!(base(&twice, 5), None);
    let mut wide = ex.clone();
    wide.fssm = fssm(&[[5, 0, 2, 0, 0x0e]], &[]);
    assert_eq!(base(&wide, 5), None);
    for stage in [Stage::Vertex, Stage::Compute, Stage::Mesh] {
        let mut other = ex.clone();
        other.stage = stage;
        assert_eq!(base(&other, 5), None);
    }
    // Without the feature the same program is refused as before.
    let mut plain = ex;
    plain.required_features = Some(0);
    assert!(refusal(&plain).contains("dynamic soffset unsupported"));
}

#[test]
fn pixel_heap_refuses_what_it_cannot_prove() {
    // A first-lane index is the pixel's own one: each pixel is its own wave.
    let mut lane = SAMPLE.to_vec();
    lane[0] = 0x8f008502; // s0 = s2 << 5
    lane.insert(0, 0x7e040505); // s2 = readfirstlane(v5)
    assert!(super::super::translate(&pixel(&lane)).is_ok());
    // The byte offset is not a whole descriptor.
    let mut unscaled = SAMPLE;
    unscaled[0] = 0x8f00840a; // s0 = s10 << 4
    assert!(refusal(&pixel(&unscaled)).contains("dynamic soffset unsupported"));
    let mut unscaled = SAMPLE;
    unscaled[1] = 0x8f01850b; // s1 = s11 << 5
    assert!(refusal(&pixel(&unscaled)).contains("dynamic soffset unsupported"));
    // A descriptor word is replaced after the load.
    let mut patched = SAMPLE.to_vec();
    patched.insert(6, 0x8f06850a); // s6 = s10 << 5
    assert!(refusal(&pixel(&patched)).contains("not an image descriptor"));
    // A buffer descriptor could be a constant buffer or a raw buffer.
    let mut buffer = SAMPLE;
    buffer[2] = 0xf4080107; // s[4:7] = heap[s14:15][s0]
    assert!(refusal(&pixel(&buffer)).contains("heap buffer in a pixel shader"));
    // The texel type of a view is unknown.
    let mut load = SAMPLE;
    load[6] = 0xf0000108; // image_load
    assert!(refusal(&pixel(&load)).contains("unknown component type"));
}

#[test]
fn sampled_value_must_not_keep_sampler_offset_proof() {
    // v7 = v5 << 4; sample overwrites v7; readfirstlane s1,v7.
    // The second sampler fetch must not treat sample bits as an aligned offset.
    let mut words = SAMPLE[..6].to_vec();
    words.push(0x340e0a84);
    words.extend([SAMPLE[6],SAMPLE[7],0x7e020507,SAMPLE[4],SAMPLE[5],
                  SAMPLE[6],SAMPLE[7],SAMPLE[8]]);
    assert!(refusal(&pixel(&words)).contains("dynamic soffset unsupported"));
}

#[test]
fn second_sample_component_must_kill_offset_proof() {
    // A two-component image result starts at v7 and overwrites v8 as well.
    let mut words = SAMPLE[..6].to_vec();
    words.push(0x34100a84); // v8 = v5 << 4
    words.extend([SAMPLE[6] | 0x200,SAMPLE[7],0x7e020508,SAMPLE[4],SAMPLE[5],
                  SAMPLE[6],SAMPLE[7],SAMPLE[8]]);
    assert!(refusal(&pixel(&words)).contains("dynamic soffset unsupported"));
}

#[test]
fn sampler_heap_can_track_a_vector_offset_copy() {
    // v6 = v5 << 4; v7 = v6; s1 = readfirstlane(v7).
    // The aligned sampler offset survives a genuine copy.
    let mut words = vec![SAMPLE[0],0x340c0a84,0x7e0e0306,0x7e020507];
    words.extend(&SAMPLE[2..]);
    assert!(super::super::translate(&pixel(&words)).is_ok());
}

#[test]
fn mixed_texture_words_are_not_an_intact_heap_descriptor() {
    // Two independent indices and loads, then replace word 1 of A with
    // word 1 of B. The old proof only checked the ordinal of each word.
    let mut words = vec![SAMPLE[0],SAMPLE[1],0x8f02850b,0xf40c0607,0x04000000];
    words.extend(&SAMPLE[2..6]);
    words.extend([0xbe850319,SAMPLE[6],SAMPLE[7],SAMPLE[8]]);
    assert!(refusal(&pixel(&words)).contains("not an image descriptor"));
}

#[test]
fn mixed_sampler_words_are_not_an_intact_heap_descriptor() {
    let mut words = vec![SAMPLE[0],SAMPLE[1],0x8f03840a,0xf4080608,0x06000000];
    words.extend(&SAMPLE[2..6]);
    words.extend([0xbe950319,SAMPLE[6],SAMPLE[7],SAMPLE[8]]);
    assert!(refusal(&pixel(&words)).contains("not a sampler descriptor"));
}

#[test]
fn complete_texture_descriptor_copy_stays_supported() {
    let mut words = vec![SAMPLE[0],SAMPLE[1],0x8f02850b,0xf40c0607,0x04000000];
    words.extend(&SAMPLE[2..6]);
    words.extend([0xbe840418,0xbe86041a,0xbe88041c,0xbe8a041e,
                  SAMPLE[6],SAMPLE[7],SAMPLE[8]]);
    assert!(super::super::translate(&pixel(&words)).is_ok());
}

#[test]
fn branch_must_not_hide_mixed_texture_words() {
    let mut words = vec![SAMPLE[0],SAMPLE[1],0x8f02850b,0xf40c0607,0x04000000];
    words.extend(&SAMPLE[2..6]);
    // Unconditional branch to the next instruction, then mixed word.
    words.extend([0xbf820000,0xbe850319,SAMPLE[6],SAMPLE[7],SAMPLE[8]]);
    assert!(refusal(&pixel(&words)).contains("not an image descriptor"));
}

#[test]
fn branch_can_select_complete_texture_descriptors() {
    // SCC from s1 = s11 << 4. If SCC=0 skip A; otherwise use A.
    // Each branch loads all eight words at the same destination.
    let words = [SAMPLE[0],0x8f02850b,SAMPLE[1],0xbf840003,
        SAMPLE[2],SAMPLE[3],0xbf820002,0xf40c0107,0x04000000,
        SAMPLE[4],SAMPLE[5],SAMPLE[6],SAMPLE[7],SAMPLE[8]];
    assert!(super::super::translate(&pixel(&words)).is_ok());
}

#[test]
fn branch_must_not_mix_only_on_one_path() {
    let mut words = vec![SAMPLE[0],SAMPLE[1],0x8f02850b,0xf40c0607,0x04000000];
    words.extend(&SAMPLE[2..6]);
    words.extend([0xbf840001,0xbe850319,SAMPLE[6],SAMPLE[7],SAMPLE[8]]);
    assert!(refusal(&pixel(&words)).contains("not an image descriptor"));
}

#[test]
fn loop_reloads_must_not_keep_previous_generation_words() {
    let mut words = SAMPLE[..6].to_vec();
    // Save old word 1, reload the complete texture descriptor, decrement
    // a counter and loop to the save. At exit graft the previous word.
    words.extend([0xbe830382,0xbe990305,0x8900a000,SAMPLE[2],SAMPLE[3],
                  0x80838103,0xbf068003,0xbf84fff9,
                  0xbe850319,SAMPLE[6],SAMPLE[7],SAMPLE[8]]);
    assert!(refusal(&pixel(&words)).contains("not an image descriptor"));
    words.remove(words.len()-4);
    assert!(super::super::translate(&pixel(&words)).is_ok());
}

#[test]
fn scalar_bitwise_offset_alignment_is_preserved_only_when_proven() {
    for op in [0x8900a000, 0x8800a000, 0x87008100] {
        let mut words = SAMPLE.to_vec();
        words.insert(2, op);
        assert!(super::super::translate(&pixel(&words)).is_ok(), "{op:#x}");
    }
    for op in [0x89008100, 0x88008100] {
        let mut words = SAMPLE.to_vec();
        words.insert(2, op);
        assert!(refusal(&pixel(&words)).contains("dynamic soffset unsupported"));
    }
}

#[test]
fn root_less_constants_must_agree_everywhere() {
    let ex = pixel(&SAMPLE);
    let (start, key) = rootless_constants(&ex).unwrap();
    assert_eq!((start, key.register, key.space, key.count), (10, 500, 0, 2));
    let with = |user: &[[u32; 5]], used: &[[u32; 5]]| {
        let mut ex = ex.clone();
        ex.fssm = fssm(user, used);
        rootless_constants(&ex)
    };
    assert!(with(&[CONSTANTS], &[]).is_none());
    assert!(with(&[CONSTANTS, CONSTANTS], &[USED]).is_none());
    assert!(with(&[CONSTANTS], &[USED, USED]).is_none());
    assert!(with(&[CONSTANTS], &[[2, 0, 501, 0, 0x30a04]]).is_none());
    assert!(with(&[CONSTANTS], &[[2, 0, 500, 0, 0x30b04]]).is_none());
    let mut unbound = ex.clone();
    unbound.resources.clear();
    assert!(rootless_constants(&unbound).is_none());
    let mut twice = ex.clone();
    twice.resources.push(PsvResource { res_type: 2, space: 1, lower: 500, upper: 500 });
    assert!(rootless_constants(&twice).is_none());
    // A heap index from an unresolved root constant would read as zero.
    assert!(refusal(&unbound).contains("root constant s10"));
}

#[test]
fn more_than_four_constants_are_declared_without_padding() {
    let mut ex = pixel(&SAMPLE);
    ex.fssm = fssm(&[[0, 0, 5, 500, 0x30a], [5, 0, 1, 0, 0x12], SAMPLER_HEAP], &[USED]);
    // The heap base moves to s18 so that it does not overlap the constants.
    ex.isa[8..12].copy_from_slice(&0xf40c0109u32.to_le_bytes());
    ex.isa_data = ex.isa.clone();
    let out = super::super::translate(&ex).unwrap();
    assert!(out.hlsl.contains("{ uint tb_index500_0_0; uint tb_index500_0_1; uint tb_index500_0_2; uint tb_index500_0_3; uint tb_index500_0_4; };"));
    assert!(out.hlsl.contains("s10 = tb_index500_0_0;"));
}

#[test]
fn vertex_heap_loads_take_their_type_from_the_first_use() {
    let program = |used: &[u32]| {
        let mut words = vec![
            0x8f00850a, // s0 = s10 << 5
            0xf4080107, 0x00000000, // s[4:7] = heap[s14:15][s0]
            0xe0002000, 0x80010100, // v1 = buffer_load_format_x(index v0, s[4:7])
            0xbf820000, // s_branch to the next instruction
        ];
        words.extend(used);
        words.push(0xbf810000);
        let mut ex = pixel(&words);
        ex.stage = Stage::Vertex;
        super::super::translate(&ex)
    };
    let out = program(&[0xd5690002, 0x0002020a]).unwrap(); // v2 = s10 * v1, integer
    assert!(out.hlsl.contains("Buffer<uint4> tb_heap_bufu_4;"));
    assert!(out.hlsl.contains("s10 = tb_index500_0.x;"));
    let out = program(&[0x0e040301]).unwrap(); // v2 = v1 * v1, float
    assert!(out.hlsl.contains("Buffer<float4> tb_heap_buf_4;"));
    // One path reads an integer, the other a float.
    let mixed = program(&[0xbf850002, 0xd5690002, 0x0002020a, 0x0e040301]);
    assert!(mixed.is_err_and(|reason| reason.contains("ambiguous")));
}

#[test]
fn compute_group_ids_follow_the_last_launch_argument() {
    let mut ex = pixel(&[0xbf810000]);
    ex.stage = Stage::Compute;
    let end = |rows: &[[u32; 5]]| {
        let mut ex = ex.clone();
        ex.fssm = fssm(rows, &[]);
        user_data_end(&ex)
    };
    let table = [3, 0x40, 12, 7, 0x105];
    assert_eq!(end(&[table]), Some(6));
    assert_eq!(end(&[table, [4, 0, 0, 1, 0xf00]]), Some(6)); // static sampler
    assert_eq!(end(&[table, [1, 0, 2, 7, 0x206]]), Some(8)); // root descriptor
    assert_eq!(end(&[table, CONSTANTS, [5, 0, 1, 0, 0x0c], [6, 0, 1, 0, 0x0d]]), Some(14));
    assert_eq!(end(&[table, [9, 0, 1, 0, 0x0c]]), None); // unknown argument
    assert_eq!(end(&[]), None);
}

#[test]
fn vertex_table_parameter_follows_the_shaders_own_records() {
    let mut ex = super::super::mesh::heap_extract(&[], 0, &[]);
    ex.stage = Stage::Vertex;
    // A fetch-call vertex shader: an SRV in table 0 (s8), a CBV and an SRV in table 1 (s9).
    ex.fssm = fssm(&[], &[[0x300, 0, 3, 0, 0x808], [0x302, 8, 22, 0, 0x10904], [0x300, 16, 92, 0, 0x10908]]);
    assert_eq!(vertex_table_parameter(&ex, 8), Some(0));
    assert_eq!(vertex_table_parameter(&ex, 9), Some(1));
    // No record for the register: the chained-entry rule still decides.
    assert_eq!(vertex_table_parameter(&ex, 19), None);
    // Root constants and root descriptors in a register do not make it a table.
    ex.fssm = fssm(&[], &[[2, 0, 500, 0, 0x20a04], [0x100, 0, 9, 0, 0x30b04]]);
    assert_eq!(vertex_table_parameter(&ex, 10), None);
    assert_eq!(vertex_table_parameter(&ex, 11), None);
    // Records that disagree about the parameter are not trusted.
    ex.fssm = fssm(&[], &[[0x300, 0, 3, 0, 0x10908], [0x302, 8, 5, 0, 0x20904]]);
    assert_eq!(vertex_table_parameter(&ex, 9), None);
    // Other stages keep their own register conventions.
    ex.fssm = fssm(&[], &[[0x300, 0, 3, 0, 0x10908]]);
    assert_eq!(vertex_table_parameter(&ex, 9), Some(1));
    ex.stage = Stage::Pixel;
    assert_eq!(vertex_table_parameter(&ex, 9), None);
}

/// A pixel program that samples the descriptor at s8 + 0x20 with the root's
/// static sampler s0, translated against a root whose slot 1 is `kind`.
fn sampled_table_slot(kind: u32) -> ShaderExtract {
    use crate::meta::{RootParam, RootRange};
    let words = [
        0xf40c0104u32, 0xfa000020, // s[4:11] = table[s8:9] + 0x20
        0xf09c0308, 0x00a10704, // v[7:8] = image_sample_lz(v4, s[4:11], s[20:23])
        0xbf810000,
    ];
    let isa: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut ex = super::super::mesh::heap_extract(&[], 0, &isa);
    ex.stage = Stage::Pixel;
    ex.shader_model = (6, 6);
    ex.required_features = Some(0);
    ex.fssm = fssm(&[], &[]);
    let range = |range_type, base_register| RootRange { range_type, num: 1, base_register, space: 0, offset_in_table: u32::MAX };
    let root = ex.root_sig.as_mut().unwrap();
    root.version = 2;
    root.params = vec![RootParam { visibility: 0, kind: RootParamKind::Table(vec![range(0, 0), range(kind, 7)]) }];
    let mut sampler = [0u32; 13];
    sampler[1..4].fill(3); // clamp
    sampler[9] = f32::MAX.to_bits();
    root.static_samplers = vec![sampler];
    root.num_static_samplers = 1;
    ex.resources = vec![PsvResource { res_type: 1, space: 0, lower: 0, upper: 0 }];
    ex
}

#[test]
fn sampling_a_root_uav_slot_names_the_mismatched_root() {
    let out = super::super::translate(&sampled_table_slot(0)).unwrap();
    assert!(out.hlsl.contains("tex7.SampleLevel(samp0"), "{}", out.hlsl);
    // The program samples what that root declares as a UAV. The root was
    // not the one the program's table follows.
    let reason = refusal(&sampled_table_slot(1));
    assert!(reason.contains("image_sample_lz reads table UAV u7"), "{reason}");
}

#[test]
fn a_literal_sampler_takes_the_private_static_the_runtime_appends() {
    // Texture from the resource heap; linear clamped sampler words built from literals.
    let mut words = SAMPLE[..4].to_vec();
    words.extend([0xb0140092, 0xbe9503ff, 0x00c00000, 0xbe9603ff, 0x18500000, 0xbe970380]);
    words.extend(&SAMPLE[6..]);
    let mut ex = pixel(&words);
    // Two declared samplers, so neither is the program's only one.
    ex.resources.push(PsvResource { res_type: 1, space: 1, lower: 5, upper: 5 });
    ex.resources.push(PsvResource { res_type: 1, space: 1, lower: 20, upper: 20 });
    // The title's root names another state at s20 (point clamped).
    let point = [0, 3, 3, 3, 0, 1, 1, 0, 0, 12.0_f32.to_bits(), 20, 1, 0];
    let root = |statics: Vec<[u32; 13]>| crate::meta::RootSig {
        version: 2, flags: 0x400, params: vec![], num_static_samplers: statics.len() as u32, static_samplers: statics,
    };
    ex.root_sig = Some(root(vec![point]));
    assert!(refusal(&ex).contains("not a sampler descriptor"));
    // inline_sampler::unmatched adds the literal state after the root's registers.
    let mut linear = point;
    (linear[0], linear[10], linear[11], linear[12]) = (0x15, 21, 32765, 5);
    ex.root_sig = Some(root(vec![point, linear]));
    let out = super::super::translate(&ex).unwrap();
    assert!(out.hlsl.contains("SamplerState samp21 : register(s21, space32765);"), "{}", out.hlsl);
}

#[test]
fn a_missing_literal_sampler_binds_the_private_register() {
    // A compute program whose literal sampler state the root does not hold.
    // The runtime appends the state after every sampler register the root
    // uses; the translator must bind the same private register.
    let mut words = SAMPLE[..4].to_vec();
    words.extend([0xb0140092, 0xbe9503ff, 0x00c00000, 0xbe9603ff, 0x18500000, 0xbe970380]);
    words.extend(&SAMPLE[6..]);
    let isa: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut ex = super::super::mesh::heap_extract(&[], 0x400, &isa);
    ex.stage = Stage::Compute;
    ex.shader_model = (6, 6);
    ex.required_features = Some(3 << 25);
    ex.fssm = fssm(&[CONSTANTS, RESOURCE_HEAP, SAMPLER_HEAP], &[USED]);
    ex.resources = vec![
        PsvResource { res_type: 2, space: 0, lower: 500, upper: 500 },
        PsvResource { res_type: 1, space: 1, lower: 5, upper: 5 },
    ];
    let mut point = [0u32; 13];
    point[1..4].fill(3); // clamp
    point[9] = 12.0_f32.to_bits();
    point[10] = 5;
    point[11] = 1;
    point[12] = 0;
    ex.root_sig = Some(crate::meta::RootSig {
        version: 2,
        flags: 0x400,
        params: Vec::new(),
        num_static_samplers: 1,
        static_samplers: vec![point],
    });
    ex.has_root_sig = true;
    let insns = crate::rdna2::disassemble(&ex.isa);
    let em = super::super::Emitter::new(&ex, insns.clone());
    assert_eq!(em.private_samplers.len(), 1);
    assert!(em.private_samplers.values().all(|&r| r == 6), "{:?}", em.private_samplers);
}

#[test]
fn cube_sample_with_lod_clamp_lowers_without_an_offset() {
    // The sample instruction with dim=3 (cube) and modi=1 (clamp).
    // v[4:6] coordinates, v7 clamp, v8 result.
    let mut words = SAMPLE.to_vec();
    words[6] = 0xf0840118; // image_sample_cl, dim 3
    words[7] = 0x00a10804; // vaddr v4, dst v8, SRSRC s4, SSAMP s20
    let out = super::super::translate(&pixel(&words)).unwrap();
    assert!(out.profile.starts_with("ps_6_"), "{}", out.profile);
    assert!(out.hlsl.contains("TextureCube<float4>"), "{}", out.hlsl);
    assert!(out.hlsl.contains(".Sample("), "{}", out.hlsl);
    assert!(out.hlsl.contains("tb_cube_dir(asfloat(v4), asfloat(v5), asfloat(v6))"), "{}", out.hlsl);
    assert!(out.hlsl.contains("asfloat(v7)"), "{}", out.hlsl);
}

/// A cube sample with derivatives takes them on the face: (ds, dt) along x, then
/// along y, before the (s, t, face) coordinates. HLSL takes derivatives of the
/// direction, which is affine in (s, t), so a difference of directions gives them.
fn cube_gradient_call(opcode: u32) -> String {
    let mut words = SAMPLE.to_vec();
    words[6] = opcode;
    words[7] = 0x00a10804; // vaddr v4, dst v8, SRSRC s4, SSAMP s20
    let out = super::super::translate(&pixel(&words)).unwrap();
    assert!(out.hlsl.contains("TextureCube<float4>"), "{}", out.hlsl);
    out.hlsl.lines().find(|line| line.contains(".SampleGrad(")).expect("a gradient sample").trim().to_owned()
}

fn along(ds: u32, dt: u32) -> String {
    format!("(tb_cube_dir(1.5 + asfloat(v{ds}), 1.5 + asfloat(v{dt}), asfloat(v10)) - tb_cube_dir(1.5, 1.5, asfloat(v10)))")
}

#[test]
fn a_cube_gradient_sample_reads_two_derivatives_per_direction() {
    let call = cube_gradient_call(0xf0880118); // image_sample_d, dim 3
    let expected = format!("tb_cube_dir(asfloat(v8), asfloat(v9), asfloat(v10)), {}, {});", along(4, 5), along(6, 7));
    assert!(call.ends_with(&expected), "{call}");
}

#[test]
fn cube_sample_with_derivatives_and_a_lod_clamp_lowers() {
    // modi=3 (d_cl): TextureCube.SampleGrad takes the clamp without an offset.
    let call = cube_gradient_call(0xf08c0118);
    let expected = format!("{}, {}, asfloat(v11));", along(4, 5), along(6, 7));
    assert!(call.ends_with(&expected), "{call}");
}

/// The Lanes `bindless-table` program under its root: one table with two raw
/// UAVs (u0 and u1, space1), one root constant (b0) and the direct-indexing
/// flag, followed by `private` root SRVs of the runtime (registers in space 32766).
fn table_constant_and_heap(private: &[u32]) -> ShaderExtract {
    use crate::meta::{RootParam, RootRange};
    let words = [
        0xbe800304u32, 0xbe810380, // s[0:1] = table 0
        0xf4080200, 0xfa000020, // s[8:11] = table[s0:1] + 0x20
        0x8f028505, // s2 = s5 << 5, the root constant
        0xf4080303, 0x04000000, // s[12:15] = heap[s6:7][s2]
        0x7e000281, // v0 = 1
        0xe0700000, 0x80020000, // table UAV store
        0x7e0202ff, 0x7ab1e232, // v1
        0xe0700000, 0x80030100, // heap UAV store
        0xbf810000,
    ];
    let isa: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut ex = super::super::mesh::heap_extract(&[], 0x400, &isa);
    ex.stage = Stage::Compute;
    ex.shader_model = (6, 6);
    ex.psv_threads = Some([64, 1, 1]);
    ex.fssm = fssm(
        &[[3, 8, 1, 1, 0x4], [0, 0, 1, 0, 0x105], [5, 0, 1, 0, 6]],
        &[[0x303, 8, 1, 1, 0x404], [2, 0, 0, 0, 0x1_0504]],
    );
    ex.resources = vec![
        PsvResource { res_type: 7, space: 1, lower: 1, upper: 1 },
        PsvResource { res_type: 2, space: 0, lower: 0, upper: 0 },
    ];
    let root = ex.root_sig.as_mut().unwrap();
    root.version = 2;
    root.params = vec![
        RootParam { visibility: 0, kind: RootParamKind::Table(vec![
            RootRange { range_type: 1, num: 2, base_register: 0, space: 1, offset_in_table: u32::MAX }]) },
        RootParam { visibility: 0, kind: RootParamKind::Constants { register: 0, space: 0, num32: 1 } },
    ];
    root.params.extend(private.iter().map(|&register| RootParam {
        visibility: 0, kind: RootParamKind::Descriptor { kind: 3, register, space: 32766 },
    }));
    ex
}

#[test]
fn the_runtimes_private_root_srvs_leave_a_compute_roots_layout_alone() {
    let guest = super::super::translate(&table_constant_and_heap(&[])).unwrap().hlsl;
    for line in [
        "cbuffer TBIndex0_0 : register(b0, space0) { uint tb_index0_0; };",
        "s5 = tb_index0_0.x;",
        "s12 = tb_heap_index(s2 >> 5);",
        "uint tb_heap_index(uint i) { return i; }",
        "RWByteAddressBuffer raw_ubuf1 : register(u1, space1);",
    ] {
        assert!(guest.contains(line), "{line}\n{guest}");
    }
    // With the index map appended, the root constant and every register stay
    // where the title's root put them. Only the heap index goes through the map.
    let mapped = super::super::translate(&table_constant_and_heap(&[0])).unwrap().hlsl;
    let expected = guest.replace(
        "uint tb_heap_index(uint i) { return i; }",
        "uint tb_heap_index(uint i) { return tb_heap_map.Load(0) == 0u ? i : tb_heap_map.Load(4u+i*4u); }",
    );
    let declared: String = mapped.lines()
        .filter(|line| *line != "ByteAddressBuffer tb_heap_map : register(t0, space32766);")
        .flat_map(|line| [line, "\n"]).collect();
    assert_ne!(declared, mapped, "{mapped}");
    assert_eq!(declared, expected);
    // The stride table alone, or after the map, changes how the heap descriptor
    // is loaded, not where the constant is. A root SRV of the title's own is a
    // parameter like any other: it does change the root's shape.
    for private in [&[1][..], &[0, 1]] {
        let strided = super::super::translate(&table_constant_and_heap(private)).unwrap().hlsl;
        for line in ["cbuffer TBIndex0_0 : register(b0, space0) { uint tb_index0_0; };", "s5 = tb_index0_0.x;", "tb_heap_strides.Load"] {
            assert!(strided.contains(line), "{private:?}: {line}\n{strided}");
        }
    }
}

#[test]
fn scalar_load_through_a_root_descriptor_address_is_refused() {
    // A root SRV's address arrives in s7:s8 (user-data class 1). A scalar load
    // through it reads the buffer's own words, not a descriptor of a table.
    let words = [
        0xbe800307, // s0 = s7
        0xbe810308, // s1 = s8
        0xf4000080, 0xfa000000, // s2 = [s0:1 + 0]
        0x7e000202, // v0 = s2
        0xbf810000,
    ];
    let mut ex = pixel(&words);
    ex.stage = Stage::Compute;
    ex.required_features = None;
    ex.fssm = fssm(&[[1, 0, 2, 5, 0x307]], &[[0x100, 0, 5, 1, 0x30704]]);
    ex.resources = vec![PsvResource { res_type: 4, space: 7, lower: 5, upper: 5 }];
    assert!(refusal(&ex).contains("s_load at 0x8: base s7 is a root descriptor address"));
    // The same load through a register no record names is a table fetch, as before.
    ex.fssm = fssm(&[], &[]);
    ex.resources.clear();
    assert!(super::super::translate(&ex).is_ok());
}

#[test]
fn heap_offset_computed_into_vcc_hi_is_recognized() {
    // s_lshl_b32 [VccHi] <- [Sgpr(12), ConstInt(5)]: the HeapOffset marking names a
    // register decoded as VccHi (SOFFSET 107), which is not an ordinary SGPR.
    // The heap index scales by the same 32-byte descriptor stride as any other
    // offset, and the load indexes it the same way.
    let words = [
        0x8f6b850c,            // s_lshl_b32 [VccHi] <- [Sgpr(12), ConstInt(5)]
        0xf40c0507, 0xd6000000, // s_load_dwordx8 [Sgpr(20)] <- [Sgpr(14)] imm=0, soff=107
        0xbf810000,            // s_endpgm
    ];
    let out = super::super::translate(&pixel(&words)).unwrap();
    assert!(out.hlsl.contains("s20 = tb_heap_index(vcc_hi >> 5)"), "{}", out.hlsl);
    // vcc_hi is declared by the function header alone: the emitter must not add it
    // again to the register declarations (dxc rejects the duplicate).
    let declarations = out.hlsl.lines()
        .filter(|line| line.contains("uint") && line.contains("vcc_hi ="))
        .count();
    assert_eq!(declarations, 1, "{declarations} vcc_hi declaration lines: {out:?}", out = out.hlsl);
}
