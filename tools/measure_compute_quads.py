#!/usr/bin/env python3
"""Measure on a real GPU where the threads of a compute group sit in a wave.

A console compute program takes implicit derivatives across the four lanes of a
quad, lanes 4k to 4k+3. Which threads those are depends on how the hardware
places a group's threads in a wave, which matters for groups with rows. Each
shader here records, for every thread, its lane, its wave, the threads in the
neighbouring lanes, and (in a second variant) the derivatives of its own
coordinates, for several group shapes and wave sizes.

This measures and prints; it has no expected result. A shader the driver
refuses (a wave size it lacks, or derivatives in compute) is listed as such.
No game data and no translator output is involved.
"""
import argparse
import json
from pathlib import Path
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))

SHAPES = ((8, 8), (16, 4), (4, 16), (64, 1), (8, 4), (16, 2), (32, 1))
WAVES = (None, 64, 32)
GROUPS = 4
WORDS = 12  # three 16-byte records for each thread
NONE = 0xffffffff


def hlsl(x, y, wave, derivatives):
    """One thread's record: lane, lane count, first thread of its wave, the threads
    one, two and three lanes away within its quad, then the derivatives of x and y."""
    size = f"[WaveSize({wave})]\n" if wave else ""
    if derivatives:
        slopes = ("asuint(ddx_fine(float(gtid.x)))", "asuint(ddx_fine(float(gtid.y)))",
                  "asuint(ddy_fine(float(gtid.x)))", "asuint(ddy_fine(float(gtid.y)))")
    else:
        slopes = (f"{NONE:#x}u",) * 4
    stores = ["lane", "WaveGetLaneCount()", "WaveActiveMin(thread)",
              *(f"WaveReadLaneAt(thread, lane ^ {n}u)" for n in (1, 2, 3)), *slopes, "0u", "0u"]
    body = "".join(f"    tb_out.Store(at + {4 * n}u, {value});\n" for n, value in enumerate(stores))
    return (f"ByteAddressBuffer tb_in : register(t0);\nRWByteAddressBuffer tb_out : register(u0);\n"
            f"{size}[numthreads({x}, {y}, 1)]\n"
            f"void main(uint3 gid : SV_GroupID, uint3 gtid : SV_GroupThreadID) {{\n"
            f"    uint thread = gid.x * {x * y}u + gtid.y * {x}u + gtid.x;\n"
            f"    uint lane = WaveGetLaneIndex();\n"
            f"    uint at = thread * {WORDS * 4}u;\n{body}}}\n")


def cases():
    for x, y in SHAPES:
        for wave in WAVES:
            for derivatives in (False, True):
                name = f"g{x}x{y}_w{wave or 'any'}_{'slopes' if derivatives else 'lanes'}"
                yield name, x, y, wave, derivatives


