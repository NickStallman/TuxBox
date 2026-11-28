from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from project_tuxbox.persistent_dxc import PersistentDxc, build_helper, session_helper, staged


@unittest.skipUnless(sys.platform == "linux", "The DXC helper uses Linux pipes")
class PersistentDxcTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.dxc = self.root / "bin/dxc"
        self.dxc.parent.mkdir()
        self.dxc.touch()
        library = self.root / "lib/libdxcompiler.so"
        library.parent.mkdir()
        library.touch()
        self.source = self.root / "source.hlsl"
        self.source.write_bytes(b"synthetic HLSL")
        self.output = self.root / "out.cso"
        self.report = []

    def helper(self, body):
        executable = self.root / "helper"
        executable.write_text(f"#!{sys.executable}\n" + body)
        executable.chmod(0o700)
        return executable

    def test_replaces_crashed_helper_and_retries_before_reusing_it(self):
        helper = self.helper(
            """import os, sys
from pathlib import Path
for line in sys.stdin:
    source = Path(line.strip())
    output = Path(sys.stdin.readline().strip())
    request = (sys.stdin.readline(), sys.stdin.readline())
    marker = source.with_suffix(".crashed")
    if not marker.exists():
        marker.touch()
        output.write_bytes(b"partial DXIL")
        os._exit(3)
    assert not output.exists()
    output.write_bytes(f"{os.getpid()}:".encode() + source.read_bytes())
    print("ok", flush=True)
"""
        )
        with PersistentDxc(helper, 1, self.report.append) as pool:
            self.assertTrue(pool.compile(self.dxc, self.source, self.output, "cs_6_0"))
            first = self.output.read_bytes()
            pid = int(first.split(b":")[0])
            self.assertTrue(first.endswith(self.source.read_bytes()))
            self.output.unlink()
            self.assertTrue(pool.compile(self.dxc, self.source, self.output, "cs_6_0"))
            self.assertEqual(self.output.read_bytes(), first)
        self.assertFalse(Path(f"/proc/{pid}").exists())
        self.assertEqual(self.report, [])

    def test_repeated_crash_leaves_no_partial_dxil_for_cli(self):
        helper = self.helper(
            """import os, sys
from pathlib import Path
source = Path(sys.stdin.readline().strip())
output = Path(sys.stdin.readline().strip())
sys.stdin.readline(), sys.stdin.readline()
assert not output.exists()
with source.with_suffix(".attempts").open("ab") as log:
    log.write(b"x")
output.write_bytes(b"partial DXIL")
os._exit(3)
"""
        )
        with PersistentDxc(helper, 1, self.report.append) as pool:
            self.assertFalse(pool.compile(self.dxc, self.source, self.output, "cs_6_0"))
        self.assertFalse(self.output.exists())
        self.assertEqual(self.source.with_suffix(".attempts").read_bytes(), b"xx")
        self.assertEqual(len(self.report), 1)

    def test_refused_program_goes_to_the_cli_and_the_helper_serves_on(self):
        helper = self.helper(
            """import os, sys
from pathlib import Path
while source := sys.stdin.readline():
    output = Path(sys.stdin.readline().strip())
    target, flags = sys.stdin.readline().strip(), sys.stdin.readline().strip()
    if target == "ps_6_6":
        output.write_bytes(b"partial DXIL")
        print("no", flush=True)
    else:
        output.write_text(f"{os.getpid()} {target} {flags}")
        print("ok", flush=True)
"""
        )
        with PersistentDxc(helper, 1, self.report.append) as pool:
            self.assertTrue(pool.compile(self.dxc, self.source, self.output, "cs_6_6", ("-O1", "-flag")))
            pid, rest = self.output.read_text().split(" ", 1)
            self.assertEqual(rest, "cs_6_6 -O1 -flag")
            self.assertFalse(pool.compile(self.dxc, self.source, self.output, "ps_6_6"))
            self.assertFalse(self.output.exists())
            self.assertTrue(pool.compile(self.dxc, self.source, self.output, "ms_6_6"))
            self.assertEqual(self.output.read_text(), f"{pid} ms_6_6 ")
        self.assertEqual(len(self.report), 1)

    def test_slow_compile_times_out_like_the_cli(self):
        helper = self.helper(
            """import sys, time
for _ in range(4): sys.stdin.readline()
time.sleep(60)
"""
        )
        with PersistentDxc(helper, 1, self.report.append) as pool:
            with self.assertRaises(subprocess.TimeoutExpired):
                pool.compile(self.dxc, self.source, self.output, "cs_6_6", timeout=0.3)
        self.assertFalse(self.output.exists())

    def release(self, version):
        """A stand-in DXC release whose dxc reports `version`."""
        dxc = self.root / "release/bin/dxc"
        for path in (dxc, self.root / "release/lib/libdxcompiler.so", self.root / "release/include/dxc/dxcapi.h"):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.touch()
        dxc.write_text(f"#!/bin/sh\necho 'libdxcompiler.so: 1.9(1)({version})'\n")
        dxc.chmod(0o700)
        return dxc

    def test_the_launcher_builds_the_helper_unless_told_otherwise(self):
        with patch("project_tuxbox.persistent_dxc.build_helper", return_value="built") as build:
            self.assertEqual(session_helper({}, self.report.append), "built")
            self.assertEqual(session_helper({"TUXBOX_PERSISTENT_DXC": "/mine"}, self.report.append), "/mine")
            self.assertIsNone(session_helper({"TUXBOX_PERSISTENT_DXC": "0"}, self.report.append))
        self.assertEqual(build.call_count, 1)

    def test_builds_once_for_a_verified_release(self):
        dxc, output = self.release("1.9.2609.5"), self.root / "helpers"

        def compiler(args, **_):
            Path(args[-1]).write_bytes(b"helper")
            return subprocess.CompletedProcess(args, 0, "", "")

        with patch("project_tuxbox.persistent_dxc.shutil.which", return_value="c++"), \
                patch("project_tuxbox.persistent_dxc.subprocess.run", side_effect=compiler) as run, \
                patch("project_tuxbox.persistent_dxc.subprocess.check_output", return_value=b"(1.9.2609.5)"):
            first = build_helper(self.report.append, dxc, output)
            self.assertEqual(build_helper(self.report.append, dxc, output), first)
        self.assertEqual(Path(first).read_bytes(), b"helper")
        self.assertEqual(run.call_count, 1)
        self.assertEqual(self.report, [])

    def test_says_why_it_leaves_the_work_to_the_command_line(self):
        output = self.root / "helpers"
        self.assertIsNone(build_helper(self.report.append, self.release("1.8.2505.1"), output))
        self.assertIsNone(build_helper(self.report.append, self.dxc.parent / "missing/bin/dxc", output))
        dxc = self.release("1.9.2609.5")
        with patch("project_tuxbox.persistent_dxc.shutil.which", return_value=None):
            self.assertIsNone(build_helper(self.report.append, dxc, output))
        with patch("project_tuxbox.persistent_dxc.shutil.which", return_value="false"):
            self.assertIsNone(build_helper(self.report.append, dxc, output))
        for message, reason in zip(self.report, ("only with DXC 1.9.2609", "whole Microsoft DXC release",
                                                 "C++ compiler", "did not build"), strict=True):
            self.assertIn(reason, message)

    def test_staged_pool_runs_a_private_copy_or_none(self):
        helper = self.helper("")
        with TemporaryDirectory() as directory:
            with staged(helper, directory, 1, self.report.append) as pool:
                self.assertEqual(pool.helper.parent, Path(directory))
                self.assertEqual(pool.helper.read_bytes(), helper.read_bytes())
            with staged(None, directory, 1, self.report.append) as pool:
                self.assertIsNone(pool)
            with staged(self.root / "missing", directory, 1, self.report.append) as pool:
                self.assertIsNone(pool)
        self.assertEqual(len(self.report), 1)
