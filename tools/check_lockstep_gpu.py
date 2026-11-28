#!/usr/bin/env python3
"""Check on a real GPU that translated branches are decisions of the whole wave.

Each synthetic wave32 program branches on a lane mask, then runs a reduction
behind a mask branch, which the translator can only express by branching with
the whole wave. One program also runs as a ray tracing resolve shader, whose
interleaved loops go through the block dispatcher, and once with only an EXEC
guard before its reduction, which the dispatcher keeps as a decision of the
wave too. Another exchanges lanes in the resolve mode. Others count the lanes
of a mask, find its first, retire lanes with a write to EXEC, and read the lane
a mask elects. Two more read a lane inside a loop that lanes leave at different
times: a sum for each distinct key, and a read of lanes that have already left.
The reference models the console: one program counter per wave,
shared scalar registers and per-lane vector writes under EXEC. Only the
translator's input and output are instrumented. No game data is needed.
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
from check_packed_integer_gpu import assemble  # noqa: E402
from check_wave_guard import cases  # noqa: E402
from frosty_gpu_guest import run_gpu_guest  # noqa: E402
from project_tuxbox.dxil_store import COMPUTE_FLAGS  # noqa: E402
from project_tuxbox.inspect import digest  # noqa: E402

WIDTH = 32
LIMIT = 1000
# The OR-saveexec, DPP and lane-read reduction of the translator's own fixture:
# s7 receives the sum of the active lanes' v0.
REDUCTION = [0xbe803dc1, 0xd5010002, 0x00020080, 0x4a0404e9, 0xde54c102, 0x4a0404e9, 0x93601202,
             0x4a0404e9, 0x00092402, 0x4a0404fa, 0xff094002, 0xd7780003, 0x02010102, 0x4a040503,
             0xd7600007, 0x00010102, 0xbefe0300]
OUTPUTS = ("s20", "s21", "v10", "s7")
COMPARE = f"v_cmp_lt_u32_e32 vcc_lo, 0x{LIMIT:x}, v0"
# Lanes exchanged in the ray tracing resolve mode: reversed within each quad,
# then swapped with the neighbour. A lane EXEC has not got gives zero. A scalar
# write behind a branch on EXEC follows, as in that mode's other program.
SWIZZLES = ["ds_swizzle_b32 v11, v0 offset:0x801b", "ds_swizzle_b32 v10, v11 offset:0x041f", COMPARE,
            "s_and_saveexec_b32 s10, vcc_lo", ("branch", "s_cbranch_execz", "done"), "s_mov_b32 s20, 7",
            ("label", "done"), "s_mov_b32 exec_lo, s10"]


def label(name):
    return ("label", name)


def branch(mnemonic, target):
    return ("branch", mnemonic, target)


# Every program may use s10-s11, s20-s23, v10-v12. s12 holds the entry EXEC.
BODIES = {
    # A scalar write behind a branch on VCC.
    "vccz": [COMPARE, branch("s_cbranch_vccz", "done"), "s_mov_b32 s20, 1", label("done"),
             "v_mov_b32_e32 v10, s20"],
    "vccnz": [COMPARE, "s_mov_b32 s20, 1", branch("s_cbranch_vccnz", "done"), "s_mov_b32 s20, 2", label("done")],
    # A scalar write in an EXEC-guarded block that no lane may enter.
    "execz": [COMPARE, "s_and_saveexec_b32 s10, vcc_lo", branch("s_cbranch_execz", "done"),
              "s_mov_b32 s20, 7", "v_mov_b32_e32 v10, 5", label("done"), "s_mov_b32 exec_lo, s10"],
    # The condition code of a mask operation, and a whole mask compared with zero.
    "scc": [COMPARE, "s_and_b32 s11, vcc_lo, exec_lo", branch("s_cbranch_scc0", "first"),
            "s_mov_b32 s20, 3", label("first"), "s_cmp_eq_u32 vcc_lo, 0", branch("s_cbranch_scc1", "done"),
            "s_mov_b32 s21, 4", label("done")],
    # Lanes leave a loop at different iterations; the wave stays until the last.
    "loop": ["v_and_b32_e32 v11, 7, v0", "s_mov_b32 s10, exec_lo", label("again"),
             "s_add_u32 s20, s20, 1", "v_cmp_lt_u32_e64 s11, s20, v11", "s_and_b32 exec_lo, exec_lo, s11",
             "v_add_nc_u32_e32 v10, 1, v10", branch("s_cbranch_execnz", "again"), "s_mov_b32 exec_lo, s10"],
    # Interleaved loops, which take the block dispatcher.
    "dispatch": [COMPARE, label("outer"), "s_add_u32 s20, s20, 1", label("inner"),
                 branch("s_cbranch_vccz", "quiet"), "s_add_u32 s21, s21, 1", label("quiet"),
                 "s_and_saveexec_b32 s10, vcc_lo", branch("s_cbranch_execz", "empty"),
                 "v_add_nc_u32_e32 v10, 1, v10", label("empty"), "s_mov_b32 exec_lo, s10",
                 "s_add_u32 s22, s22, 1", "s_cmp_lt_u32 s20, 3", branch("s_cbranch_scc1", "outer"),
                 "s_cmp_lt_u32 s22, 6", branch("s_cbranch_scc1", "inner")],
    # A mask filled behind a branch on a mask, then read as a number: its lanes
    # counted, its first found, and those below each lane counted.
    "counted": ["s_mov_b32 s11, 0", COMPARE, branch("s_cbranch_vccz", "none"), "s_and_b32 s11, vcc_lo, exec_lo",
                label("none"), "s_bcnt1_i32_b32 s20, s11", "s_ff1_i32_b32 s21, s11",
                "v_mbcnt_lo_u32_b32 v10, s11, 0"],
    # The lanes a block left out, taken from EXEC and its saved copy at once.
    "others": [COMPARE, "s_and_saveexec_b32 s10, vcc_lo", "v_mov_b32_e32 v10, 5", "s_andn2_wrexec_b32 s11, s10",
               branch("s_cbranch_scc0", "done"), "s_mov_b32 s20, 9", "v_mov_b32_e32 v10, 6", label("done"),
               "s_bcnt1_i32_b32 s21, s11", "s_mov_b32 exec_lo, s10"],
    # EXEC restored from a register that one path left a number in. Nothing runs
    # under it before the next restore.
    "stale": ["s_mov_b32 s10, exec_lo", COMPARE, branch("s_cbranch_vccz", "kept"), "s_mov_b32 s10, 5",
              label("kept"), "s_mov_b32 exec_lo, s10", "s_mov_b32 exec_lo, s12", "s_mov_b32 s20, 4",
              "v_mov_b32_e32 v10, s20"],
    # The first running lane, and the value that lane holds.
    "first": ["s_and_b32 s11, s12, exec_lo", "s_ff1_i32_b32 s11, s11", "v_readlane_b32 s21, v0, s11",
              "s_mov_b32 s20, s11"],
}


# Loops that lanes leave at different times, around a lane read. Each is a
# whole program body and needs no reduction after it.
LANE_READ_BODIES = {
    # For each distinct key among the pending lanes, add up the values of the
    # lanes that hold it with a lane-exchange sum, and give the first of them
    # the total.
    "keysum": [
        "v_and_b32_e32 v11, 3, v0", "v_mov_b32_e32 v10, 0", "v_mov_b32_e32 v13, 1", label("again"),
        "s_add_u32 s21, s21, 1", "v_cmp_ne_u32_e32 vcc_lo, 0, v13", "v_mov_b32_e32 v13, 0",
        "s_and_saveexec_b32 s8, vcc_lo", branch("s_cbranch_execz", "tail"),
        "v_readfirstlane_b32 s9, v11", "v_cmp_eq_u32_e64 s1, s9, v11", "s_and_saveexec_b32 s10, s1",
        branch("s_cbranch_execz", "other"), "v_mov_b32_e32 v1, v0", "v_mov_b32_e32 v14, 0",
        "s_or_saveexec_b32 s11, -1", "v_mov_b32_e32 v2, 0", "v_mov_b32_e32 v3, 0",
        "v_cndmask_b32_e64 v2, v2, v1, s11", "v_cndmask_b32_e64 v3, v3, v14, s11",
        *(line for move in ("dpp8:[1,0,3,2,5,4,7,6]", "dpp8:[2,2,0,0,6,6,4,4]", "dpp8:[4,4,4,4,0,0,0,0]",
                            "row_mirror bound_ctrl:1")
          for line in (f"v_mov_b32 v4, v2 {move}", f"v_mov_b32 v5, v3 {move}",
                       "v_add_co_u32 v2, vcc_lo, v4, v2", "v_add_co_ci_u32_e32 v3, vcc_lo, v5, v3, vcc_lo")),
        "v_permlanex16_b32 v4, v2, 0, 0", "v_permlanex16_b32 v5, v3, 0, 0",
        "v_add_co_u32 v2, vcc_lo, v4, v2", "v_add_co_ci_u32_e32 v3, vcc_lo, v5, v3, vcc_lo",
        "v_readlane_b32 s2, v2, 0", "v_readlane_b32 s3, v3, 0", "s_mov_b32 exec_lo, s11",
        "v_mbcnt_lo_u32_b32 v15, -1, 0", "s_ff1_i32_b32 s11, exec_lo", "v_cmpx_eq_i32_e32 s11, v15",
        branch("s_cbranch_execz", "other"), "v_lshl_add_u32 v10, s3, 16, s2", label("other"),
        "s_mov_b32 exec_lo, s10", "v_cndmask_b32_e64 v13, 0, 1, s1", "v_xor_b32_e32 v13, 1, v13",
        label("tail"), "s_mov_b32 exec_lo, s8", "v_cmp_ne_u32_e32 vcc_lo, 0, v13",
        "s_bcnt1_i32_b32 s1, vcc_lo", branch("s_cbranch_scc1", "again"),
        "s_bcnt1_i32_b32 s20, exec_lo",
    ],
    # A waterfall over the keys that reads two fixed lanes in every iteration,
    # whether or not they are still in the loop.
    "retired": [
        "v_and_b32_e32 v11, 3, v0", "v_mov_b32_e32 v10, 0", "s_mov_b32 s16, exec_lo",
        "s_mov_b32 s17, exec_lo", label("again"), "v_readfirstlane_b32 s18, v11",
        "v_cmpx_eq_u32_e32 s18, v11", "v_readlane_b32 s22, v0, 5",
        "v_readlane_b32 s23, v0, 30", "s_add_u32 s22, s22, s23", "s_xor_b32 s21, s21, s22",
        "s_add_u32 s21, s21, s20", "v_add_nc_u32_e32 v10, s22, v10",
        "s_add_u32 s20, s20, 1", "s_andn2_wrexec_b32 s17, s17", branch("s_cbranch_execnz", "again"),
        "s_mov_b32 exec_lo, s16",
    ],
}


def program(body, resolve=False, guarded=False):
    """Assembler lines and branch positions of one complete program."""
    if body in LANE_READ_BODIES.values():
        return ["s_mov_b32 s12, exec_lo", *body, "s_mov_b32 exec_lo, s12", "s_endpgm"]
    # No active lane skips the reduction. A branch on VCC around it is what the
    # targeted guards refuse. One on EXEC they accept as a vote of the wave,
    # which only a program without interleaved loops keeps.
    skip = ([branch("s_cbranch_execz", "end")] if guarded
            else ["v_cmp_eq_u32_e32 vcc_lo, v1, v1", branch("s_cbranch_vccz", "end")])
    return [
        "s_mov_b32 s12, exec_lo",
        # The resolve mode is selected by a compare into an odd scalar register.
        *(["v_cmp_eq_u32_e64 s31, 0, v1"] if resolve else []), *body, "s_mov_b32 exec_lo, s12",
        *skip, ("words", REDUCTION), label("end"), "s_endpgm",
    ]


def words_of(items, llvm_mc, *options):
    lines = [item for item in items if isinstance(item, str)]
    sizes, encoded = [], []
    for line in lines:
        encoded.append(assemble([line], llvm_mc, *options))
    offsets, position, queue = {}, 0, iter(encoded)
    for item in items:
        if isinstance(item, str):
            sizes.append(next(queue))
            position += len(sizes[-1])
        elif item[0] == "label":
            offsets[item[1]] = position
        else:
            position += 1 if item[0] == "branch" else len(item[1])
    words, queue = [], iter(encoded)
    for item in items:
        if isinstance(item, str):
            words += next(queue)
        elif item[0] == "words":
            words += item[1]
        elif item[0] == "branch":
            jump = (offsets[item[2]] - len(words) - 1) & 0xffff
            words += assemble([f"{item[1]} {jump}"], llvm_mc)
    return words


def lane_read_model(name, active, values):
    """The console's registers after `LANE_READ_BODIES[name]`."""
    lanes = range(len(values))
    keys = [value & 3 for value in values]
    v10, iterations, mixed = [0] * len(values), 0, 0
    pending = [i for i in lanes if active[i]]
    while True:
        # `v_readfirstlane_b32` reads lane 0 when EXEC is empty.
        key = keys[pending[0] if pending else 0]
        group = [i for i in pending if keys[i] == key]
        if name == "keysum":
            iterations += 1
            if group:
                total = sum(values[i] for i in group)
                v10[group[0]] = ((total >> 32 << 16) + total) & 0xffffffff
        else:
            read = (values[5] + values[30]) & 0xffffffff
            mixed = ((mixed ^ read) + iterations) & 0xffffffff
            for i in group:
                v10[i] = (v10[i] + read) & 0xffffffff
            iterations += 1
        pending = [i for i in pending if i not in group]
        if not pending:
            break
    s20, s21 = (sum(active), iterations) if name == "keysum" else (iterations, mixed)
    return [(s20, s21, v10[i], 0) for i in lanes]