def quad_order(x, y, tx, ty):
    """The index of thread (tx, ty) if threads are numbered two-by-two block by block."""
    return ((ty // 2) * (x // 2) + tx // 2) * 4 + (ty & 1) * 2 + (tx & 1)


def describe(x, y, words):
    """What one case's readback shows. `words[t]` is thread t's record."""
    threads = x * y
    rows = [words[t] for t in range(GROUPS * threads)]
    counts = sorted({r[1] for r in rows})
    report = {"lane_counts": counts}
    width = counts[0] if len(counts) == 1 else None

    def holds(lane_of):
        return bool(width) and all(r[0] == lane_of(t % threads) % width for t, r in enumerate(rows))

    orders = {"row by row": lambda t: t,
              "two-by-two blocks": lambda t: quad_order(x, y, t % x, t // x) if x % 2 == 0 and y % 2 == 0 else -1}
    report["thread_order"] = [name for name, order in orders.items() if holds(order)] or ["neither tested order"]
    report["waves_per_group"] = sorted({len({rows[g * threads + t][2] for t in range(threads)}) for g in range(GROUPS)})
    report["same_in_every_group"] = all(rows[t][0] == rows[t % threads][0] for t in range(len(rows)))
    # The first group, as a grid of lanes, and the threads of each of its quads.
    report["lanes_of_group_0"] = [[rows[ty * x + tx][0] for tx in range(x)] for ty in range(y)]
    quads = {}
    for t in range(threads):
        quads.setdefault((rows[t][2], rows[t][0] // 4), []).append((rows[t][0] & 3, t % x, t // x))
    report["quads_of_group_0"] = [[(tx, ty) for _, tx, ty in sorted(members)] for _, members in sorted(quads.items())]
    if rows[0][6] != NONE:
        slope = lambda bits: struct.unpack("<f", struct.pack("<I", bits))[0]
        # A fine derivative is the difference across the quad: it names the partner.
        agree = {"ddx": 0, "ddy": 0}
        for t in range(threads):
            for name, lane_step, at in (("ddx", 3, 6), ("ddy", 4, 8)):
                partner = rows[t][lane_step] % threads
                step = (abs(partner % x - t % x), abs(partner // x - t // x))
                agree[name] += step == (abs(slope(rows[t][at])), abs(slope(rows[t][at + 1])))
        report["ddx_partner_is_one_lane_away"] = f"{agree['ddx']} of {threads} threads"
        report["ddy_partner_is_two_lanes_away"] = f"{agree['ddy']} of {threads} threads"
        report["slopes_of_group_0"] = sorted({tuple(slope(rows[t][n]) for n in range(6, 10)) for t in range(threads)})
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("dxc", "dxv", "provider", "proton", "prefix", "compiler-dir", "work", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--adapter", required=True)
    args = parser.parse_args(argv)
    from check_translator_gpu import select_adapter
    from frosty_gpu_guest import run_gpu_guest
    # The guest opens the default adapter, so expose only the one asked for.
    device = select_adapter(args.adapter)
    if not device:
        parser.error(f"vulkaninfo lists no device {args.adapter}")
    print(f"adapter {args.adapter.lower()}: {device}", flush=True)
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=work) as tmp:
        trial = Path(tmp)
        (trial / "one.input").write_bytes(bytes(16))
        listed, refused_by_dxc = [], {}
        for name, x, y, wave, derivatives in cases():
            source, blob = trial / f"{name}.hlsl", trial / f"{name}.cso"
            source.write_text(hlsl(x, y, wave, derivatives))
            built = subprocess.run([args.dxc, "-T", "cs_6_6", "-E", "main", "-Fo", blob, source],
                                   timeout=60, capture_output=True, text=True)
            if built.returncode or subprocess.run([args.dxv, blob], timeout=60, capture_output=True).returncode:
                refused_by_dxc[name] = built.stderr.strip().splitlines()[-1:] or ["validation failed"]
                continue
            listed.append((name, x, y))
        (trial / "cases.txt").write_text("".join(
            f"{name} {name} one.input {GROUPS} {GROUPS * x * y * WORDS // 4}\n" for name, x, y in listed))
        exe, status, fields = run_gpu_guest(
            ROOT / "tools/reference/independent-shaders/wave_pairs.cpp", "wave-pairs.exe", trial, work,
            args.proton.resolve(), args.prefix, args.provider, args.compiler_dir)
        results = {}
        for name, x, y in listed:
            path = trial / f"{name}.bin"
            count = GROUPS * x * y * WORDS
            data = path.read_bytes() if path.is_file() else b""
            if len(data) != count * 4:
                results[name] = {"not_run": fields.get(f"{name}_pipeline", "no readback")}
                continue
            flat = struct.unpack(f"<{count}I", data)
            results[name] = describe(x, y, [flat[t * WORDS:(t + 1) * WORDS] for t in range(GROUPS * x * y)])
    report = dict(scope="Measured placement of compute threads in waves and quads; no expected result",
                  adapter=fields.get("adapter"), wanted_adapter=args.adapter.lower(), guest_exit=status,
                  refused_by_dxc=refused_by_dxc, cases=results)
    if fields.get("error"):
        report["error"] = fields["error"]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"adapter {fields.get('adapter')}, guest exit {status}")
    for name, result in results.items():
        if "not_run" in result:
            print(f"{name}: not run ({result['not_run']})")
            continue
        line = (f"{name}: {result['lane_counts']} lanes, threads {', '.join(result['thread_order'])}, "
                f"{result['waves_per_group']} waves per group")
        if "ddx_partner_is_one_lane_away" in result:
            line += (f"; ddx partner one lane away for {result['ddx_partner_is_one_lane_away']}, "
                     f"ddy partner two lanes away for {result['ddy_partner_is_two_lanes_away']}")
        print(line)
    for name, why in refused_by_dxc.items():
        print(f"{name}: DXC refused it ({why[0]})")
    print(f"full report: {args.output}")
    if fields.get("adapter") != args.adapter.lower():
        print(f"WRONG ADAPTER: asked for {args.adapter}, the guest ran on {fields.get('adapter')}")
        return 1
    return 0 if status == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
