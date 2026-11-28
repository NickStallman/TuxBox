use super::*;
use crate::meta::{RootParam, RootParamKind, RootRange, RootSig};
use crate::psv::PsvResource;
use crate::{ShaderExtract, Stage};

mod fixture {
    include!("../../../examples/support/key_count_fixture.rs");
}
use fixture::Valid;

fn program(wide: bool, valid: Valid, masked: bool, after: &[u32]) -> Vec<Insn> {
    let words = fixture::words(wide, valid, masked, after);
    disassemble(&words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>())
}

fn lowered(wide: bool, valid: Valid, masked: bool, after: &[u32]) -> (Vec<Insn>, Vec<usize>) {
    let mut insns = program(wide, valid, masked, after);
    let adds = lower_key_counts(&mut insns);
    (insns, adds)
}

/// Whether nothing was rewritten.
fn kept(mut insns: Vec<Insn>) -> bool {
    let before: Vec<String> = insns.iter().map(|i| i.mnemonic.clone()).collect();
    lower_key_counts(&mut insns).is_empty() && insns.iter().map(|i| &i.mnemonic).eq(before.iter())
}

const SMOV_S40_S20: u32 = 0xbea8_0314;
const VMOV_V20_V9: u32 = 0x7e28_0309;
const VMOV_V9_0: u32 = 0x7e12_0280;

#[test]
fn every_counting_lane_adds_one_at_its_own_key() {
    for wide in [false, true] {
        for valid in [Valid::Before, Valid::Inside] {
            for masked in [false, true] {
                let (insns, adds) = lowered(wide, valid, masked, &[]);
                let [add] = adds[..] else { panic!("one add, found {adds:?}") };
                assert_eq!(insns[add].mnemonic, "buffer_atomic_add");
                // The count and the element come from the lane itself.
                let moves: Vec<&Insn> = insns[..add].iter().rev().filter(|i| i.mnemonic == "v_mov_b32").take(2).collect();
                assert_eq!((&moves[0].dst, &moves[0].src), (&vec![Opnd::Vgpr(15)], &vec![Opnd::Vgpr(3)]));
                assert_eq!((&moves[1].dst, &moves[1].src), (&vec![Opnd::Vgpr(14)], &vec![Opnd::ConstInt(1)]));
                // Nothing reads another lane or branches any more.
                let wave = |i: &&Insn| i.mnemonic.contains("lane") || i.mnemonic.contains("mbcnt") || i.mnemonic.contains("branch")
                    || i.mnemonic.starts_with("s_ff1") || i.mnemonic.starts_with("s_bcnt") || i.mnemonic.starts_with("s_or_saveexec")
                    || i.src.iter().any(|s| matches!(s, Opnd::Raw(_)));
                assert_eq!(insns.iter().filter(wave).count(), 0, "wide={wide}");
                // EXEC narrows to the counting lanes and returns to what it was.
                let suffix = if wide { "b64" } else { "b32" };
                let mask = if valid == Valid::Before { Opnd::Sgpr(12) } else { Opnd::VccLo };
                let and = insns.iter().find(|i| i.mnemonic == format!("s_and_{suffix}")).unwrap();
                assert_eq!(and.src, vec![mask, mask]);
                let exec: Vec<&Insn> = insns.iter().filter(|i| i.dst == [Opnd::ExecLo] && i.mnemonic.starts_with("s_mov")).collect();
                assert_eq!(exec.iter().map(|i| i.src[0]).take(2).collect::<Vec<_>>(), [Opnd::Sgpr(18), Opnd::Sgpr(14)]);
            }
        }
    }
}

#[test]
fn a_leftover_that_is_read_keeps_the_loop() {
    // The count, and a sum register in lanes the loop wrote with every lane enabled.
    assert!(kept(program(false, Valid::Inside, false, &[SMOV_S40_S20])));
    assert!(kept(program(true, Valid::Before, false, &[VMOV_V20_V9])));
    // Written again first, under the same EXEC, it is no longer the loop's value.
    let (_, adds) = lowered(false, Valid::Inside, false, &[VMOV_V9_0, VMOV_V20_V9]);
    assert_eq!(adds.len(), 1);
    // After EXEC may widen, the write no longer covers every lane that reads.
    let restore = 0xbefe_030e; // s_mov_b32 exec_lo, s14
    assert!(kept(program(false, Valid::Inside, false, &[VMOV_V9_0, restore, VMOV_V20_V9])));
    // VCC and SCC are leftovers too.
    assert!(kept(program(false, Valid::Before, false, &[0xbf85_0000]))); // s_cbranch_scc1
    assert!(kept(program(false, Valid::Before, false, &[0xbea8_036a]))); // s_mov_b32 s40, vcc_lo
}