def model(name, active, values):
    """The console's registers after `BODIES[name]`, as one row per lane."""
    lanes = range(len(values))
    if name in LANE_READ_BODIES:
        return lane_read_model(name, active, values)
    taken = [active[i] and values[i] > LIMIT for i in lanes]
    some = any(taken)
    s20 = s21 = 0
    v10 = [0] * len(values)
    if name == "swizzle":
        v11 = [values[i ^ 3] if active[i] and active[i ^ 3] else 0 for i in lanes]
        v10 = [v11[i ^ 1] if active[i] and active[i ^ 1] else 0 for i in lanes]
        s20, name = 7 * some, "none"
    if name == "vccz":
        s20 = int(some)
        v10 = [s20 if active[i] else 0 for i in lanes]
    elif name == "vccnz":
        s20 = 1 if some else 2
    elif name == "execz":
        s20 = 7 if some else 0
        v10 = [5 if taken[i] else 0 for i in lanes]
    elif name == "scc":
        s20, s21 = (3, 4) if some else (0, 0)
    elif name == "loop":
        limits = [values[i] & 7 for i in lanes]
        running = list(active)
        while True:
            s20 += 1
            running = [running[i] and s20 < limits[i] for i in lanes]
            v10 = [v10[i] + int(running[i]) for i in lanes]
            if not any(running):
                break
    elif name == "dispatch":
        s22, at = 0, "outer"
        while at:
            if at == "outer":
                s20 += 1
            s21 += int(some)
            v10 = [v10[i] + int(taken[i]) for i in lanes]
            s22 += 1
            at = "outer" if s20 < 3 else "inner" if s22 < 6 else None
    elif name == "counted":
        s20 = sum(taken)
        s21 = taken.index(True) if some else 0xffffffff
        v10 = [sum(taken[:i]) if active[i] else 0 for i in lanes]
    elif name == "others":
        others = [active[i] and not taken[i] for i in lanes]
        s20, s21 = 9 * any(others), sum(others)
        v10 = [5 if taken[i] else 6 if others[i] else 0 for i in lanes]
    elif name == "stale":
        s20 = 4
        v10 = [4 if active[i] else 0 for i in lanes]
    elif name == "first":
        s20 = active.index(True) if any(active) else 0xffffffff
        s21 = values[s20 & (WIDTH - 1)]
    elif name != "none":
        raise ValueError(name)
    total = sum(v for v, a in zip(values, active) if a) & 0xffffffff if any(active) else 0
    return [(s20, s21, v10[i], total) for i in lanes]


