//! Every instruction the decoder names, given to the translator alone.
//!
//! The record shows which instructions the translator has a path for. A change
//! that adds or loses one shows here. When the change is meant, update the
//! record and include it in the pull request:
//! `TUXBOX_BLESS_INSTRUCTIONS=1 cargo test ... instruction_inventory`.
use std::collections::BTreeMap;
use std::fmt::Write;

use crate::rdna2::{disassemble, Enc};
use crate::{ShaderExtract, Stage};

const RECORD: &str = include_str!("instruction_inventory.txt");
const END: u32 = 0xbf81_0000;

/// One instruction of `enc` with opcode `op`, over low registers and no modifiers.
fn encode(enc: Enc, op: u32) -> Vec<u32> {
    let sources = 257 | 258 << 9 | 259 << 18;
    match enc {
        Enc::Sop2 => vec![0x8000_0000 | op << 23 | 3 << 16 | 2 << 8 | 1],
        Enc::Sopk => vec![0xb000_0000 | op << 23 | 3 << 16 | 1],
        Enc::Sop1 => vec![0xbe80_0000 | 3 << 16 | op << 8 | 1],
        Enc::Sopc => vec![0xbf00_0000 | op << 16 | 2 << 8 | 1],
        Enc::Sopp => vec![0xbf80_0000 | op << 16],
        // s[2:3] as the base, no offset register.
        Enc::Smem => vec![0xf400_0080 | op << 18, 0xfa00_0000],
        Enc::Vop2 => vec![op << 25 | 3 << 17 | 2 << 9 | 257],
        Enc::Vop1 => vec![0x7e00_0000 | 3 << 17 | op << 9 | 257],
        Enc::Vopc => vec![0x7c00_0000 | op << 17 | 2 << 9 | 257],
        Enc::Vop3 if op >= 0x400 => vec![0xcc00_0000 | (op & 0x7f) << 16 | 3, sources],
        Enc::Vop3 => vec![0xd400_0000 | op << 16 | 3, sources],
        Enc::Vinterp => vec![0xc800_0000 | 3 << 18 | op << 16 | 1],
        Enc::Ds => vec![0xd800_0000 | op << 18, 3 << 24 | 2 << 8 | 1],
        Enc::Flat => vec![0xdc00_0000 | op << 18, 3 << 24 | 0x7f << 16 | 2 << 8 | 1],
        // v0 as data, no vector address, the descriptor in s[8:11].
        Enc::Mubuf => vec![0xe000_0000 | op << 18, 0x8002_0000],
        Enc::Mtbuf => vec![0xe800_0000 | (op & 7) << 16, 0x8002_0000 | (op >> 3) << 21],
        Enc::Mimg => vec![0xf000_0f08 | (op & 0x7f) << 18 | op >> 7, 0x0002_0004],
        Enc::Exp => vec![0xf800_080f, 0x0302_0100],
        Enc::Unknown => Vec::new(),
    }
}

const FAMILIES: [(Enc, u32); 17] = [
    (Enc::Sop2, 0x60), (Enc::Sopk, 29), (Enc::Sop1, 0x100), (Enc::Sopc, 0x80), (Enc::Sopp, 0x80),
    (Enc::Smem, 0x100), (Enc::Vop2, 0x3e), (Enc::Vop1, 0x100), (Enc::Vopc, 0x100), (Enc::Vop3, 0x480),
    (Enc::Vinterp, 4), (Enc::Ds, 0x100), (Enc::Flat, 0x80), (Enc::Mubuf, 0x100), (Enc::Mtbuf, 16),
    (Enc::Mimg, 0x100), (Enc::Exp, 1),
];

fn program(stage: Stage, words: &[u32]) -> ShaderExtract {
    let isa: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    ShaderExtract {
        flat_inputs: Vec::new(), flat_outputs: Vec::new(), stage, shader_model: (6, 0),
        required_features: None, bitcode_stripped: true, fspx_config: None, ps09_pixel_inputs: None,
        fssm: Vec::new(), entry: Some("main".into()), isa_data: isa.clone(), isa, isa_offset_in_fssm: 0,
        psv_threads: None, isa_preamble: [0, 64, 1, 1], has_input_sig: false, has_output_sig: false,
        has_psv0: false, has_root_sig: false, input_sig: Vec::new(), output_sig: Vec::new(),
        resources: Vec::new(), gs_info: None, mesh_info: None, primitive_sig: Vec::new(), root_sig: None,
    }
}