#[test]
fn another_shape_keeps_the_loop() {
    let find = |insns: &[Insn], mnemonic: &str| insns.iter().position(|i| i.mnemonic == mnemonic).unwrap();
    let rebuilt = |mut words: Vec<u32>, at: usize, word: u32| {
        words[at] = word;
        disassemble(&words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>())
    };
    for wide in [false, true] {
        let words = fixture::words(wide, Valid::Inside, false, &[]);
        let insns = program(wide, Valid::Inside, false, &[]);
        let word = |mnemonic: &str| insns[find(&insns, mnemonic)].offset / 4;
        // The old value returned to the adding lane (GLC).
        let atomic = word("buffer_atomic_add");
        assert!(kept(rebuilt(words.clone(), atomic, words[atomic] | 0x4000)));
        // Another atomic.
        assert!(kept(rebuilt(words.clone(), atomic, words[atomic] + 0x4_0000)));
        // A sum step that selects other lanes.
        let step = insns.iter().find(|i| i.src == [Opnd::Raw(233)]).unwrap().offset / 4 + 1;
        assert!(kept(rebuilt(words.clone(), step, words[step] ^ 0x100)));
        // The count read from another lane.
        let read = insns.iter().rposition(|i| i.mnemonic == "v_readlane_b32").unwrap();
        let read = insns[read - 1].offset / 4 + 1;
        assert!(kept(rebuilt(words.clone(), read, words[read] + 0x200)));
        // The count taken from the high word of the sum.
        let moved = insns[find(&insns, "buffer_atomic_add") - 3].offset / 4;
        assert!(kept(rebuilt(words.clone(), moved, words[moved] + 1)));
        // A pass that removes the lanes that did not match.
        let remain = word("v_xor_b32");
        assert!(kept(rebuilt(words.clone(), remain, 0x7e00_0000)));
        // A branch into the loop from outside.
        let mut entered = words.clone();
        let target = word("v_readfirstlane_b32");
        entered.insert(2, 0xbf85_0000 | (target as u32 + 1 - 3));
        let entered = disassemble(&entered.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>());
        assert!(lower_key_counts(&mut entered.clone()).is_empty());
    }
    // In a 64-lane wave a mask takes two registers; the saved EXEC may not share one with the key.
    let words = fixture::words(true, Valid::Inside, false, &[]);
    let insns = program(true, Valid::Inside, false, &[]);
    let save = insns[find(&insns, "s_and_saveexec_b64")].offset / 4;
    let clash = rebuilt(words.clone(), save, words[save] + (12 << 16)); // s26:s27 with the key in s27
    assert_eq!(clash[find(&clash, "s_and_saveexec_b64")].dst, [Opnd::Sgpr(26)]);
    assert!(lower_key_counts(&mut clash.clone()).is_empty());
}

#[test]
fn two_loops_answer_for_each_other() {
    // The second loop reads only its own key, mask and descriptor.
    let mut words = fixture::words(false, Valid::Inside, false, &[]);
    words.pop();
    let second = fixture::words(false, Valid::Inside, false, &[]);
    words.extend_from_slice(&second[2..]);
    let mut insns = disassemble(&words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>());
    assert_eq!(lower_key_counts(&mut insns).len(), 2);
    // A mask from before both loops is not something the first one leaves behind.
    let mut words = fixture::words(false, Valid::Before, false, &[]);
    words.pop();
    let start = fixture::words(false, Valid::Before, false, &[]).iter().position(|&w| w == 0x7e0c_0281).unwrap();
    words.extend_from_slice(&second_from(start));
    let mut insns = disassemble(&words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>());
    assert_eq!(lower_key_counts(&mut insns).len(), 2);
}

fn second_from(start: usize) -> Vec<u32> {
    fixture::words(false, Valid::Before, false, &[])[start..].to_vec()
}

#[test]
fn the_translation_has_no_wave_operation() {
    // 64-lane programs, which nothing but the rewrite translates.
    for (wide, threads) in [(true, 64), (true, 128)] {
        for valid in [Valid::Before, Valid::Inside] {
            let shader = fixture::shader(threads, &fixture::words(wide, valid, true, &[]));
            let translated = crate::hlsl::translate(&shader).unwrap_or_else(|e| panic!("wide={wide}: {e}"));
            let body = &translated.hlsl[translated.hlsl.find("void main(").unwrap()..];
            assert_eq!(body.matches("InterlockedAdd(utb0[v15], v14, ").count(), 1, "{body}");
            assert!(!body.contains("Wave") && !body.contains("tb_guard") && !body.contains("groupshared"), "{body}");
            assert_eq!(translated.required_wave_size, None);
            // A narrow host runs the same program.
            if wide {
                assert_eq!(crate::hlsl::translate_for_wave32_host(&shader).unwrap().hlsl, translated.hlsl);
            }
        }
    }
}

#[test]
fn counters_another_instruction_uses_keep_the_refusal() {
    // buffer_load_format_x v20, v3, s[8:11], idxen: the same UAV, read after the loop.
    let load = [0xe000_2000, 0x8002_1403];
    let shader = fixture::shader(64, &fixture::words(true, Valid::Inside, false, &load));
    // Without the rewrite the program keeps the refusal it had.
    let error = crate::hlsl::translate(&shader).err().expect("refused");
    assert!(error.contains("inside non-uniform loop"), "{error}");
}


#[test]
fn a_loop_the_whole_wave_mode_translates_is_not_rewritten() {
    // In a 32-lane program the translator keeps the wave together for the
    // loop's lane reads, so the loop stays as the console runs it.
    for threads in [32, 96] {
        for valid in [Valid::Before, Valid::Inside] {
            let shader = fixture::shader(threads, &fixture::words(false, valid, true, &[]));
            let translated = crate::hlsl::translate(&shader).unwrap();
            assert_eq!(translated.required_wave_size, Some(32));
            // The first pending lane's key is still read from the wave.
            assert!(translated.hlsl.contains("= WaveReadLaneAt(v3, "), "{}", translated.hlsl);
        }
    }
}
