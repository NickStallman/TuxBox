from contextlib import contextmanager
import hashlib
from pathlib import Path
import re
import struct
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
import zlib
from unittest import mock

from examples.lanes import shader_fixture
from project_tuxbox.shader_prep import SOURCE_CACHE
from project_tuxbox.verify import BASE_SUITES, LANES_SCENARIOS, lanes_compute_heap_stride


def parts(data):
    count = struct.unpack_from("<I", data, 28)[0]
    found = {}
    for start in struct.unpack_from(f"<{count}I", data, 32):
        size = struct.unpack_from("<I", data, start + 4)[0]
        found[data[start:start + 4]] = data[start + 8:start + 8 + size]
    return found


def fssm(dxil):
    at = dxil.index(b"XBCH")
    compressed = struct.unpack_from("<Q", dxil, at + 8)[0]
    return zlib.decompress(dxil[at + 24:at + 24 + compressed])


def rows(program, offset_field, count_field):
    start = struct.unpack_from("<Q", program, offset_field)[0]
    count = struct.unpack_from("<I", program, count_field)[0]
    return [list(struct.unpack_from("<5I", program, 16 + start + 20 * n)) for n in range(count)]


class ComputeHeapStrideFixtureTests(unittest.TestCase):
    def test_program_is_rootless_and_names_the_heap_base(self):
        data = shader_fixture.heap_stride_compute_shader()
        self.assertEqual(struct.unpack_from("<I", data, 24)[0], len(data))
        found = parts(data)
        self.assertNotIn(b"RTS0", found)
        self.assertEqual(found[b"SFI0"], struct.pack("<Q", 1 << 25))
        self.assertEqual(struct.unpack_from("<I", found[b"DXIL"], 0)[0] >> 16, 5)
        program = fssm(found[b"DXIL"])
        self.assertEqual(struct.unpack_from("<I", program, 8)[0], len(program) - 16)
        # One class-5 user record: the resource heap base (s2).
        self.assertEqual(rows(program, 0xe8, 0xf8), [[5, 0, 1, 0, 2]])
        # No used-resource rows: the program picks its descriptors by heap index.
        self.assertEqual(rows(program, 0xf0, 0xfc), [])
        psv = found[b"PSV0"]
        self.assertEqual(struct.unpack_from("<3I", psv, 4 + 36), (64, 1, 1))

    def test_header_carries_the_program_and_the_layout_constants(self):
        data = shader_fixture.heap_stride_compute_shader()
        text = shader_fixture.header()
        values = re.search(r"compute_heap_stride_shader\[\] = \{([0-9,]+)\};", text).group(1)
        self.assertEqual(bytes(int(value) for value in values.split(",")), data)
        self.assertIn(f'compute_heap_stride_shader_digest[] = L"{hashlib.sha256(data).hexdigest()}"', text)
        self.assertIn("compute_heap_stride_input_slot = 100u, compute_heap_stride_output_slot = 110u;", text)
        self.assertIn("compute_heap_stride_bytes = 32u, compute_heap_stride_records = 64u;", text)


