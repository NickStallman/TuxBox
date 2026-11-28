import unittest

from tools.check_float_compares_gpu import (ITEMS, KNOWN, OUTPUTS, S25, chunks_of, float_class, items, known_for, mnemonics,
                                            model, modified, program, relation, value)

NAN, SIGNALLING, ONE, TWO = 0x7fc00000, 0xff800001, 0x3f800000, 0x40000000


class FloatCompareReferenceTests(unittest.TestCase):
    # Hand-computed values from the ISA's predicate definitions, independent of the model's formulas.
    def test_ordered_and_unordered_predicates(self):
        nan, one = value(NAN, False), value(ONE, False)
        # LG is ordered: false on NaN. NEQ and the N forms are true on NaN.
        self.assertFalse(relation("lg", nan, one))
        self.assertTrue(relation("neq", nan, one))
        self.assertTrue(relation("lg", one, 2.0))
        for name in ("nge", "nlg", "ngt", "nle", "nlt", "u"):
            self.assertTrue(relation(name, nan, one), name)
            self.assertTrue(relation(name, one, nan), name)
        for name in ("lt", "eq", "le", "gt", "ge", "lg", "o"):
            self.assertFalse(relation(name, nan, one), name)
        self.assertTrue(relation("tru", nan, nan))
        self.assertFalse(relation("f", one, one))
        self.assertTrue(relation("nlt", 2.0, one))
        self.assertFalse(relation("nlt", 0.5, one))

    def test_signed_zeros_and_denormals(self):
        plus, minus = value(0x00000000, False), value(0x80000000, False)
        self.assertTrue(relation("eq", plus, minus))
        self.assertFalse(relation("lg", plus, minus))
        self.assertFalse(relation("lt", minus, plus))
        # A flushed denormal equals zero; a kept one does not.
        self.assertTrue(relation("eq", value(0x00000001, False), minus))
        self.assertFalse(relation("eq", value(0x00000001, True), minus))
        self.assertTrue(relation("lt", value(0x807fffff, True), plus))

    def test_classes(self):
        classes = {SIGNALLING: 0, NAN: 1, 0xff800000: 2, 0xbf800000: 3, 0x807fffff: 4, 0x80000000: 5,
                   0x00000000: 6, 0x00000001: 7, ONE: 8, 0x7f800000: 9}
        for bits, number in classes.items():
            self.assertEqual(float_class(bits), number, hex(bits))

    def test_source_modifiers(self):
        self.assertEqual(modified("-|v1|", ONE), 0xbf800000)
        self.assertEqual(modified("-|v1|", 0xbf800000), 0xbf800000)
        self.assertEqual(modified("|v0|", 0x80000000), 0)
        self.assertEqual(modified("-v0", 0x80000000), 0)

    def test_cmpx_narrows_exec_and_keeps_vcc(self):
        # Lanes 0-2 active. Lane 1 compares NaN and leaves EXEC; lane 3 is
        # inactive and its v0 would pass. VCC holds the odd active lanes.
        a = [ONE, NAN, ONE, ONE] + [0] * 28
        b = [TWO] * 32
        out = model([("v_cmpx_lt_f32", "e32")], 32, [True, True, True] + [False] * 29, a, b)
        # bit | branch taken by some lane << 1 | lanes below << 8 | count << 16
        self.assertEqual(out[10][:4], [0x20003, 0x20102, 0x20103, 0])
        self.assertEqual(out[18][:4], [0x10000, 0x10001, 0x10100, 0])
        self.assertEqual(out[19][:4], [S25, S25, S25, 0])

    def test_compare_writes_vcc_and_an_empty_exec_writes_nothing(self):
        a, b = [ONE] * 64, [TWO] * 64
        out = model([("v_cmp_gt_f32", "e32")], 64, [False] + [True] * 63, a, b)
        self.assertEqual(out[10][:2], [0, 0])
        # The compare is false everywhere, so VCC no longer holds the odd lanes.
        self.assertEqual(out[18][1], 0)
        out = model([("v_cmpx_tru_f32", "e64")], 64, [False] * 64, a, b)
        self.assertEqual({word for words in out.values() for word in words}, {0})

    def test_scalar_and_constant_sources(self):
        # The scalar source is the first active lane's v0.
        a, b = [TWO, ONE] + [0] * 30, [ONE] * 32
        out = model([("v_cmp_eq_f32", "scalar")], 32, [False, True] + [True] * 30, a, b)
        self.assertEqual(out[10][1] & 1, 1)
        out = model([("v_cmp_gt_f32", "abs-const"), ("v_cmp_class_f32", "literal")], 32, [True] * 32,
                    [0xbf800000] * 32, [0] * 32)
        self.assertEqual(out[10][0] & 1, 1)
        # 0x2c3 selects classes 0, 1, 6, 7 and 9, not negative normals.
        self.assertEqual(out[11][0] & 1, 0)

    def test_known_translation_faults(self):
        # The per-lane skip: lane 1 left EXEC, so it misses the scalar written behind the branch.
        a, b = [ONE, NAN, ONE, ONE] + [0] * 28, [TWO] * 32
        active = [True, True, True] + [False] * 29
        out = model([("v_cmpx_lt_f32", "e32")], 32, active, a, b, frozenset({"execz"}))
        self.assertEqual(out[10][:4], [0x20003, 0x20100, 0x20103, 0])
        # A wave32 VOP3 compare into s24 leaves s25 zero; a VOPC one does not.
        self.assertEqual(model([("v_cmp_lt_f32", "e64")], 32, active, a, b, frozenset({"s25"}))[19][0], 0)
        self.assertEqual(model([("v_cmp_lt_f32", "e32")], 32, active, a, b, frozenset({"s25"}))[19][0], S25)
        self.assertEqual(known_for("64-paired"), frozenset())
        self.assertEqual(known_for("32"), frozenset(KNOWN))

    def test_programs_hold_every_pair_once(self):
        chunks = list(chunks_of(items()))
        self.assertEqual(sorted(pair for chunk in chunks for pair in chunk), sorted(items()))
        self.assertEqual({name for name, _ in items()}, set(mnemonics()))
        for chunk in chunks:
            self.assertLessEqual(len(chunk), ITEMS)
            self.assertEqual(len({"_class_" in name for name, _ in chunk}), 1)
            for width in (32, 64):
                lines = [line for line in program(chunk, width) if isinstance(line, str)]
                # Outputs stay in the registers the instrument stores.
                written = {int(line.split()[1].strip(",")[1:]) for line in lines
                           if line.startswith(("v_cndmask", "v_mov", "v_or", "v_lshl_or")) and line.split()[1][0] == "v"}
                self.assertLessEqual(written - {4}, set(OUTPUTS[width]))


if __name__ == "__main__":
    unittest.main()
