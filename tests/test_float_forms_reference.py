import unittest

from tools.check_float_forms_gpu import FORMS, KNOWN, OUTPUTS, Open, chunks_of, known_for, mix, program


FORM = {form[0]: form[3] for form in FORMS}
QUIET = 0x7fc00000


def result(label, a, b=0, c=0):
    return FORM[label](a, b, c)


class FloatFormsReferenceTests(unittest.TestCase):
    # Hand-computed values, independent of the model's formulas.
    def assertConsole(self, want, got):
        """`got` is the console's first expectation."""
        self.assertIsNone(want.reason(got), want)

    def assertAlternative(self, want, got, reason):
        self.assertEqual(want.reason(got), reason, want)

    def test_legacy_product_of_zero_is_zero(self):
        plain = "v_mul_legacy_f32 v{d}, s20, v5"
        for other in (0x7f800000, 0xff800000, QUIET, 0xbf800000, 0x80000000):
            self.assertConsole(result(plain, 0x80000000, other), 0)
            self.assertConsole(result(plain, other, 0), 0)
            # The manual gives no sign.
            self.assertAlternative(result(plain, 0x80000000, other), 0x80000000, "legacy zero product is -0")
        # Underflow of a nonzero product keeps its sign.
        self.assertConsole(result(plain, 0x80800000, 0x00800000), 0x80000000)
        self.assertConsole(result(plain, 0x40000000, 0xc0400000), 0xc0c00000)
        self.assertFalse(result(plain, 0x40000000, 0xc0400000).matches(0xc0c00001))
        # A denormal source is read as zero.
        self.assertConsole(result("v_mul_legacy_f32 v{d}, v4, v5 (denormals)", 0x00000001, 0x7f800000), 0)
        self.assertConsole(result("v_mul_legacy_f32 v{d}, 0, v5", 0x3f800000, QUIET), 0)

    def test_fused_rounding(self):
        fmac = "v_fmac_f32 v{d}, s20, v5"
        # (1 + 2^-12)^2 - (1 + 2^-11) = 2^-24, which a separate multiply rounds away.
        self.assertConsole(result(fmac, 0x3f800800, 0x3f800800, 0xbf801000), 0x33800000)
        self.assertAlternative(result(fmac, 0x3f800800, 0x3f800800, 0xbf801000), 0, "rounded twice")
        self.assertFalse(result(fmac, 0x3f800800, 0x3f800800, 0xbf801000).matches(0x34000000))
        # 2 * MAX - MAX is MAX, and infinity if rounded twice.
        self.assertConsole(result(fmac, 0x7f7fffff, 0x40000000, 0xff7fffff), 0x7f7fffff)
        self.assertAlternative(result(fmac, 0x7f7fffff, 0x40000000, 0xff7fffff), 0x7f800000, "rounded twice")
        self.assertConsole(result(fmac, 0, 0x7f800000, 0x3f800000), QUIET)
        self.assertConsole(result(fmac, 0x7f800000, 0x3f800000, 0xff800000), QUIET)
        self.assertConsole(result(fmac, 0x80000000, 0x3f800000, 0x80000000), 0x80000000)
        self.assertConsole(result(fmac, 0x3f000000, 0xbf800000, 0x3f000000), 0)
        self.assertFalse(result(fmac, 0x3f000000, 0xbf800000, 0x3f000000).matches(0x80000000))
        self.assertConsole(result("v_fmac_f32 v{d}, 0x40400000, v5", 0, 0x40000000, 0x3f800000), 0x40e00000)
        for shared in ("v_fmac_f32 v{d}, v4, v5 (product also used)", "v_fma_f32 v{d}, v4, v5, v6 (product also used)"):
            self.assertConsole(result(shared, 0x3f800800, 0x3f800800, 0xbf801000), 0x33800000)
            self.assertAlternative(result(shared, 0x3f800800, 0x3f800800, 0xbf801000), 0, "rounded twice")
            # Equal first and third inputs select the rounded product.
            self.assertConsole(result(shared, 0x3f800800, 0x3f800800, 0x3f800800), 0x3f801000)

    def test_vop3_modifiers(self):
        self.assertConsole(result("v_fmac_f32_e64 v{d}, -v4, v5", 0x40000000, 0x40000000, 0x3f800000), 0xc0400000)
        self.assertConsole(result("v_fmac_f32_e64 v{d}, v4, v5 div:2", 0x40000000, 0x40000000, 0x3f800000),
                           0x40200000)
        # OMOD turns -0 into +0, unless MODE.IEEE disables it.
        omod = result("v_fmac_f32_e64 v{d}, v4, v5 mul:2", 0x80000000, 0x3f800000, 0x80000000)
        self.assertConsole(omod, 0)
        self.assertAlternative(omod, 0x80000000, "OMOD kept -0")
        # OMOD scales the rounded result, so an overflow stays infinite.
        legacy_half = result("v_mul_legacy_f32_e64 v{d}, v4, v5 div:2", 0x7f7fffff, 0x40000000)
        self.assertConsole(legacy_half, 0x7f800000)
        self.assertFalse(legacy_half.matches(0x7f7fffff))
        clamp = "v_mul_legacy_f32_e64 v{d}, v4, v5 clamp"
        self.assertConsole(result(clamp, 0x40000000, 0x3f800000), 0x3f800000)
        self.assertConsole(result(clamp, 0xc0000000, 0x3f800000), 0)
        self.assertFalse(result(clamp, 0x40000000, 0x3f800000).matches(0x40000000))
        # NaN passes through CLAMP with MODE.DX10_CLAMP off, which isn't measured.
        self.assertConsole(result(clamp, QUIET, 0x3f800000), QUIET)
        self.assertAlternative(result(clamp, QUIET, 0x3f800000), 0, "NaN clamped to 0")
        self.assertConsole(result("v_mul_legacy_f32_e64 v{d}, |v4|, |v5|", 0xc0000000, 0xc0400000), 0x40c00000)
        # A denormal result scaled by OMOD may be flushed or kept.
        scaled = result("v_fract_f32_e64 v{d}, v4 div:2", 0x00800000)
        self.assertConsole(scaled, 0)
        self.assertTrue(scaled.matches(0x00400000))

    def test_fraction(self):
        fract = "v_fract_f32 v{d}, s20"
        self.assertConsole(result(fract, 0xbf99999a), 0x3f4ccccc)  # -1.2 -> 0.8
        self.assertConsole(result(fract, 0xbfc00000), 0x3f000000)
        self.assertConsole(result(fract, 0x3f7fffff), 0x3f7fffff)
        self.assertConsole(result(fract, 0xbf7fffff), 0x33800000)
        # -2^-25 + 1 rounds to 1.0.
        self.assertConsole(result(fract, 0xb3000000), 0x3f7fffff)
        self.assertAlternative(result(fract, 0xb3000000), 0x3f800000, "fraction rounded to 1.0")
        for zero in (0x80000000, 0xc0400000):
            self.assertConsole(result(fract, zero), 0)
            self.assertFalse(result(fract, zero).matches(0x80000000))
        self.assertConsole(result("v_fract_f32 v{d}, v4 (denormals)", 0x80000001), 0)
        for special in (0x7f800000, 0xff800000, QUIET):
            self.assertConsole(result(fract, special), QUIET)
        self.assertConsole(result("v_fract_f32_e64 v{d}, -v4", 0x3fc00000), 0x3f000000)

    def test_byte_selection(self):
        word = 0x80ff7f01
        for k, value in enumerate((0x3f800000, 0x42fe0000, 0x437f0000, 0x43000000)):
            self.assertEqual(result(f"v_cvt_f32_ubyte{k} v{{d}}, s20", word).bits, value)
            self.assertEqual(result(f"v_cvt_f32_ubyte0_sdwa v{{d}}, v4 src0_sel:BYTE_{k}", word).bits, value)
        self.assertEqual(result("v_cvt_f32_ubyte1_sdwa v{d}, v4 src0_sel:WORD_1", word).bits, 0x43000000)
        self.assertEqual(result("v_cvt_f32_ubyte0_e64 v{d}, v4 mul:2", word).bits, 0x40000000)
        self.assertEqual(result("v_cvt_f32_ubyte3_e64 v{d}, v4 clamp", word).bits, 0x3f800000)

    def test_mixed_precision_sources(self):
        # S0 = high half 1.0, S1 = low half -2.0 (f16), S2 = f32 3.0: 1 * -2 + 3 = 1.
        self.assertConsole(mix(0b001, 0b011)(0x3c000000, 0x0000c000, 0x40400000), 0x3f800000)
        # Without OPSEL_HI the word is an f32, whatever OPSEL says.
        self.assertConsole(mix(0b111, 0)(0x40000000, 0x40400000, 0x3f800000), 0x40e00000)
        # NEG_HI is abs, applied before NEG: -|-2| * 1 + 0 = -2.
        self.assertConsole(mix(0, 0b011, negate=1, absolute=1)(0xc000, 0x3c00, 0), 0xc0000000)
        # The smallest f16 denormal is kept: 2^-24 * 1 + 0.
        self.assertConsole(mix(0, 0b111)(0x0001, 0x3c00, 0), 0x33800000)
        # CLAMP: 4 * 2 is 1, and no alternative keeps 8.
        clamped = mix(0, 0b011, clamp=True)(0x4400, 0x4000, 0)
        self.assertConsole(clamped, 0x3f800000)
        self.assertFalse(clamped.matches(0x41000000))
        self.assertConsole(mix(0, 0b011)(0x7c00, 0, 0), QUIET)

    def test_known_faults_are_modelled_for_their_hosts(self):
        label = "v_mul_legacy_f32_e64 v{d}, v4, v5 div:2"
        self.assertEqual(known_for("1002:13c0", label), [])
        [(name, model)] = known_for("10de:2b85", label)
        self.assertEqual(name, "omod-in-product")
        # 2 MAX is infinite on the console before OMOD halves it; with the 0.5
        # moved into the product it is MAX.
        self.assertEqual(model(0x7f7fffff, 0x40000000, 0), 0x7f7fffff)
        self.assertFalse(result(label, 0x7f7fffff, 0x40000000).matches(0x7f7fffff))
        self.assertEqual(model(0x3fc00000, 0xff7fffff, 0), 0xff3fffff)
        # Elsewhere the fault and the console agree.
        self.assertTrue(result(label, 0x40000000, 0x40400000).matches(model(0x40000000, 0x40400000, 0)))
        for fault in KNOWN.values():
            self.assertTrue(set(fault["forms"]) <= set(FORM))

    def test_programs_fit_the_check(self):
        chunks = list(chunks_of(FORMS))
        self.assertEqual(sum(map(len, chunks)), len(FORMS))
        self.assertEqual(len({form[0] for form in FORMS}), len(FORMS))
        for chunk in chunks:
            self.assertLessEqual(len(chunk), OUTPUTS)
            self.assertEqual(len({form[1] for form in chunk}), 1)
            self.assertIn("v_readfirstlane_b32 s20, v4", program(chunk))
        self.assertIsInstance(result("v_fract_f32 v{d}, s20", 0x3f800000), Open)


if __name__ == "__main__":
    unittest.main()
