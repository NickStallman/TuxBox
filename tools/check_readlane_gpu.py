#!/usr/bin/env python3
"""Run synthetic compute v_readlane_b32 programs on a real GPU and compare against RDNA2 semantics.

Covers constant-lane Wave32/Wave64 broadcasts, masked-EXEC and branch-skipped lanes,
Wave32 multi-wave isolation, Wave32 v_mbcnt_lo_u32_b32(exec_lo) ballot preservation,
uniform loops coexisting with SGPR spills, and dynamic SGPR GROUP_READ fallback with
wave-width masking. Mixed register cases exercise an EXEC-independent lane write,
a first-active-lane selector and later vector use. No game data is required.
"""
import argparse
import json
from pathlib import Path
import random
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from project_tuxbox.inspect import digest

VIEW = 96  # first word of the compute.cpp UAV view (byte offset 384)
WORDS = 384

# (case_name, group_lanes, wave_width, target_lane, skip_last_lane, mode)
CASES = [
    ("wave32_active_0", 32, 32, 0, False, "broadcast"),
    ("wave32_active_15", 32, 32, 15, False, "broadcast"),
    ("wave32_active_31", 32, 32, 31, False, "broadcast"),
    ("wave32_masked_31", 32, 32, 31, False, "broadcast"),
    ("wave32_masked_branch_31", 32, 32, 31, True, "broadcast"),
    ("wave32_64t_masked_31", 64, 32, 31, False, "broadcast"),
    ("wave64_64t_lane31", 64, 64, 31, False, "broadcast"),
    ("wave64_64t_lane47", 64, 64, 47, False, "broadcast"),
    ("wave64_64t_masked_lane63", 64, 64, 63, False, "broadcast"),
    ("wave64_128t_lane31", 128, 64, 31, False, "inplace"),
    ("wave64_128t_lane63", 128, 64, 63, False, "inplace"),
    ("wave64_64t_spill_and_loop_lane31", 64, 64, 31, False, "plus10"),
    ("wave32_ctl_dep_guard_31", 32, 32, 31, True, "broadcast"),
    ("wave32_64t_and_exec_31", 64, 32, 31, False, "broadcast"),
    ("wave32_mbcnt_exec_31", 32, 32, 31, False, "mbcnt"),
    # s7 is one register for the wave, so lane 63 reads it too though it skipped the read.
    ("wave64_swizzle_branch_dyn_63", 64, 64, 63, False, "broadcast"),
    ("wave64_dyn_sgpr_high_bits_47", 64, 64, 47, False, "broadcast"),
    ("wave32_64t_dyn_sgpr_31", 64, 32, 31, False, "broadcast"),
    ("wave32_mixed_dynamic_11", 32, 32, 11, False, "mixed"),
    ("wave32_mixed_empty_exec", 32, 32, 11, False, "mixed_empty"),
    ("wave32_mixed_lds_dpp_11", 32, 32, 11, False, "mixed_lds_dpp"),
]


def seed_and_expected(lanes, wave_width, target_lane, skip_last, mode, values):
    seed = [0x76540000 + i for i in range(WORDS)]
    seed[VIEW:VIEW + lanes] = values
    expected = seed.copy()
    for lane in range(lanes):
        src = (lane & ~(wave_width - 1)) | target_lane
        val = values[src]
        if mode in ("mixed", "mixed_empty", "mixed_lds_dpp"):
            written = 0x5a5a31f2

            def v6(index):
                vector_value = written if mode != "mixed_empty" and index == target_lane else index + 17
                return (3 if mode == "mixed_empty" else written) ^ vector_value
            if mode == "mixed_lds_dpp":
                # Group memory and a quad permute both give the neighbour's v6.
                other = v6(lane ^ 1)
                expected[VIEW + lanes + lane] = (v6(lane) ^ (other << 1) ^ (other << 2)) & 0xFFFFFFFF
            else:
                expected[VIEW + lanes + lane] = v6(lane)
        elif mode == "mbcnt":
            prefix = lane if lane < 31 else 31
            out = ((prefix & 0xff) << 24) | (val & 0x00ffffff)
            expected[VIEW + lanes + lane] = out
        elif mode == "plus10":
            expected[VIEW + lanes + lane] = (val + 10) & 0xffffffff
        elif mode == "inplace":
            expected[VIEW + lane] = val
        else:
            if skip_last and lane == lanes - 1:
                expected[VIEW + lanes + lane] = 0
            else:
                expected[VIEW + lanes + lane] = val
    return struct.pack(f"<{WORDS}I", *seed), struct.pack(f"<{WORDS}I", *expected)