def instrument(hlsl, resolve=False):
    if resolve:
        # This mode keeps whole masks and numbers its lanes by the host wave.
        start = "    v0 = WaveGetLaneIndex(); v1 = gtid.y; v2 = gtid.z; uint tb_lane_bit = 1u << v0;"
        load = (start + "\n    uint tb_io = v0; uint input = output[tb_io];\n"
                "    exec_lo = WaveActiveBallot((input >> 31) != 0u).x; v0 = input & 0x7fffffffu;")
        index, voted = "tb_io", "exec_lo == 0u"
    else:
        start = "    v0 = gtid.x; v1 = gtid.y; v2 = gtid.z;"
        load = ("    uint input = output[gtid.x];\n    exec_lo = input >> 31;\n"
                "    v0 = input & 0x7fffffffu; v1 = gtid.y; v2 = gtid.z;")
        index, voted = "gtid.x", "WaveActiveAnyTrue"
    if hlsl.count(start) != 1 or "[WaveSize(32)]" not in hlsl or voted not in hlsl:
        raise ValueError("fixture must branch with a whole wave32")
    hlsl = hlsl.replace(start, load)
    stores = []
    for column, register in enumerate(OUTPUTS):
        # A register the program never touches is not declared.
        value = register if register in hlsl.split() or f" {register} " in hlsl or f" {register}=" in hlsl else "0u"
        stores.append(f"    output[{32 * (column + 1)}u + {index}] = {value};\n")
    end = hlsl.rstrip().rfind("}")
    return "RWBuffer<uint> output : register(u0);\n" + hlsl[:end] + "".join(stores) + "}\n"


