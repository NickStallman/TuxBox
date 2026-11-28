#!/usr/bin/env python3
"""Check translated pixel attribute interpolation with GPU readback.

Synthetic pixel programs read two vertex attributes with v_interp_p1_f32 and
v_interp_p2_f32, with v_interp_mov_f32, and as half floats with
v_interp_p1ll_f16 and v_interp_p2_f16, and export the bits they got. A vertex
shader gives the attributes values that are known at every pixel of a 16 by 16
target. No game data is needed.
"""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from tools.frosty_gpu_guest import run_gpu_guest

SIDE = 16
# A rasterizer interpolates in its own precision, so floats are compared by value.
TOLERANCE = 1e-4
# One triangle covers the target. `u` and `v` are the pixel centre's position,
# from 0 to 1; `w` differs at each vertex, so the first vertex's value and the
# two differences can be told apart.
VERTEX = """struct V { float4 p : SV_Position; float4 a : TEXCOORD0; float4 b : TEXCOORD1; };
V main(uint id : SV_VertexID) {
    V o;
    float2 xy = float2(id == 2 ? 3 : -1, id == 1 ? 3 : -1);
    o.p = float4(xy, 0.5, 1);
    o.a = float4(0.375, (xy.x + 1) * 0.5, 0, 0);
    o.b = float4(0, 0, (1 - xy.y) * 0.5, 2.5 + id);
    return o;
}
"""


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def number(bits):
    return struct.unpack("<f", struct.pack("<I", bits))[0]


def wanted(variant, x, y):
    """What each exported word must be, as (kind, value): a float, or exact bits under a mask."""
    u, v = (x + 0.5) / SIDE, (y + 0.5) / SIDE
    if variant == "smooth":
        # `w` is 2.5, 3.5 and 4.5 at the vertices, so it follows both barycentrics.
        # The second vertex's weight is (1 - v) / 2 and the third's u / 2.
        return [("float", 0.375), ("float", u), ("float", v), ("float", 2.5 + (1 - v) / 2 + u)]
    if variant == "flat":
        return [("float", 0.375), ("float", 2.5), ("float", 1.0), ("float", 2.0)]
    # The low half is 0.375 as an f16; the manual leaves the high half alone.
    return [("low", 0x3600), ("bits", 0), ("bits", 0), ("bits", 0)]


def agrees(kind, value, got):
    if kind == "float":
        return abs(number(got) - value) <= TOLERANCE
    return (got & 0xffff if kind == "low" else got) == value


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    for name in ("emitter", "provider", "proton", "prefix", "compiler-dir", "dxc", "dxv", "work", "output"):
        ap.add_argument("--" + name, type=Path, required=True)
    ap.add_argument("--adapter", required=True)
    a = ap.parse_args(argv)
    if a.work.exists() or a.output.exists():
        ap.error("fresh work/output required")
    a.work.mkdir(parents=True)
    variants = ("smooth", "flat", "half")
    rows, shaders = [], {}
    with TemporaryDirectory(dir=a.work) as tmp:
        trial = Path(tmp)
        sources = {"vertex": VERTEX}
        for variant in variants:
            sources[variant] = subprocess.check_output([a.emitter, variant], text=True, timeout=15)
        for name, source in sources.items():
            hlsl = trial / (name + ".hlsl")
            hlsl.write_text(source)
            blob = trial / (name + ".cso")
            subprocess.run([a.dxc, "-T", "vs_6_0" if name == "vertex" else "ps_6_6", "-E", "main", "-O3", "-Fo", blob, hlsl],
                           check=True, timeout=30)
            subprocess.run([a.dxv, blob], check=True, timeout=30)
            shaders[name] = {"hlsl_sha256": sha(hlsl), "dxil_sha256": sha(blob)}
        # The draw-and-read-back guest of the pixel quad check, with these shaders.
        guest = (ROOT / "tools/reference/straight-pixel-quads/main.cpp").read_text()
        old = 'for (const char* shader : {"copy", "inplace"}) {'
        assert guest.count(old) == 1 and guest.count('"../independent-shaders/context.h"') == 1
        guest = guest.replace(old, "for (const char* shader : {" + ", ".join(f'"{v}"' for v in variants) + "}) {")
        guest = guest.replace('"../independent-shaders/context.h"',
                              '"' + str(ROOT / "tools/reference/independent-shaders/context.h") + '"')
        (trial / "guest.cpp").write_text(guest)
        exe, status, fields = run_gpu_guest(
            trial / "guest.cpp", "pixel-interp.exe", trial, a.work, a.proton, a.prefix, a.provider, a.compiler_dir,
            extra_env={"TUXBOX_NULL_PIPELINES": "0"})
        for variant in variants:
            path = trial / (variant + "_full.bin")
            data = path.read_bytes() if path.is_file() else b""
            row = {"case": variant, "matched": len(data) == SIDE * SIDE * 16}
            for index in range(SIDE * SIDE if row["matched"] else 0):
                x, y = index % SIDE, index // SIDE
                got = struct.unpack_from("<4I", data, index * 16)
                want = wanted(variant, x, y)
                if not all(agrees(kind, value, word) for (kind, value), word in zip(want, got)):
                    row.update(matched=False, first_mismatch_pixel=[x, y], expected=[value for _, value in want],
                               actual=[number(word) if kind == "float" else word for (kind, _), word in zip(want, got)])
                    break
            rows.append(row)
        passed = status == 0 and fields.get("adapter") == a.adapter.lower() and all(row["matched"] for row in rows)
        report = dict(schema=1, scope="Synthetic pixel attribute interpolation: v_interp_p1_f32, v_interp_p2_f32, "
                                      "v_interp_mov_f32, v_interp_p1ll_f16 and v_interp_p2_f16",
                      passed=passed, adapter=fields.get("adapter"), guest_exit=status, cases=len(rows),
                      pixels=SIDE * SIDE * len(rows), tolerance=TOLERANCE, guest_sha256=sha(exe), shaders=shaders, rows=rows)
        if fields.get("error"):
            report["error"] = fields["error"]
        with a.output.open("x") as stream:
            json.dump(report, stream, indent=2)
            stream.write("\n")
    for row in rows:
        if not row["matched"]:
            print(f"WRONG {row}")
    print(f"{'PASS' if passed else 'FAIL'}: {len(rows)} cases, {SIDE * SIDE * len(rows)} pixels, "
          f"{sum(not row['matched'] for row in rows)} wrong {report.get('error', '')}")
    return int(not passed)


if __name__ == "__main__":
    raise SystemExit(main())
