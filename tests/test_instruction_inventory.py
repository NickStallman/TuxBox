"""docs/INSTRUCTIONS.md is generated, and says no more than its inputs show."""
import unittest

from project_tuxbox import instruction_inventory as inventory

RECORD = """Vop2 0x3 v_add_f32 | yes
Vop2 0x4 v_sub_f32 | yes
Vinterp 0x0 v_interp_p1_f32 | v_interp outside a pixel shader
Mubuf 0x0 buffer_load_format_x | mubuf: s0 not a buffer descriptor (None)
Mubuf 0x1 buffer_load_format_xy | mubuf: s0 not a buffer descriptor (None)
Sopp 0x0 s_nop | nothing
Sop1 0x1e s_setpc_b64 | nothing (Vertex only)
Ds 0x1 ds_sub_u32 | unsupported encoding
"""
CHECKS = {"adapters": {"AMD 1002:0001": {"instructions": ["v_add_f32"]},
                       "NVIDIA 10de:0001": {"instructions": ["v_add_f32", "buffer_load_format_x"]}}}
USE = {"title": "Example 1.0", "translations": 1000,
       "instructions": {"v_add_f32": 900, "v_sub_f32": 40, "buffer_load_format_x": 7, "tb_helper": 3}}


class Inventory(unittest.TestCase):
    def setUp(self):
        self.text = inventory.render(RECORD, CHECKS, USE)

    def test_each_instruction_gets_the_state_its_evidence_supports(self):
        for line in (
            "| `v_add_f32` | 0x3 | Yes | AMD 1002:0001, NVIDIA 10de:0001 | 900 |",
            "| `v_sub_f32` | 0x4 | Yes | — | 40 |",
            # Alone it lacks a descriptor; real shaders that hold it translate.
            "| `buffer_load_format_x` | 0x0 | Yes, in recorded shaders | NVIDIA 10de:0001 | 7 |",
            # Nothing shows this one translating.
            "| `buffer_load_format_xy` | 0x1 | Unproven: mubuf: s0 not a buffer descriptor (None) | — | — |",
            "| `v_interp_p1_f32` | 0x0 | Unproven: v_interp outside a pixel shader | — | — |",
            "| `s_nop` | 0x0 | Nothing to emit | — | — |",
            "| `s_setpc_b64` | 0x1e | Nothing to emit | — | — |",
            "| `ds_sub_u32` | 0x1 | **No** | — | — |",
        ):
            self.assertIn(line, self.text)

    def test_the_summary_counts_what_the_tables_show(self):
        self.assertIn("- 8 instructions in 8 encodings.", self.text)
        self.assertIn("- 3 are used by the recorded shaders; a GPU check runs 2 of those.", self.text)
        self.assertIn("- 1 encodings have no translation, and 2 are unproven", self.text)
        # Used and unchecked, most used first; the translator's own helpers aren't instructions.
        gaps = self.text.split("## Used without a GPU check")[1].split("## ")[0]
        self.assertIn("| `v_sub_f32` | 40 |", gaps)
        self.assertNotIn("v_add_f32", gaps)
        self.assertNotIn("tb_helper", self.text)

    def test_the_document_matches_its_inputs(self):
        self.assertEqual(inventory.DOCUMENT.read_text(encoding="utf-8"), inventory.current(),
                         "Regenerate it with python tools/shader_harness.py inventory")


if __name__ == "__main__":
    unittest.main()
