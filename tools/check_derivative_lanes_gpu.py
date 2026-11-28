#!/usr/bin/env python3
"""Check on a real GPU how a derivative-taking compute group numbers its lanes.

A synthetic console program samples at its own thread coordinates with an
implicit derivative, exchanges the thread's x within its quad (`ds_swizzle_b32`
in quad mode) and reads x from lane 5. In a group with rows the console's lanes
4k to 4k+3 are one two-by-two block of a row pair, so the exchange returns the
x of the thread diagonally opposite in the block and lane 5 is thread (3, 0).

The guest binds no descriptor heap. The check replaces the heap texture with a
function that returns the derivatives of the coordinates it is given, which
also shows that the host differences two-by-two blocks. No game data is used.
"""
import argparse
import json
from pathlib import Path
import re
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
from frosty_gpu_guest import run_gpu_guest  # noqa: E402
from project_tuxbox.inspect import digest  # noqa: E402

GROUPS = 64
# One 32-lane wave each, so a lane read means the same lane whichever wave width is assumed.
SHAPES = ((8, 4), (16, 2), (4, 8), (32, 1))
OUTPUTS = ("v10", "v11", "v12")
HEAP = ('struct TBHeapTexture { uint slot; float4 Sample(uint sampler, float2 at) { return float4('
        'ddx_fine(at.x) + 16.0 * ddy_fine(at.x) + 256.0 * ddx_fine(at.y) + 4096.0 * ddy_fine(at.y), 0.0, 0.0, 0.0); } };\n'
        'TBHeapTexture tb_heap_texture(uint slot) { TBHeapTexture texture; texture.slot = slot; return texture; }\n'
        'RWByteAddressBuffer tb_out : register(u0);\n')


def bits(value):
    return struct.unpack("<I", struct.pack("<f", float(value)))[0]


def lane_of(x, y, tx, ty):
    """The console lane of thread (tx, ty) in a derivative-taking group."""
    if y == 1:
        return tx
    return ((ty // 2) * (x // 2) + tx // 2) * 4 + (ty & 1) * 2 + (tx & 1)


def expected(x, y):
    """Each thread's sample, exchange and lane read, threads row by row."""
    threads = {lane_of(x, y, tx, ty): (tx, ty) for ty in range(y) for tx in range(x)}
    # Fine derivatives across the quad: x and y each step by one, or x by one and two in a row.
    slopes = bits(1 + 4096 if y > 1 else 1 + 16 * 2)
    rows = []
    for ty in range(y):
        for tx in range(x):
            lane = lane_of(x, y, tx, ty)
            # Quad mode 0x1b: lane positions 0, 1, 2, 3 read 3, 2, 1, 0.
            partner = threads[(lane & ~3) | (3 - (lane & 3))]
            wave = lane & ~63
            rows.append((slopes, partner[0], threads[wave + 5][0]))
    return rows


def instrument(hlsl, x, y):
    assert f"[numthreads({x}, {y}, 1)]" in hlsl and "ResourceDescriptorHeap" in hlsl, "unexpected translation"
    hlsl = re.sub(r"Texture2D<float4> (tb_heap_\w+);", r"TBHeapTexture \1;", hlsl)
    hlsl = re.sub(r"SamplerState (\w+) = SamplerDescriptorHeap\[[^;]*\];", r"uint \1 = 0u;", hlsl)
    hlsl = re.sub(r"ResourceDescriptorHeap\[([^\]]+)\]", r"tb_heap_texture(\1)", hlsl)
    assert "DescriptorHeap" not in hlsl and "Texture2D" not in hlsl, "unexpected heap access"
    index = f"((gid.x * {y}u + gtid.y) * {x}u + gtid.x)"
    stores = "".join(f"    tb_out.Store(({index} * 4u + {column}u) * 4u, {register});\n"
                     for column, register in enumerate(OUTPUTS))
    end = hlsl.rstrip().rfind("}")
    return HEAP + hlsl[:end] + stores + "}\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("emitter", "dxc", "dxv", "provider", "proton", "prefix", "compiler-dir", "work", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--adapter", required=True)
    args = parser.parse_args(argv)
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=work) as tmp:
        trial = Path(tmp)
        (trial / "one.input").write_bytes(bytes(16))
        shaders, cases = {}, []
        for x, y in SHAPES:
            name = f"lanes_{x}x{y}"
            emitted = subprocess.check_output([args.emitter.resolve(), f"{x}x{y}"], text=True, timeout=60)
            source, blob = trial / f"{name}.hlsl", trial / f"{name}.cso"
            source.write_text(instrument(emitted, x, y))
            subprocess.run([args.dxc, "-T", "cs_6_6", "-E", "main", "-Fo", blob, source], check=True, timeout=60,
                           capture_output=True)
            subprocess.run([args.dxv, blob], check=True, timeout=60, capture_output=True)
            shaders[name] = dict(hlsl_sha256=digest(source), dxil_sha256=digest(blob))
            cases.append(f"{name} {name} one.input {GROUPS} {GROUPS * x * y}\n")
        (trial / "cases.txt").write_text("".join(cases))
        exe, status, fields = run_gpu_guest(
            ROOT / "tools/reference/independent-shaders/wave_pairs.cpp", "wave-pairs.exe", trial, work,
            args.proton.resolve(), args.prefix, args.provider, args.compiler_dir)
        wrong = {}
        for x, y in SHAPES:
            name = f"lanes_{x}x{y}"
            path = trial / f"{name}.bin"
            data = path.read_bytes() if path.is_file() else b""
            if len(data) != GROUPS * x * y * 16:
                wrong[name] = fields.get(f"{name}_pipeline", "no readback")
                continue
            got = struct.unpack(f"<{GROUPS * x * y * 4}I", data)
            want = expected(x, y)
            for thread in range(GROUPS * x * y):
                row = got[thread * 4:thread * 4 + 3]
                if row != want[thread % (x * y)]:
                    place = thread % (x * y)
                    wrong[name] = dict(group=thread // (x * y), thread=(place % x, place // x),
                                       got=dict(zip(OUTPUTS, row)), wanted=dict(zip(OUTPUTS, want[place])))
                    break
        passed = status == 0 and fields.get("adapter") == args.adapter and not wrong
        report = dict(passed=passed, scope="Lane numbering of a synthetic derivative-taking compute group",
                      adapter=fields.get("adapter"), guest_exit=status, cases=len(SHAPES), wrong=wrong, shaders=shaders,
                      emitter_sha256=digest(args.emitter), guest_sha256=digest(exe))
        if fields.get("error"):
            report["error"] = fields["error"]
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    for name, detail in wrong.items():
        print(f"WRONG {name}: {detail}")
    if not passed:
        print(f"FAIL: {len(wrong)} of {len(SHAPES)} groups, guest exit {status}, adapter {fields.get('adapter')}")
        return 1
    print(f"PASS: {len(SHAPES)} groups, adapter {fields['adapter']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