def inputs():
    yield from cases(WIDTH)
    yield "below", 0xffffffff, list(range(WIDTH))
    yield "one_above", 0xffffffff, [LIMIT + 1 if lane == 17 else lane for lane in range(WIDTH)]
    yield "inactive_above", 0x0000ffff, [lane if lane < 16 else LIMIT + lane for lane in range(WIDTH)]


def seed_and_expected(name, mask, values):
    seed = [0x76540000 + i for i in range(384)]
    active = [bool((mask >> lane) & 1) for lane in range(WIDTH)]
    seed[96:96 + WIDTH] = [v | (int(a) << 31) for v, a in zip(values, active)]
    expected = seed.copy()
    for lane, row in enumerate(model(name, active, values)):
        for column, word in enumerate(row):
            expected[96 + WIDTH * (column + 1) + lane] = word
    return struct.pack("<384I", *seed), struct.pack("<384I", *expected)


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
        expected, shaders = {}, {}
        bodies = BODIES | LANE_READ_BODIES
        for name, body, resolve in [*((name, name, False) for name in bodies), ("resolve", "dispatch", True),
                                    ("guarded", "dispatch", False), ("swizzle", "swizzle", True)]:
            lines = SWIZZLES if body == "swizzle" else bodies[body]
            words = words_of(program(lines, resolve, name == "guarded"), args.llvm_mc)
            emitted = subprocess.check_output(
                [args.emitter.resolve(), *(["resolve"] if resolve else []), str(WIDTH),
                 *(f"{word:08x}" for word in words)], text=True, timeout=30)
            source = trial / f"{name}.hlsl"
            source.write_text(instrument(emitted, resolve))
            blob = trial / f"{name}.cso"
            subprocess.run([args.dxc, "-T", "cs_6_6", "-E", "main", *COMPUTE_FLAGS, "-Fo", blob, source], check=True, timeout=30,
                           capture_output=True)
            subprocess.run([args.dxv, blob], check=True, timeout=30, capture_output=True)
            shaders[name] = dict(hlsl_sha256=digest(source), dxil_sha256=digest(blob),
                                 dispatched="tb_pc" in emitted)
            for case, mask, values in inputs():
                label_ = f"{name}_{case}"
                seed, expected[label_] = seed_and_expected(body, mask, values)
                (trial / f"{label_}.seed").write_bytes(seed)
                (trial / f"{label_}.cso").hardlink_to(blob)
        if not all(shaders[name]["dispatched"] for name in ("dispatch", "resolve", "guarded")):
            raise ValueError("the interleaved fixture must use the block dispatcher")
        (trial / "cases.txt").write_text("".join(f"{name} 0 uint 1\n" for name in expected))
        exe, status, fields = run_gpu_guest(
            ROOT / "tools/reference/independent-shaders/compute.cpp", "lockstep.exe", trial, work,
            args.proton.resolve(), args.prefix, args.provider, args.compiler_dir)
        wrong = {}
        for name, reference in expected.items():
            path = trial / f"{name}.bin"
            actual = path.read_bytes() if path.is_file() else b""
            if actual != reference:
                pairs = enumerate(zip(struct.unpack(f"<{len(actual) // 4}I", actual), struct.unpack("<384I", reference)))
                wrong[name] = next((dict(word=i, got=a, wanted=b) for i, (a, b) in pairs if a != b), "no readback")
        passed = status == 0 and fields.get("adapter") == args.adapter and not wrong
        report = dict(passed=passed, scope="Synthetic wave32 programs branching with the whole wave",
                      adapter=fields.get("adapter"), guest_exit=status, cases=len(expected),
                      compared_words=len(expected) * 384, wrong=wrong, shaders=shaders,
                      emitter_sha256=digest(args.emitter), guest_sha256=digest(exe))
        if fields.get("error"):
            report["error"] = fields["error"]
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    for name, detail in wrong.items():
        print(f"WRONG {name}: {detail}")
    if not passed:
        print(f"FAIL: {len(wrong)} of {len(expected)} cases, guest exit {status}")
        return 1
    print(f"PASS: {len(expected)} cases, {len(expected) * 384} words, adapter {fields['adapter']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
