#!/usr/bin/env python3
"""Check on a real GPU that translated per-key lane counts match the console.

The synthetic program is the loop ray binning uses: each pass takes the key of
the first lane still to serve, counts the wave's lanes that hold it with a
64-bit sum, and has one lane add that count to a uint buffer. The translation
has every counting lane add 1 at its own key instead, so each counter must end
with the number of lanes that hold its key, whatever the host's wave width.
Keys are random, with repeats, keys no lane holds and lanes that do not count.
The programs are 64-lane ones: a 32-lane program keeps the wave together for
the loop's lane reads instead, and check_lockstep_gpu.py covers that form.
Only the translator's input is instrumented. No game data is needed.
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
sys.path.insert(0, str(ROOT / "tools"))
from frosty_gpu_guest import run_gpu_guest  # noqa: E402
from project_tuxbox.inspect import digest  # noqa: E402

# The guest's buffer, and the uint view the shader sees as u0.
WORDS, FIRST, VIEW = 384, 96, 192
COUNTERS, KEYS = 32, 32  # view words: counters from 0, lane keys from 32
NO_COUNT, MASKED = 0xffff, 3
# (console wave width, threads in a group, where the counting mask is made, lanes that enter)
PROGRAMS = {
    "wave64": ("64", 64, "inside", "all"),
    "wave64-masked": ("64", 64, "before", "masked"),
    "wave64-two-waves": ("64", 128, "before", "masked"),
}
START = "    v0 = gtid.x; v1 = gtid.y; v2 = gtid.z;"


def instrument(hlsl):
    if hlsl.count(START) != 1 or hlsl.count("InterlockedAdd(utb0[v15], v14, ") != 1:
        raise ValueError("unexpected key-count translation")
    if "RWBuffer<uint> utb0 : register(u0);" not in hlsl or "Wave" in hlsl[hlsl.index("void main("):]:
        raise ValueError("the translation is not one add per lane on a uint buffer at u0")
    return hlsl.replace(START, START + f" v3 = utb0[{KEYS}u + gtid.x];")


def cases():
    """(name, program, groups, seed) of every trial."""
    rng = random.Random(0x4b455953)
    for program, (_, threads, _, _) in PROGRAMS.items():
        for trial, (groups, spread) in enumerate(((1, 4), (1, COUNTERS), (3, 9), (2, 1))):
            seed = [rng.getrandbits(32) for _ in range(WORDS)]
            for lane in range(threads):
                key = rng.randrange(spread) if rng.random() > 0.2 else rng.choice((NO_COUNT, MASKED))
                seed[FIRST + KEYS + lane] = key
            yield f"{program}-{trial}", program, groups, seed


def expected(program, groups, seed):
    _, threads, _, lanes = PROGRAMS[program]
    want = list(seed)
    for lane in range(threads):
        key = seed[FIRST + KEYS + lane]
        if key != NO_COUNT and not (lanes == "masked" and key == MASKED):
            want[FIRST + key] = (want[FIRST + key] + groups) & 0xffffffff
    return want


def prepare(trial, emitter, dxc, dxv):
    """Translate, compile and seed every case in `trial`."""
    shaders = {}
    for name, (width, threads, valid, lanes) in PROGRAMS.items():
        emitted = subprocess.check_output([emitter, width, str(threads), valid, lanes], text=True, timeout=30)
        source = trial / f"{name}.hlsl"
        source.write_text(instrument(emitted))
        blob = trial / f"{name}.cso"
        subprocess.run([dxc, "-T", "cs_6_0", "-E", "main", "-Fo", blob, source], check=True, timeout=30,
                       capture_output=True)
        subprocess.run([dxv, blob], check=True, timeout=30, capture_output=True)
        shaders[name] = dict(hlsl_sha256=digest(source), dxil_sha256=digest(blob))
    trials = {}
    for name, program, groups, seed in cases():
        trials[name] = expected(program, groups, seed)
        (trial / f"{name}.seed").write_bytes(struct.pack(f"<{WORDS}I", *seed))
        (trial / f"{name}.cso").hardlink_to(trial / f"{program}.cso")
        with (trial / "cases.txt").open("a") as listing:
            listing.write(f"{name} 0 uint {groups}\n")
    return shaders, trials


def difference(want, data):
    if len(data) != 4 * WORDS:
        return "no readback"
    for i, (got, wanted) in enumerate(zip(struct.unpack(f"<{WORDS}I", data), want)):
        if got != wanted:
            return dict(word=i, got=got, wanted=wanted)
    return None


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
        shaders, trials = prepare(trial, args.emitter.resolve(), args.dxc, args.dxv)
        exe, status, fields = run_gpu_guest(
            ROOT / "tools/reference/independent-shaders/compute.cpp", "key-counts.exe", trial, work,
            args.proton.resolve(), args.prefix, args.provider, args.compiler_dir)
        wrong = {}
        for name, want in trials.items():
            path = trial / f"{name}.bin"
            found = difference(want, path.read_bytes() if path.is_file() else b"")
            if found:
                wrong[name] = found
        passed = status == 0 and fields.get("adapter") == args.adapter and not wrong
        report = dict(passed=passed, scope="Synthetic per-key lane counts added to a uint UAV",
                      adapter=fields.get("adapter"), wave_lanes_max=fields.get("wave_lanes_max"),
                      guest_exit=status, cases=len(trials), compared_words=len(trials) * WORDS, wrong=wrong,
                      shaders=shaders, emitter_sha256=digest(args.emitter), guest_sha256=digest(exe))
        if fields.get("error"):
            report["error"] = fields["error"]
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    for name, detail in wrong.items():
        print(f"WRONG {name}: {detail}")
    if not passed:
        print(f"FAIL: {len(wrong)} of {len(trials)} cases, guest exit {status}")
        return 1
    print(f"PASS: {len(trials)} cases, {len(trials) * WORDS} words, adapter {fields['adapter']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
