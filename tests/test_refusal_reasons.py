import hashlib
import os
from pathlib import Path
from tempfile import TemporaryDirectory
import sys
import unittest
from unittest.mock import patch

from project_tuxbox.runtime_shaders import REASON_BYTES, ShaderRefused, compile_request
from project_tuxbox.scarlett_report import refusal_bucket
from project_tuxbox.shader_prep import ToolFailure

REASON = "wave reduction at 0x35c may execute in divergent control flow"


@unittest.skipUnless(sys.platform == "linux", "Runtime shader compilation locks its cache with flock, on Proton on Linux")
class CachedRefusalReasons(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.cache = root / "cache"
        self.cache.mkdir()
        self.session = root / "session"
        self.session.mkdir()
        self.source = root / "request.dxbc"
        self.source.write_bytes(b"Xbox compute shader")
        self.key = hashlib.sha256(self.source.read_bytes()).hexdigest()
        self.reason = self.cache / (self.key + ".reason")
        self.stderr = f"note: translating\n{REASON}\n"

    def refuse(self):
        def run(args, *, timeout):
            self.assertEqual(args[0], "xshader")
            raise ToolFailure("xshader", 65, self.stderr)

        with patch("project_tuxbox.runtime_shaders.run_tool", side_effect=run) as tool:
            with self.assertRaises(ShaderRefused) as raised:
                compile_request(self.source, self.key, ("xshader", "dxc", "dxv"), self.cache, self.session)
        return raised.exception, tool.call_count

    def test_warm_refusal_keeps_the_emitter_reason(self):
        fresh, calls = self.refuse()
        self.assertEqual((fresh.cached, calls), (False, 1))
        # The provider reads this marker and expects exactly the key.
        self.assertEqual((self.cache / (self.key + ".unsupported")).read_text(), self.key)
        self.assertEqual(self.reason.read_text(), REASON)

        warm, calls = self.refuse()
        self.assertEqual((warm.cached, warm.remembered, calls), (True, True, 0))
        self.assertEqual(str(warm), REASON)
        self.assertEqual(refusal_bucket(warm), refusal_bucket(fresh))
        self.assertEqual(refusal_bucket(warm), "wave_divergent")

    def test_marker_without_reason_stays_unknown(self):
        self.refuse()
        self.reason.unlink()
        warm, calls = self.refuse()
        self.assertEqual((warm.cached, warm.remembered, calls), (True, False, 0))
        self.assertEqual(refusal_bucket(warm), "cached_reason_unavailable")

    def test_unusable_reason_files_are_ignored(self):
        self.refuse()
        self.reason.write_bytes(b"x" * (REASON_BYTES + 1))
        self.assertEqual(refusal_bucket(self.refuse()[0]), "cached_reason_unavailable")
        self.reason.unlink()
        elsewhere = self.cache / "elsewhere"
        elsewhere.write_text(REASON)
        os.symlink(elsewhere, self.reason)
        self.assertEqual(refusal_bucket(self.refuse()[0]), "cached_reason_unavailable")

    def test_long_reason_is_bounded(self):
        self.stderr = "s_load " + "x" * (2 * REASON_BYTES)
        self.refuse()
        self.assertEqual(len(self.reason.read_bytes()), REASON_BYTES)
        warm, _ = self.refuse()
        self.assertTrue(warm.remembered)


if __name__ == "__main__":
    unittest.main()


@unittest.skipUnless(sys.platform == "linux", "Runtime shader compilation locks its cache with flock, on Proton on Linux")
class StrideTableRetry(unittest.TestCase):
    """An IDXEN stride refusal is retried with the stride table appended."""

    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.cache = self.root / "cache"
        self.cache.mkdir()
        self.source = self.root / "request.dxbc"
        self.source.write_bytes(b"Xbox compute shader")
        self.key = hashlib.sha256(self.source.read_bytes()).hexdigest()
        self.variant_digest = "ab" * 32

    def _program(self):
        import struct
        bitcode = b"BC\xc0\xde"
        program = struct.pack("<II4sIII", 0x50060, 7, b"DXIL", 0x100, 16, len(bitcode)) + bitcode
        chunk = struct.pack("<4sI", b"DXIL", len(program)) + program
        return b"DXBC" + bytes(16) + struct.pack("<IIII", 1, 36 + len(chunk), 1, 36) + chunk

    def test_idxen_refusal_is_retried_with_stride_table(self):
        import subprocess
        import struct
        from project_tuxbox.runtime_shaders import compile_request

        result = self._program()
        tool_calls = []

        def run(args, *, timeout):
            # The publish path's DXC/DXV calls (the refusal's xshader call is
            # covered by the patched refusal below).
            tool_calls.append(args[0])
            if args[0] == "dxc":
                args[args.index("-Fo") + 1].write_bytes(result)
            elif args[0] != "xshader":
                self.assertEqual(args[0], "dxv")
                self.assertEqual(args[1].read_bytes(), result)

        def retry(cmd, **kwargs):
            # Reimplement the real CLI's outputs for the variant: HLSL and profile.
            Path(cmd[cmd.index("--hlsl-out") + 1]).write_text("void main() {}")
            Path(cmd[cmd.index("--profile-out") + 1]).write_text("cs_6_0")
            self.assertIn("--stride-table", cmd)
            return subprocess.CompletedProcess(cmd, 0, stdout=f"stride-digest={self.variant_digest}\n", stderr="")

        def refused(args, *, timeout):
            if args and args[0] == "xshader":
                raise ToolFailure("xshader", 65,
                                  "raw buffer IDXEN needs the console descriptor stride [buffer_load_dword @ 0x1]")
            return run(args, timeout=timeout)

        with patch("project_tuxbox.runtime_shaders.run_tool", side_effect=refused):
            with patch("project_tuxbox.runtime_shaders.subprocess.run", side_effect=retry):
                compiled = compile_request(self.source, self.key, ("xshader", "dxc", "dxv"), self.cache, None)

        self.assertTrue(compiled)
        # The variant cso is published under the variant digest.
        self.assertEqual((self.cache / (self.variant_digest + ".cso")).read_bytes(), result)
        # The original refusal markers stay, so the runtime's own retry sees them.
        self.assertEqual((self.cache / (self.key + ".unsupported")).read_text(), self.key)
        self.assertEqual((self.cache / (self.key + ".reason")).read_text(),
                         "raw buffer IDXEN needs the console descriptor stride [buffer_load_dword @ 0x1]")

    def test_refusal_without_stride_flag_publishes_refusal(self):
        import subprocess
        from project_tuxbox.runtime_shaders import compile_request

        def refused(args, *, timeout):
            if args and args[0] == "xshader":
                raise ToolFailure("xshader", 65, "wave reduction at 0x35c may execute in divergent control flow")
            return None

        with patch("project_tuxbox.runtime_shaders.run_tool", side_effect=refused):
            with patch("project_tuxbox.runtime_shaders.subprocess.run") as retry_mock:
                with self.assertRaises(ShaderRefused):
                    compile_request(self.source, self.key, ("xshader", "dxc", "dxv"), self.cache, None)
                retry_mock.assert_not_called()

    def test_retry_without_digest_marker_falls_back_to_refusal(self):
        import subprocess
        from project_tuxbox.runtime_shaders import compile_request

        def refused(args, *, timeout):
            if args and args[0] == "xshader":
                raise ToolFailure("xshader", 65,
                                  "raw buffer IDXEN needs the console descriptor stride [buffer_load_dword @ 0x1]")
            return None

        def retry(cmd, **kwargs):
            # A retry that succeeds at the CLI but reports no digest marker.
            Path(cmd[cmd.index("--hlsl-out") + 1]).write_text("void main() {}")
            Path(cmd[cmd.index("--profile-out") + 1]).write_text("cs_6_0")
            return subprocess.CompletedProcess(cmd, 0, stdout="entry={:?}\n", stderr="")

        with patch("project_tuxbox.runtime_shaders.run_tool", side_effect=refused):
            with patch("project_tuxbox.runtime_shaders.subprocess.run", side_effect=retry):
                with self.assertRaises(ShaderRefused):
                    compile_request(self.source, self.key, ("xshader", "dxc", "dxv"), self.cache, None)


if __name__ == "__main__":
    unittest.main()