def parse_receipt(path):
    fields = {}
    if path.is_file():
        for line in path.read_text(errors="replace").splitlines():
            if "=" in line:
                k, v = line.split("=", 1)
                fields[k.strip()] = v.strip()
    return fields


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--emitter", type=Path, required=True)
    ap.add_argument("--dxc", type=Path, required=True)
    ap.add_argument("--dxv", type=Path)
    ap.add_argument("--provider", type=Path)
    ap.add_argument("--proton", type=Path)
    ap.add_argument("--prefix", type=Path)
    ap.add_argument("--compiler-dir", type=Path)
    ap.add_argument("--native-exe", type=Path, help="Prebuilt compute.exe for native Windows D3D12 execution")
    ap.add_argument("--adapter", required=True)
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--no-assert-wave", action="store_true")
    ap.add_argument("--only", action="append", help="Restrict to named case(s)")
    a = ap.parse_args(argv)

    work = a.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    selected = [c for c in CASES if not a.only or c[0] in a.only]
    if not selected:
        ap.error("no matching cases selected")

    rng = random.Random(0x52454144)
    rows = []
    expected = {}
    with TemporaryDirectory(dir=work) as tmp:
        trial = Path(tmp)
        dispatched = []
        for name, lanes, wave_width, target_lane, skip_last, mode in selected:
            cmd = [str(a.emitter.resolve()), name]
            if a.no_assert_wave:
                cmd.append("--no-assert-wave")
            emit = subprocess.run(cmd, text=True, capture_output=True, timeout=30)
            if emit.returncode != 0:
                lines = [l.strip() for l in (emit.stderr or emit.stdout).splitlines()
                         if l.strip() and "RUST_BACKTRACE" not in l]
                msg = next((l for l in reversed(lines) if "panicked at" not in l), lines[-1] if lines else "emit failed")
                rows.append(dict(case=name, lanes=lanes, wave_width=wave_width, matched=False, error=msg[:200]))
                continue
            hlsl = trial / f"{name}.hlsl"
            hlsl.write_text(emit.stdout, encoding="utf-8")
            blob = trial / f"{name}.cso"
            profile = "cs_6_6" if "[WaveSize(" in emit.stdout else "cs_6_0"
            subprocess.run([str(a.dxc.resolve()), "-T", profile, "-E", "main", "-O3", "-Fo", str(blob), str(hlsl)],
                           check=True, capture_output=True, timeout=30)
            if a.dxv and a.dxv.is_file():
                subprocess.run([str(a.dxv.resolve()), str(blob)], check=True, capture_output=True, timeout=30)
            values = [rng.getrandbits(32) for _ in range(lanes)]
            seed_bytes, exp_bytes = seed_and_expected(lanes, wave_width, target_lane, skip_last, mode, values)
            (trial / f"{name}.seed").write_bytes(seed_bytes)
            expected[name] = exp_bytes
            if "[WaveSize(64)]" in emit.stdout:
                (trial / f"{name}.minwave").write_text("64\n")
            dispatched.append(name)
            rows.append(dict(case=name, lanes=lanes, wave_width=wave_width, profile=profile,
                             hlsl_sha256=digest(hlsl), dxil_sha256=digest(blob)))

        (trial / "cases.txt").write_text("".join(f"{name} 0 uint 1\n" for name in dispatched), encoding="utf-8")
        if a.native_exe:
            exe = a.native_exe.resolve()
            run = subprocess.run([str(exe)], cwd=trial, capture_output=True, text=True, timeout=60)
            status = run.returncode
            fields = parse_receipt(trial / "result.txt")
        else:
            from tools.frosty_gpu_guest import run_gpu_guest
            exe, status, fields = run_gpu_guest(
                ROOT / "tools/reference/independent-shaders/compute.cpp",
                "readlane-gpu.exe",
                trial, work, a.proton.resolve(), a.prefix, a.provider, a.compiler_dir,
                extra_env={"TUXBOX_NULL_PIPELINES": "0"},
            )

        # Only a host that reports narrower waves may leave the 64-lane programs out.
        narrow = int(fields.get("wave_lanes_max", 0)) in range(1, 64)
        for row in rows:
            name = row["case"]
            if name not in expected:
                continue
            if narrow and fields.get(f"{name}_skipped") == "64":
                row["skipped"] = "host has no 64-lane waves"
                continue
            path = trial / f"{name}.bin"
            actual = path.read_bytes() if path.is_file() else b""
            exp = expected[name]
            row["matched"] = actual == exp
            if row["matched"]:
                row["readback_sha256"] = digest(path)
            else:
                act_w = struct.unpack(f"<{len(actual) // 4}I", actual) if len(actual) == len(exp) else ()
                exp_w = struct.unpack(f"<{WORDS}I", exp)
                diffs = [i - VIEW for i in range(WORDS) if i < len(act_w) and act_w[i] != exp_w[i]]
                row["mismatched_words"] = len(diffs)
                row["first_mismatch"] = (
                    dict(word=diffs[0], actual=f"{act_w[VIEW + diffs[0]]:#010x}",
                         expected=f"{exp_w[VIEW + diffs[0]]:#010x}")
                    if diffs else "readback-size-mismatch"
                )

        adapter_ok = (a.adapter.lower() == "any") or (fields.get("adapter", "").lower() == a.adapter.lower())
        passed = status == 0 and adapter_ok and all(r.get("matched") or r.get("skipped") for r in rows)
        report = dict(
            schema=1,
            scope="Synthetic compute v_readlane_b32 programs translated to HLSL, compiled with DXC, and checked via D3D12 UAV readback",
            adapter=fields.get("adapter"),
            guest_exit=status,
            passed=passed,
            cases=len(rows),
            matched_cases=sum(1 for r in rows if r.get("matched")),
            compared_words=len(dispatched) * WORDS,
            emitter_sha256=digest(a.emitter.resolve()),
            guest_sha256=digest(exe),
            rows=rows,
        )
        if fields.get("error"):
            report["error"] = fields["error"]
        a.output.parent.mkdir(parents=True, exist_ok=True)
        a.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    failed = [r for r in rows if not r.get("matched") and not r.get("skipped")]
    skipped = sum(1 for r in rows if r.get("skipped"))
    note = f", {skipped} 64-lane cases not run (host has no 64-lane waves)" if skipped else ""
    print(f"{'PASS' if passed else 'FAIL'}: {report['matched_cases']}/{report['cases']} cases, "
          f"{report['compared_words']} words, adapter {report['adapter']}{note}")
    for r in failed:
        print(f"  FAIL {r['case']}: {r.get('error') or r.get('first_mismatch')} "
              f"(mismatched_words={r.get('mismatched_words', 'n/a')})")
    return int(not passed)


if __name__ == "__main__":
    raise SystemExit(main())