class ComputeHeapStrideGateTests(unittest.TestCase):
    def run_gate(self, output, compiled, refused=None):
        key = hashlib.sha256(shader_fixture.heap_stride_compute_shader()).hexdigest()
        with TemporaryDirectory() as prefix:
            sources = Path(prefix) / SOURCE_CACHE
            runtime = sources.parent / "runtime/fingerprint"
            runtime.mkdir(parents=True)
            sources.mkdir()
            stale = [sources / f"{key}.cso", runtime / f"{key}.unsupported"]
            kept = sources / "title.cso"
            for path in (*stale, kept):
                path.write_bytes(b"")
            env = {"WINEPREFIX": prefix, "TUXBOX_SHADER_SESSION": "1", "TUXBOX_LOG": "stderr",
                   "TUXBOX_NULL_PIPELINES": "1"}
            seen = {}

            @contextmanager
            def session(cache, session_env, report):
                self.assertEqual(cache, sources.parent)
                seen["env"] = dict(session_env)
                yield
                for k in compiled:
                    (runtime / f"{k}.cso").write_bytes(b"")
                    report(f"Compiled Xbox shader {k}")
                for k, reason in (refused or []):
                    (runtime / f"{k}.unsupported").write_bytes(b"")
                    report(f"Xbox shader {k}: xshader failed (65): {reason}")

            def check(name, binary, arguments, match, log_check=None, **_):
                seen["name"], seen["arguments"] = name, arguments
                seen["stale"] = [path.exists() for path in stale]
                seen["problem"] = log_check
                return {}

            with mock.patch("project_tuxbox.runtime_shaders.shader_session", session):
                lanes_compute_heap_stride(SimpleNamespace(env=env), check, ["--adapter", "10de:2c02"], "10de:2c02", ())
            seen["left"] = sorted(path.name for path in sources.parent.rglob("*") if path.is_file())
            seen["env_after"] = env
            seen["problem"] = seen["problem"](output)
            return seen, key

    def test_retry_passes_and_clean_entries(self):
        bound = "bound paged resource descriptor stride table at compute root slot 1\n"
        seen, _ = self.run_gate(bound, ["a" * 64])
        self.assertEqual(seen["name"], "lanes-compute-heap-stride")
        self.assertEqual(seen["arguments"][:2], ["--selftest", "compute-heap-stride"])
        self.assertEqual(seen["stale"], [False, False])
        self.assertNotIn("TUXBOX_SHADER_SESSION", seen["env"])
        self.assertEqual(seen["env"]["TUXBOX_DESCRIPTOR_HEAP_LIMIT"], "64")
        self.assertEqual(seen["env"]["TUXBOX_NULL_PIPELINES"], "0")
        self.assertIsNone(seen["problem"])
        self.assertEqual(seen["left"], ["title.cso"])
        self.assertNotIn("TUXBOX_DESCRIPTOR_HEAP_LIMIT", seen["env_after"])
        self.assertEqual(seen["env_after"]["TUXBOX_NULL_PIPELINES"], "1")

    def test_no_bind_or_stale_entries_fail(self):
        bound = "bound paged resource descriptor stride table at compute root slot 1\n"
        seen, key = self.run_gate(bound, ["a" * 64])
        self.assertIn("did not bind", self.run_gate("", ["a" * 64])[0]["problem"])
        self.assertIn("translated nothing", self.run_gate(bound, [])[0]["problem"])
        # The fixture must not be translated against the bare root: that is
        # the retry, not the ordinary translation.
        self.assertIn("without its root", self.run_gate(bound, [key])[0]["problem"])
        self.assertIn("stand-in", self.run_gate("substituted null compute pipeline\n", ["a" * 64])[0]["problem"])
        self.assertIn("stand-in", self.run_gate(bound + "substituted null compute pipeline\n", ["a" * 64])[0]["problem"])

    def test_refused_program_fails(self):
        key = hashlib.sha256(shader_fixture.heap_stride_compute_shader()).hexdigest()
        seen, _ = self.run_gate(
            "bound paged resource descriptor stride table at compute root slot 1\n",
            [], [(key, "raw buffer IDXEN needs the console descriptor stride")])
        self.assertIn("without its root",
                      self.run_gate("bound paged resource descriptor stride table at compute root slot 1\n",
                                    [key])[0]["problem"])
        self.assertIn("stand-in",
                      self.run_gate("substituted null compute pipeline\n", [key], [])[0]["problem"])

    def test_selftest_is_not_in_the_basic_scenarios(self):
        self.assertNotIn("compute-heap-stride", LANES_SCENARIOS)
        self.assertIn("lanes-compute-heap-stride", BASE_SUITES)


if __name__ == "__main__":
    unittest.main()
