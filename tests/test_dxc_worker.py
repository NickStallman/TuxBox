"""The DXC helper must record what the command line records about a program."""
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "tools/dxc_worker.cpp"
HLSL = """RWByteAddressBuffer output : register(u0);
[WaveSize(%d)]
[numthreads(64, 1, 1)] void main(uint3 id : SV_DispatchThreadID) {
    output.Store(4 * id.x, WaveActiveSum(id.x) + WaveGetLaneCount());
}
"""


def tools():
    """(compiler, Microsoft DXC release directory), or None without both."""
    compiler, dxc = shutil.which("c++"), os.environ.get("DXC")
    if not compiler or not dxc:
        return None
    release = Path(dxc).resolve().parent.parent
    complete = (release / "include/dxc/dxcapi.h").is_file() and (release / "lib/libdxcompiler.so").is_file()
    return (compiler, release) if complete else None


def parts(path):
    data = path.read_bytes()
    count = struct.unpack_from("<I", data, 28)[0]
    return {data[at:at + 4]: data[at + 8:at + 8 + struct.unpack_from("<I", data, at + 4)[0]]
            for at in struct.unpack_from(f"<{count}I", data, 32)}


@unittest.skipUnless(sys.platform == "linux" and tools(), "needs a C++ compiler and DXC set to a Linux release")
class DxcWorkerTests(unittest.TestCase):
    def test_keeps_the_wave_size_a_program_requires(self):
        compiler, release = tools()
        environment = dict(os.environ, LD_LIBRARY_PATH=str(release / "lib"))
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            worker = root / "worker"
            subprocess.run([compiler, "-std=c++17", "-O1", "-isystem", release / "include", SOURCE,
                            "-L", release / "lib", "-ldxcompiler", "-o", worker], check=True, timeout=180)
            requests, outputs = [], []
            for lanes in (32, 64):
                source = root / f"wave{lanes}.hlsl"
                source.write_text(HLSL % lanes)
                helped, direct = root / f"helper{lanes}.cso", root / f"dxc{lanes}.cso"
                requests.append(f"{source}\n{helped}\ncs_6_6\n-O1\n")
                outputs.append((helped, direct))
                subprocess.run([release / "bin/dxc", "-T", "cs_6_6", "-E", "main", "-O1", "-Fo", direct, source],
                               check=True, env=environment, timeout=60)
            run = subprocess.run([worker], input="".join(requests).encode(), capture_output=True,
                                 env=environment, timeout=60)
            self.assertEqual(run.stdout, b"ok\nok\n", run.stderr)
            for helped, direct in outputs:
                # The runtime reads the group's wave width from this part.
                self.assertEqual(parts(helped)[b"PSV0"], parts(direct)[b"PSV0"], helped.name)
                listing = subprocess.run([release / "bin/dxc", "-dumpbin", helped], capture_output=True, text=True,
                                         env=environment, timeout=60).stdout
                self.assertIn(f"WaveSize={helped.stem.removeprefix('helper')}", listing)


if __name__ == "__main__":
    unittest.main()
