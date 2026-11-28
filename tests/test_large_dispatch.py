import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
from tempfile import TemporaryDirectory
import unittest

from examples.lanes import shader_fixture
from project_tuxbox.verify import BASE_SUITES, GATE_LABELS


def with_group_base(data):
    """The fixture with the runtime's group base appended to its root.

    The fixture's root is version 1.0 with one table of one UAV, and RTS0 is
    its last part. The group base is three constants at b0, space32766.
    """
    at = data.index(b"RTS0")
    assert at + 8 + struct.unpack_from("<I", data, at + 4)[0] == len(data)
    root = struct.pack("<22I",
                       1, 2, 24, 0, 0, 0,     # version, parameters, their offset, no samplers, flags
                       0, 0, 48, 1, 0, 56,    # a table at 48, constants at 56
                       1, 68,                 # one range, at 68
                       0, 32766, 3,           # b0, space32766, three values
                       1, 1, 0, 0, 0)         # the range: one UAV, u0, space 0, offset 0
    out = bytearray(data[:at] + b"RTS0" + struct.pack("<I", len(root)) + root)
    struct.pack_into("<I", out, 24, len(out))
    return bytes(out)


class LargeDispatchTests(unittest.TestCase):
    def test_fixture_stores_its_group_id_through_one_table(self):
        data = shader_fixture.group_shader()
        self.assertEqual(struct.unpack_from("<I", data, 24)[0], len(data))
        self.assertIn(",".join(str(byte) for byte in data), shader_fixture.header())
        self.assertIn(f"group_shader_mark = {shader_fixture.GROUP_MARK:#x};", shader_fixture.header())
        self.assertIn("lanes-large-dispatch", BASE_SUITES)
        self.assertIn("lanes-large-dispatch", GATE_LABELS)

    def test_translation_against_the_group_base_adds_it_to_the_group_id(self):
        xshader = os.environ.get("TUXBOX_XSHADER") or shutil.which("xshader")
        dxc = os.environ.get("DXC") or shutil.which("dxc")
        if not (xshader and dxc):
            self.skipTest("set TUXBOX_XSHADER and DXC to translate the group ID program")
        with TemporaryDirectory() as directory:
            trial = Path(directory)
            hlsl = {}
            for name, data in (("plain", shader_fixture.group_shader()),
                               ("based", with_group_base(shader_fixture.group_shader()))):
                (trial / f"{name}.dxbc").write_bytes(data)
                result = subprocess.run([xshader, trial / f"{name}.dxbc", "--hlsl-out", trial / f"{name}.hlsl"],
                                        capture_output=True, text=True, timeout=30)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                hlsl[name] = (trial / f"{name}.hlsl").read_text()
            # The group ID still arrives in s5-s7: the constants take no launch register.
            self.assertIn("s5 = gid.x; s6 = gid.y; s7 = gid.z;", hlsl["plain"])
            based = hlsl["based"]
            self.assertIn("void main(uint3 tb_launch_gid : SV_GroupID, uint3 gtid : SV_GroupThreadID) {\n"
                          "    uint3 gid = tb_launch_gid + tb_group_base;\n", based)
            # Otherwise the translation is the same.
            undone = (based.replace("cbuffer TBGroupBase : register(b0, space32766) { uint3 tb_group_base; };\n", "")
                      .replace("\n    uint3 gid = tb_launch_gid + tb_group_base;", "")
                      .replace("tb_launch_gid", "gid"))
            self.assertEqual(undone, hlsl["plain"])
            result = subprocess.run([dxc, "-T", "cs_6_0", "-E", "main", "-Fc", trial / "based.ll",
                                     "-Fo", trial / "based.cso", trial / "based.hlsl"],
                                    capture_output=True, text=True, timeout=60)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertRegex((trial / "based.ll").read_text(), r"TBGroupBase\s+cbuffer\s+NA\s+NA\s+CB0\s+cb0,space32766\s+1")


if __name__ == "__main__":
    unittest.main()
