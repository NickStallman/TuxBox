#!/usr/bin/env python3
"""Check on a real GPU that translated image_atomic_cmpswap swaps and returns like RDNA2.

The synthetic programs run 32 lanes on a 4x4x4 uint 3D image. In one, every
lane ORs its own bit into the same texel in a compare-and-swap retry loop, as
ProjectScarlett's voxel boundary pass does: read the texel, swap with GLC and
retry while the returned value differs from the one compared. The texel must
end with every bit set, and the values returned by the successful swaps must
form one chain that adds one lane's bit at a time. In the other, each lane
swaps its own texel once, with a compare value that matches the texel or not.
A swap must store only when it matches and always return the texel's previous
value. Only the translator's input and output are instrumented. No game data is
needed.
"""
import argparse
import json
from pathlib import Path
import random
import re
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
from check_lockstep_gpu import branch, label, words_of  # noqa: E402
from frosty_gpu_guest import run_gpu_guest  # noqa: E402
from project_tuxbox.inspect import digest  # noqa: E402

LANES, TEXELS, WORDS = 32, 64, 96
# v4 is the value compared and v5 the value stored, from buffer words 0-31 and
# 32-63. The value each swap returns goes to words 64-95.
IMAGE = "s[8:15] dmask:0x3 dim:SQ_RSRC_IMG_3D unorm glc"
DESCRIPTOR = ["s_load_dwordx8 s[8:15], s[4:5], 0x0", "s_waitcnt lgkmcnt(0)"]
# Texel (1, 2, 3), addressed with non-sequential registers.
SHARED = 1 + 2 * 4 + 3 * 16
PROGRAMS = {
    "contention": [*DESCRIPTOR, "v_mov_b32_e32 v13, 1", "v_mov_b32_e32 v14, 2", "v_mov_b32_e32 v20, 3",
                   "v_lshlrev_b32_e64 v5, v0, 1",
                   "image_load v4, [v13, v14, v20], s[8:15] dmask:0x1 dim:SQ_RSRC_IMG_3D unorm glc",
                   "s_waitcnt vmcnt(0)", "s_mov_b32 s16, exec_lo", label("again"),
                   "v_or_b32_e32 v18, v4, v5", "v_mov_b32_e32 v19, v4",
                   f"image_atomic_cmpswap v[18:19], [v13, v14, v20], {IMAGE}", "s_waitcnt vmcnt(0)",
                   "v_cmp_ne_u32_e32 vcc_lo, v18, v4", "v_mov_b32_e32 v4, v18",
                   "s_and_b32 exec_lo, exec_lo, vcc_lo", branch("s_cbranch_execnz", "again"),
                   "s_mov_b32 exec_lo, s16", "s_endpgm"],
    # Lane i swaps texel i, addressed with sequential registers.
    "swap": [*DESCRIPTOR, "v_and_b32_e32 v13, 3, v0", "v_bfe_u32 v14, v0, 2, 2", "v_lshrrev_b32_e32 v15, 4, v0",
             "v_mov_b32_e32 v18, v5", "v_mov_b32_e32 v19, v4",
             f"image_atomic_cmpswap v[18:19], v[13:15], {IMAGE}", "s_waitcnt vmcnt(0)", "s_endpgm"],
}
START = "    v0 = gtid.x; v1 = gtid.y; v2 = gtid.z;"
END = re.compile(r"^    // 0x[0-9a-f]{4} s_endpgm$", re.MULTILINE)


def instrument(hlsl):
    if hlsl.count(START) != 1 or len(END.findall(hlsl)) != 1 or "InterlockedCompareExchange(uav0[" not in hlsl:
        raise ValueError("unexpected compare-swap fixture")
    if "RWTexture3D<uint> uav0 : register(u0);" not in hlsl:
        raise ValueError("the image is not a 3D uint UAV at u0")
    hlsl = hlsl.replace(START, START + f" v4 = output[gtid.x]; v5 = output[{LANES}u + gtid.x];")
    end = END.search(hlsl).start()
    hlsl = hlsl[:end] + f"    output[{2 * LANES}u + gtid.x] = v18;\n" + hlsl[end:]
    return "RWBuffer<uint> output : register(u1);\n" + hlsl


