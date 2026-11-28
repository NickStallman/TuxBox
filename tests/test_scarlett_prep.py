from contextlib import nullcontext, redirect_stderr
import io
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from project_tuxbox.runtime_shaders import ShaderRefused
from project_tuxbox.scarlett_prep import announce_programs, prepare_scarlett


class ScarlettPreparation(unittest.TestCase):
    def test_ranks_new_refusals_and_keeps_cached_separate(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            exe = root / "game_scarlett_master.exe"
            exe.touch()
            exe.with_name("d3d12_xs.dll").touch()
            requests = root / "requests"
            requests.mkdir()
            for n in range(1, 8):
                (requests / (str(n) * 64 + ".dxbc")).touch()

            def compile_one(source, *args):
                n = int(source.stem[0])
                if n == 1:
                    return True
                if n == 2:
                    return False
                if n in (3, 4):
                    raise ShaderRefused(
                        f"xshader failed (65): s_load at 0x{n}0: dynamic soffset unsupported",
                        cached=False,
                    )
                if n == 5:
                    raise ShaderRefused("cached unsupported HLSL translation", cached=True)
                if n == 6:
                    raise ShaderRefused("xshader failed (65): unverified mesh shader bindings", cached=False)
                raise ValueError("invalid digest")

            stderr = io.StringIO()
            host = SimpleNamespace(env={"WINEPREFIX": str(root), "TUXBOX_PERSISTENT_DXC": "0"})
            with patch("project_tuxbox.scarlett_prep.scarlett_requests", return_value=requests), \
                    patch("project_tuxbox.scarlett_prep.snapshot_compiler_cache", return_value=(None, None)), \
                    patch("project_tuxbox.scarlett_prep.desktop_progress", return_value=nullcontext(None)), \
                    patch("project_tuxbox.scarlett_prep.compile_request", side_effect=compile_one) as compile_mock, \
                    redirect_stderr(stderr):
                self.assertTrue(prepare_scarlett(exe, host))
            log = stderr.getvalue()
            self.assertEqual(compile_mock.call_count, 7)
            self.assertIn("1 compiled, 1 cached, 4 unsupported, 1 failed", log)
            self.assertIn("ProjectScarlett: 2 shaders compiled · 4 unsupported · 1 failed", log)
            self.assertIn("3 actionable, 1 cached before reasons were kept.", log)
            self.assertIn("2 x s_load at 0x_: dynamic soffset unsupported", log)
            self.assertIn("1 x unverified mesh shader bindings", log)
            self.assertNotIn("x cached unsupported HLSL", log)
            self.assertLess(log.index("2 x s_load"), log.index("1 x unverified"))

    def test_an_ordinary_launch_still_gets_the_shader_total(self):
        with TemporaryDirectory() as tmp:
            exe = Path(tmp) / "game_scarlett_master.exe"
            exe.write_bytes(b"title")
            host = SimpleNamespace(env={"WINEPREFIX": tmp})
            announce_programs(exe, host)
            self.assertNotIn("TUXBOX_SHADER_PROGRAMS", host.env)
            exe.with_name("shaders.rpf").write_bytes(b"pack")
            with patch("project_tuxbox.scarlett_prep.count_programs", return_value=25889):
                announce_programs(exe, host)
            self.assertEqual(host.env["TUXBOX_SHADER_PROGRAMS"], "25889")

    def test_counts_programs_not_requests(self):
        def container(program, extra=b""):
            parts = [(b"DXIL", program)] + ([(b"RTS0", extra)] if extra else [])
            table = 32 + 4 * len(parts)
            body, offsets = b"", []
            for tag, data in parts:
                offsets.append(table + len(body))
                body += tag + len(data).to_bytes(4, "little") + data
            head = b"DXBC" + bytes(20) + (table + len(body)).to_bytes(4, "little") + len(parts).to_bytes(4, "little")
            return head + b"".join(o.to_bytes(4, "little") for o in offsets) + body

        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            exe = root / "game_scarlett_master.exe"
            exe.touch()
            exe.with_name("d3d12_xs.dll").touch()
            requests = root / "projectscarlett" / "requests"
            requests.mkdir(parents=True)
            # One program on three roots, one of which compiles, and one refused on both of its own.
            for n, (program, signature) in enumerate(
                    [(b"first", b"a"), (b"first", b"b"), (b"first", b""), (b"second", b"a"), (b"second", b"b")], 1):
                (requests / (str(n) * 64 + ".dxbc")).write_bytes(container(program, signature))

            def compile_one(source, *args):
                if source.stem[0] == "2":
                    return True
                raise ShaderRefused("xshader failed (65): unverified mesh shader bindings", cached=False)

            stderr = io.StringIO()
            host = SimpleNamespace(env={"WINEPREFIX": str(root), "TUXBOX_PERSISTENT_DXC": "0"})
            with patch("project_tuxbox.scarlett_prep.scarlett_requests", return_value=requests), \
                    patch("project_tuxbox.scarlett_prep.count_programs", return_value=25889), \
                    patch("project_tuxbox.scarlett_prep.snapshot_compiler_cache", return_value=(None, None)), \
                    patch("project_tuxbox.scarlett_prep.desktop_progress", return_value=nullcontext(None)), \
                    patch("project_tuxbox.scarlett_prep.compile_request", side_effect=compile_one), \
                    redirect_stderr(stderr):
                self.assertTrue(prepare_scarlett(exe, host))
            log = stderr.getvalue()
            self.assertIn("ProjectScarlett: 1 of 25,889 shaders compiled · 1 unsupported", log)
            self.assertIn("5/5 requests", log)
            self.assertEqual(host.env["TUXBOX_SHADER_PROGRAMS"], "25889")

    def test_reports_expected_mesh_refusals_separately(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            exe = root / "game_scarlett_master.exe"
            exe.touch()
            exe.with_name("d3d12_xs.dll").touch()
            requests = root / "requests"
            requests.mkdir()
            for n in (1, 2, 3):
                (requests / (str(n) * 64 + ".dxbc")).touch()

            def compile_one(source, *args):
                n = int(source.stem[0])
                if n == 1:
                    raise ShaderRefused("xshader failed (65): mesh root signature missing", cached=False)
                if n == 2:
                    raise ShaderRefused("xshader failed (65): unrecognized mesh SRV/CBV root-table layout", cached=False)
                raise ShaderRefused("xshader failed (65): unverified mesh launch register", cached=False)

            stderr = io.StringIO()
            host = SimpleNamespace(env={"WINEPREFIX": str(root), "TUXBOX_PERSISTENT_DXC": "0"})
            with patch("project_tuxbox.scarlett_prep.scarlett_requests", return_value=requests), \
                    patch("project_tuxbox.scarlett_prep.snapshot_compiler_cache", return_value=(None, None)), \
                    patch("project_tuxbox.scarlett_prep.desktop_progress", return_value=nullcontext(None)), \
                    patch("project_tuxbox.scarlett_prep.compile_request", side_effect=compile_one), \
                    redirect_stderr(stderr):
                self.assertTrue(prepare_scarlett(exe, host))
            log = stderr.getvalue()
            self.assertIn("1 actionable, 2 expected at pipeline creation (mesh_root_layout=2), 0 cached before reasons were kept.", log)
            self.assertIn("1 x unverified mesh launch register", log)
            self.assertNotIn("mesh root signature missing", log)
            self.assertNotIn("root-table layout", log)


class BoundedPreparation(unittest.TestCase):
    def test_bounded_queue_completes_every_request_once(self):
        from concurrent.futures import ThreadPoolExecutor
        from project_tuxbox.scarlett_prep import bounded_results
        seen=[]
        def source():
            for i in range(1000):
                seen.append(i)
                yield i
        with ThreadPoolExecutor(max_workers=3) as pool:
            out=bounded_results(pool,lambda n:n,source(),3)
            first=next(out)
            self.assertLessEqual(len(seen),7)
            self.assertCountEqual([first,*out],range(1000))
            self.assertEqual(len(seen),1000)

    def test_slow_request_does_not_block_fast_completion(self):
        from concurrent.futures import ThreadPoolExecutor
        from threading import Event
        from project_tuxbox.scarlett_prep import bounded_results
        release=Event()
        def compile_one(n):
            if n==0 and not release.wait(5):
                raise TimeoutError("test gate")
            return n
        with ThreadPoolExecutor(max_workers=2) as pool:
            out=bounded_results(pool,compile_one,range(20),2)
            try:
                first=next(out)
                self.assertNotEqual(first,0)
            finally:
                release.set()
            self.assertCountEqual([first,*out],range(20))

    def test_worker_failure_does_not_queue_remaining_archive(self):
        from concurrent.futures import ThreadPoolExecutor
        from project_tuxbox.scarlett_prep import bounded_results
        seen=[]
        def source():
            for i in range(100):
                seen.append(i)
                yield i
        def fail(_):
            raise RuntimeError("unexpected error")
        with ThreadPoolExecutor(max_workers=2) as pool:
            with self.assertRaisesRegex(RuntimeError,"unexpected error"):
                list(bounded_results(pool,fail,source(),2))
        self.assertEqual(len(seen),4)
