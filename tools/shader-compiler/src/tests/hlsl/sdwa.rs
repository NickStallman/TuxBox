use crate::hlsl::float64::tests::translate;
use crate::rdna2::disassemble;
use crate::Stage;

// SDWA with src0 v1, src1 v2, destination v3 and every select DWORD.
const DWORDS: u32 = 0x0606_0601;

#[test]
fn sdwa_fields_a_form_does_not_apply_are_refused() {
    for (word, mnemonic, extension) in [
        (0x5006_04f9u32, "v_add_co_ci_u32", 0x0605_0601u32), // src0 WORD_1
        (0x7e06_54f9, "v_rcp_f32", DWORDS | 1 << 13),         // clamp
        (0x3606_04f9, "v_and_b32", DWORDS | 1 << 14),         // omod
    ] {
        let bytes: Vec<u8> = [word, DWORDS].iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(disassemble(&bytes)[0].mnemonic, mnemonic);
        // The carry-in forms are translated before SDWA selection, so refuse any SDWA.
        assert_eq!(translate(Stage::Compute, &[word, DWORDS]).is_ok(), mnemonic != "v_add_co_ci_u32", "{mnemonic}");
        let error = translate(Stage::Compute, &[word, extension]).unwrap_err();
        assert!(error.contains("unsupported SDWA"), "{mnemonic}: {error}");
    }
}