/// The first line of a refusal, without the instruction's name and position.
fn reason(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    let line = line.rsplit_once(" [").filter(|(_, tail)| tail.ends_with(']')).map_or(line, |(head, _)| head);
    line.replace(" at 0x0", "")
}

/// Whether the translation holds a statement for the instruction at offset 0.
fn emits(hlsl: &str) -> bool {
    let Some(start) = hlsl.find("// 0x0000 ") else { return false };
    hlsl[start..].lines().skip(1).take_while(|line| !line.trim_start().starts_with("// 0x"))
        .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with("//") && line.trim() != "}")
}

/// `yes` when some stage translates the instruction alone, `nothing` when it
/// translates to no statement, else why compute refuses it.
/// The DXC target `xshader` writes for a translation, as `main.rs` chooses it.
fn target(stage: Stage, done: &super::Translated) -> String {
    let model = if done.required_wave_size.is_some() || done.typed_int64_atomics
        || matches!(done.profile, "ps_6_6" | "cs_6_6" | "ms_6_6") { "6_6" }
        else if stage == Stage::Mesh || done.profile == "cs_6_5" { "6_5" }
        else if done.packed_dot_product { "6_4" }
        else if done.needs_barycentrics { "6_1" }
        else { "6_0" };
    format!("{}_{model}", done.profile.split('_').next().unwrap_or("cs"))
}

fn outcome(words: &[u32]) -> (String, Option<(String, String)>) {
    let mut refusal = String::new();
    for stage in [Stage::Compute, Stage::Pixel, Stage::Vertex] {
        let shader = program(stage, &[words, &[END]].concat());
        match std::panic::catch_unwind(|| super::translate(&shader)) {
            Ok(Ok(done)) => {
                let result = if emits(&done.hlsl) { "yes" } else { "nothing" };
                let text = if stage == Stage::Compute { result.into() } else { format!("{result} ({stage:?} only)") };
                let profile = target(stage, &done);
                return (text, (result == "yes").then_some((done.hlsl, profile)));
            }
            Ok(Err(why)) if refusal.is_empty() => refusal = reason(&why),
            Ok(Err(_)) => {}
            // The translator must refuse, never crash.
            Err(_) => return (format!("CRASH in {stage:?}"), None),
        }
    }
    (refusal, None)
}

#[test]
fn instruction_inventory_matches_the_record() {
    let mut now = String::new();
    let mut seen = BTreeMap::new();
    // With this set, every translation that emits a statement is written to the
    // folder it names, for `shader_harness.py inventory --compile` to compile.
    let dump = std::env::var_os("TUXBOX_INSTRUCTION_HLSL").map(std::path::PathBuf::from);
    for (enc, limit) in FAMILIES {
        for op in 0..limit {
            let words = encode(enc, op);
            let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
            let decoded = &disassemble(&bytes)[0];
            if decoded.mnemonic.contains('<') { continue; }
            assert_eq!((decoded.enc, decoded.op, decoded.dwords), (enc, op, words.len()), "{}", decoded.mnemonic);
            *seen.entry(format!("{enc:?}")).or_insert(0) += 1;
            let (result, translated) = outcome(&words);
            if let (Some(folder), Some((hlsl, profile))) = (&dump, translated) {
                let stem = folder.join(format!("{enc:?}-{op:#x}-{}", decoded.mnemonic));
                std::fs::write(stem.with_extension("hlsl"), hlsl).unwrap();
                std::fs::write(stem.with_extension("profile"), profile).unwrap();
            }
            assert!(!result.starts_with("CRASH"), "the translator crashed on {}: it must refuse", decoded.mnemonic);
            writeln!(now, "{enc:?} {op:#x} {} | {result}", decoded.mnemonic).unwrap();
        }
    }
    if std::env::var_os("TUXBOX_BLESS_INSTRUCTIONS").is_some() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tests/hlsl/instruction_inventory.txt");
        std::fs::write(path, &now).unwrap();
        println!("{seen:?}");
        return;
    }
    let recorded = RECORD.replace("\r\n", "\n");
    let changed: Vec<&str> = now.lines().filter(|line| !recorded.lines().any(|old| old == *line)).collect();
    assert!(now == recorded, "these instructions translate differently from the record:\n{}\nIf this is intended, \
        rerun with TUXBOX_BLESS_INSTRUCTIONS=1, regenerate docs/INSTRUCTIONS.md and commit both.", changed.join("\n"));
}