def cases():
    """(name, program, texels, compared, stored) of every trial."""
    rng = random.Random(0x434d5053)
    for i in range(4):
        texels = [rng.getrandbits(32) for _ in range(TEXELS)]
        texels[SHARED] = 0
        yield f"contention{i}", "contention", texels, [0] * LANES, [0] * LANES
    for i in range(3):
        texels = [rng.getrandbits(32) for _ in range(TEXELS)]
        stored = [rng.getrandbits(32) for _ in range(LANES)]
        flip = [1 << rng.randrange(32) for _ in range(LANES)]
        for kind, matches in (("swap", lambda lane: True), ("mismatch", lambda lane: False),
                              ("mixed", lambda lane: lane % 2 == 0)):
            compared = [texels[lane] ^ (0 if matches(lane) else flip[lane]) for lane in range(LANES)]
            yield f"{kind}{i}", "swap", texels, compared, stored


def expected(program, texels, compared, stored):
    """The texels and buffer after the program, or None where the order decides."""
    image, words = list(texels), [*compared, *stored, *([None] * LANES)]
    if program == "swap":
        for lane in range(LANES):
            words[2 * LANES + lane] = texels[lane]
            if compared[lane] == texels[lane]:
                image[lane] = stored[lane]
    else:
        image[SHARED] = (1 << LANES) - 1
    return image + words


def chain(returned):
    """Why the values the successful swaps returned are not one lane's bit at a time, or None."""
    owner = {value: lane for lane, value in enumerate(returned)}
    if len(owner) != LANES:
        return "two swaps returned the same value"
    value = 0
    for _ in range(LANES):
        if value not in owner:
            return f"no swap returned {value:#010x}"
        value |= 1 << owner[value]
    return None


def difference(program, want, data):
    if len(data) != 4 * (TEXELS + WORDS):
        return "no readback"
    actual = struct.unpack(f"<{TEXELS + WORDS}I", data)
    for i, (got, wanted) in enumerate(zip(actual, want)):
        if wanted is not None and got != wanted:
            where = f"texel {i}" if i < TEXELS else f"word {i - TEXELS}"
            return dict(at=where, got=got, wanted=wanted)
    if program == "contention":
        return chain(actual[TEXELS + 2 * LANES:])
    return None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("emitter", "llvm-mc", "dxc", "dxv", "provider", "proton", "prefix", "compiler-dir", "work",
                 "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--adapter", required=True)
    args = parser.parse_args(argv)
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=work) as tmp:
        trial = Path(tmp)
        shaders = {}
        for name, body in PROGRAMS.items():
            words = words_of(body, args.llvm_mc)
            emitted = subprocess.check_output([args.emitter.resolve(), *(f"{word:08x}" for word in words)],
                                              text=True, timeout=30)
            source = trial / f"{name}.hlsl"
            source.write_text(instrument(emitted))
            blob = trial / f"{name}.cso"
            subprocess.run([args.dxc, "-T", "cs_6_6", "-E", "main", "-Fo", blob, source], check=True, timeout=30,
                           capture_output=True)
            subprocess.run([args.dxv, blob], check=True, timeout=30, capture_output=True)
            shaders[name] = dict(hlsl_sha256=digest(source), dxil_sha256=digest(blob))
        trials = {}
        for name, program, texels, compared, stored in cases():
            trials[name] = program, expected(program, texels, compared, stored)
            seed = [*texels, *compared, *stored, *([0x76540000] * LANES)]
            (trial / f"{name}.seed").write_bytes(struct.pack(f"<{TEXELS + WORDS}I", *seed))
            (trial / f"{name}.cso").hardlink_to(trial / f"{program}.cso")
        (trial / "cases.txt").write_text("".join(f"{name}\n" for name in trials))
        exe, status, fields = run_gpu_guest(
            ROOT / "tools/reference/independent-shaders/image_atomic.cpp", "cmpswap.exe", trial, work,
            args.proton.resolve(), args.prefix, args.provider, args.compiler_dir)
        wrong = {}
        for name, (program, want) in trials.items():
            path = trial / f"{name}.bin"
            found = difference(program, want, path.read_bytes() if path.is_file() else b"")
            if found:
                wrong[name] = found
        passed = status == 0 and fields.get("adapter") == args.adapter and not wrong
        report = dict(passed=passed, scope="Synthetic image_atomic_cmpswap programs on a 3D uint UAV",
                      adapter=fields.get("adapter"), guest_exit=status, cases=len(trials),
                      compared_words=len(trials) * (TEXELS + WORDS), wrong=wrong, shaders=shaders,
                      emitter_sha256=digest(args.emitter), guest_sha256=digest(exe))
        if fields.get("error"):
            report["error"] = fields["error"]
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    for name, detail in wrong.items():
        print(f"WRONG {name}: {detail}")
    if not passed:
        print(f"FAIL: {len(wrong)} of {len(trials)} cases, guest exit {status}")
        return 1
    print(f"PASS: {len(trials)} cases, {len(trials) * (TEXELS + WORDS)} words, adapter {fields['adapter']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
