from pathlib import Path
import struct
import tempfile
import unittest

from project_tuxbox.vkd3d_cache import HEADER, merge_pipeline_cache

EXE = Path("title.exe")


def header(build=1):
    return b"VKS" + bytes([build]) + bytes(HEADER - 4)


def entry(key, size=13, kind=2):
    return struct.pack("<QQII", key, 0, size, kind) + bytes([key]) * (size + 7 & ~7)


class MergePipelineCache(unittest.TestCase):
    def merge(self, cache, written):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "vkd3d-proton.title.exe.cache"
            pending = Path(tmp) / "vkd3d-proton.title.exe.cache.write"
            if cache is not None:
                path.write_bytes(cache)
            if written is not None:
                pending.write_bytes(written)
            reports = []
            merge_pipeline_cache({"VKD3D_SHADER_CACHE_PATH": tmp}, EXE, reports.append)
            return (path.read_bytes() if path.exists() else None), pending.exists(), reports

    def test_new_entries_are_appended_once(self):
        result, pending, reports = self.merge(header() + entry(1), header() + entry(1) + entry(2) + entry(2))
        self.assertEqual(result, header() + entry(1) + entry(2))
        self.assertFalse(pending)
        self.assertIn("added 1 pipelines", reports[0])

    def test_a_torn_tail_is_replaced_by_the_new_entries(self):
        result, _, _ = self.merge(header() + entry(1) + entry(2)[:30], header() + entry(3) + entry(4)[:30])
        self.assertEqual(result, header() + entry(1) + entry(3))

    def test_a_cache_from_another_build_is_replaced(self):
        result, _, _ = self.merge(header(1) + entry(1), header(2) + entry(2))
        self.assertEqual(result, header(2) + entry(2))
        self.assertEqual(self.merge(None, header() + entry(5))[0], header() + entry(5))

    def test_nothing_to_add_leaves_both_files(self):
        for written in (None, b"", b"junk" * 40, header() + entry(1)):
            result, pending, reports = self.merge(header() + entry(1), written)
            self.assertEqual((result, pending, reports), (header() + entry(1), written is not None, []))

    def test_a_disabled_or_windows_cache_path_is_left_alone(self):
        for env in ({}, {"VKD3D_SHADER_CACHE_PATH": "0"}, {"VKD3D_SHADER_CACHE_PATH": "C:\\cache"}):
            merge_pipeline_cache(env, EXE, self.fail)


if __name__ == "__main__":
    unittest.main()
