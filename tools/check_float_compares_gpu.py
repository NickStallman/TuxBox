#!/usr/bin/env python3
"""Check on a real GPU that float compares give the console's lane masks.

Every `v_cmp_*_f32` and `v_cmpx_*_f32` predicate and `v_cmp[x]_class_f32`
runs on the edge values of each comparison: both zeros, denormals, infinities,
quiet and signalling NaN. Each runs in the VOPC and VOP3 encodings, with source
modifiers, inline constants, a literal, a scalar source and the denormal mode
that keeps inputs. Some lanes are inactive on entry. The programs read every
result as a whole wave does: the lane's bit, the bits of the lanes below it and
the count. A `v_cmpx` result is read from EXEC, from a write that only the
lanes left in EXEC make, and from a branch on an empty EXEC; VCC must keep its
value. Each program runs in waves of its own width, where the host has them,
and each 64-lane program also runs in the form a 32-lane host uses. The
reference models AMD's RDNA 2 instruction set. Two faults of the translation
that are not fixed yet are expected, and modelled word for word, so the check
fails when either stops showing; `KNOWN` names them. No game data is needed.
"""
import argparse
import json
import math
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
from check_lockstep_gpu import branch, label  # noqa: E402
from frosty_gpu_guest import run_gpu_guest  # noqa: E402
from project_tuxbox.dxil_store import COMPUTE_FLAGS  # noqa: E402
from project_tuxbox.inspect import digest  # noqa: E402

WAVE_OPTIONS = {32: (), 64: ("-mattr=-wavefrontsize32,+wavefrontsize64",)}
# The predicates in opcode order, as the ISA names them.
PREDICATES = ("f", "lt", "eq", "le", "gt", "lg", "ge", "o", "u", "nge", "nlg", "ngt", "nle", "neq", "nlt", "tru")
# form: (encoding, first source, second source). The scalar source is the
# first active lane's v0; the denormal form runs with fp32 denormals kept.
FORMS = {
    "e32": ("e32", "v0", "v1"),
    "e64": ("e64", "v0", "v1"),
    "neg-abs": ("e64", "-v0", "|v1|"),
    "abs-neg": ("e64", "|v0|", "-|v1|"),
    "abs-const": ("e64", "|v0|", "0.5"),
    "zero": ("e32", "0", "v1"),
    "literal": ("e32", "0x3fc00000", "v1"),
    "scalar": ("e32", "s23", "v1"),
    "denorm": ("e32", "v0", "v1"),
}
# The second source of a class test is the mask of classes.
CLASS_FORMS = {
    "e32": ("e32", "v0", "v1"),
    "e64": ("e64", "v0", "v1"),
    "neg": ("e64", "-v0", "v1"),
    "abs": ("e64", "|v0|", "v1"),
    "literal": ("e64", "v0", "0x2c3"),
    "scalar": ("e32", "s23", "v1"),
    "denorm": ("e32", "v0", "v1"),
}
CONSTANTS = {"0": 0, "0.5": 0x3f000000}
ITEMS = 8
# Words of the view: v0 inputs, v1 inputs, the entry EXEC, then one 64-word
# slot per output register from v10.
A_WORD, B_WORD, EXEC_WORD, FIRST_SLOT = 0, 64, 128, 3
OUTPUTS = {32: range(10, 20), 64: range(10, 19)}
VIEW_START, BUFFER_WORDS = 96, (768 + 4096) // 4
SEED = 0x76540000
# A number in the register after a wave32 compare's destination, which the compare must keep.
S25 = 0x1234fedc

# Signed zeros, denormals, ones, the constants the forms use, the largest and
# smallest normals, infinities, and quiet and signalling NaN.
EDGES = [0x00000000, 0x80000000, 0x00000001, 0x807fffff, 0x3f800000, 0xbf800000, 0x3f000000, 0xbf000000,
         0x3fc00000, 0x7f7fffff, 0x80800000, 0x7f800000, 0xff800000, 0x7fc00000, 0xff800001]
CLASS_MASKS = [1 << bit for bit in range(10)] + [0, 0x3ff, 0xfffffc00, 0x2c3, 0x13c, 0x2a5]


def mnemonics():
    """Every instruction this check covers."""
    names = [f"v_cmp{x}_{p}_f32" for x in ("", "x") for p in PREDICATES]
    return names + ["v_cmp_class_f32", "v_cmpx_class_f32"]


def to_float(bits):
    return struct.unpack("<f", struct.pack("<I", bits))[0]


def value(bits, keep_denormals):
    """The number a compare sees. A flushed denormal is a zero of its sign."""
    if not keep_denormals and not bits & 0x7f800000:
        bits &= 0x80000000
    return to_float(bits)


def relation(predicate, x, y):
    """RDNA 2 VOPC predicates. Python's ordered comparisons are false on NaN, as the ISA's are."""
    unordered = math.isnan(x) or math.isnan(y)
    return {
        "f": False, "lt": x < y, "eq": x == y, "le": x <= y, "gt": x > y, "lg": x < y or x > y, "ge": x >= y,
        "o": not unordered, "u": unordered,
        "nge": not x >= y, "nlg": not (x < y or x > y), "ngt": not x > y, "nle": not x <= y,
        "neq": not x == y, "nlt": not x < y, "tru": True,
    }[predicate]


def float_class(bits):
    """The class number V_CMP_CLASS_F32 tests, from the raw bits in every denormal mode."""
    exponent, fraction, negative = (bits >> 23) & 0xff, bits & 0x7fffff, bits >> 31
    if exponent == 0xff:
        return (1 if fraction & 0x400000 else 0) if fraction else (2 if negative else 9)
    if exponent == 0:
        return (5 if negative else 6) if not fraction else (4 if negative else 7)
    return 3 if negative else 8


def modified(text, bits):
    """Apply `|x|` and `-x` source modifiers to a word."""
    negate, text = text.startswith("-"), text.lstrip("-")
    if text.startswith("|"):
        bits &= 0x7fffffff
    return bits ^ (0x80000000 if negate else 0)


def source(text, lane, a, b, first):
    core = text.strip("-|")
    if core in CONSTANTS:
        bits = CONSTANTS[core]
    elif core.startswith("0x"):
        bits = int(core, 16)
    else:
        bits = {"v0": a[lane], "v1": b[lane], "s23": a[first]}[core]
    return modified(text, bits)


def forms_of(name):
    return CLASS_FORMS if "_class_" in name else FORMS


def items():
    """Every (instruction, form) pair the check runs."""
    return [(name, form) for name in mnemonics() for form in forms_of(name)]


def chunks_of(pairs):
    """Programs of up to `ITEMS` pairs. Class tests take masks as their second input."""
    for kind in (False, True):
        group = [pair for pair in pairs if ("_class_" in pair[0]) == kind]
        for start in range(0, len(group), ITEMS):
            yield group[start:start + ITEMS]


def masks(width):
    wide = width == 64
    return {"exec": "exec" if wide else "exec_lo", "vcc": "vcc" if wide else "vcc_lo",
            "saved": "s[12:13]" if wide else "s12", "copy": "s[20:21]" if wide else "s20",
            "sdst": "s[24:25]" if wide else "s24", "bits": "b64" if wide else "b32"}


def read_mask(mask, low, high, out, width):
    """Leave `bit | rank << 8 | count << 16` of a mask in `out`, its bit already there."""
    lines = [f"v_mbcnt_lo_u32_b32 v4, {low}, 0"]
    if width == 64:
        lines.append(f"v_mbcnt_hi_u32_b32 v4, {high}, v4")
    return lines + [f"s_bcnt1_i32_{masks(width)['bits']} s26, {mask}", "v_lshl_or_b32 v4, s26, 8, v4",
                    f"v_lshl_or_b32 {out}, v4, 8, {out}"]


def item_lines(index, name, form, width):
    m = masks(width)
    encoding, first, second = forms_of(name)[form]
    out = f"v{10 + index}"
    before = ["v_readfirstlane_b32 s23, v0"] if form == "scalar" else []
    # fp16 and fp64 denormals stay kept, as in the default MODE of 0xc0.
    before += ["s_denorm_mode 0xf"] if form == "denorm" else []
    after = ["s_denorm_mode 0xc"] if form == "denorm" else []
    if name.startswith("v_cmpx_"):
        skip = f"skip{index}"
        return [*before, "s_mov_b32 s22, 0", f"{name}_{encoding} {first}, {second}", *after,
                f"s_mov_b{width} {m['copy']}, {m['exec']}", branch("s_cbranch_execz", skip),
                "s_mov_b32 s22, 2", f"v_mov_b32_e32 {out}, 1", label(skip),
                f"s_mov_b{width} {m['exec']}, {m['saved']}", f"v_or_b32_e32 {out}, s22, {out}",
                *read_mask(m["copy"], "s20", "s21", out, width)]
    dst = m["vcc"] if encoding == "e32" else m["sdst"]
    low, high = ("vcc_lo", "vcc_hi") if encoding == "e32" else ("s24", "s25")
    return [*before, f"{name}_{encoding} {dst}, {first}, {second}", *after,
            f"v_cndmask_b32_e64 {out}, 0, 1, {dst}", *read_mask(dst, low, high, out, width)]


def program(chunk, width):
    m = masks(width)
    lines = [f"s_mov_b{width} {m['saved']}, {m['exec']}", "v_mbcnt_lo_u32_b32 v2, -1, 0"]
    if width == 64:
        lines.append("v_mbcnt_hi_u32_b32 v2, -1, v2")
    # VCC holds the odd active lanes. In a wave32 program a VOP3 compare's
    # destination is one register, and the next holds a number.
    lines += ["v_and_b32_e32 v3, 1, v2", f"v_cmp_eq_u32_e32 {m['vcc']}, 1, v3"]
    if width == 32:
        lines.append(f"s_mov_b32 s25, 0x{S25:x}")
    for index, (name, form) in enumerate(chunk):
        lines += item_lines(index, name, form, width)
    lines += ["v_cndmask_b32_e64 v18, 0, 1, " + m["vcc"], *read_mask(m["vcc"], "vcc_lo", "vcc_hi", "v18", width)]
    if width == 32:
        lines.append("v_mov_b32_e32 v19, s25")
    return lines + ["s_endpgm"]


def encodings(lines, llvm_mc, width):
    """The words of each line, from one run of the assembler."""
    run = subprocess.run([str(llvm_mc), "-triple=amdgcn-amd-amdhsa", "-mcpu=gfx1030", "-show-encoding",
                          *WAVE_OPTIONS[width]], input="\n".join(lines) + "\n", text=True, capture_output=True,
                         check=True, timeout=60)
    found = re.findall(r"encoding: \[([^\]]+)\]", run.stdout)
    if len(found) != len(lines):
        raise ValueError(f"{len(lines)} lines gave {len(found)} encodings: {run.stderr[-300:]}")
    return [list(struct.unpack(f"<{len(line.split(',')) // 4}I", bytes(int(v, 16) for v in line.split(","))))
            for line in found]


def words_of(items_, llvm_mc, width):
    """Native words for a list of lines, labels and forward branches."""
    encoded = iter(encodings([item for item in items_ if isinstance(item, str)], llvm_mc, width))
    pieces, offsets, position = [], {}, 0
    for item in items_:
        if isinstance(item, str):
            words = next(encoded)
            pieces.append(words)
            position += len(words)
        elif item[0] == "label":
            offsets[item[1]] = position
        else:
            pieces.append(item)
            position += 1
    words = []
    for piece in pieces:
        if isinstance(piece, tuple):
            jump = (offsets[piece[2]] - len(words) - 1) & 0xffff
            words += encodings([f"{piece[1]} {jump}"], llvm_mc, width)[0]
        else:
            words += piece
    return words


# Known faults of the translation, which the check expects until they are fixed.
# A form names the translation: "32", "64" or "64-paired". Each fault is also
# modelled, so the check fails when one disappears without this table changing.
KNOWN = {}


def known_for(form):
    return frozenset(name for name, fault in KNOWN.items() if form in fault["forms"])


def model(chunk, width, active, a, b, faults=frozenset()):
    """Each output register of every lane, as the console leaves them, or with
    the named `KNOWN` faults of the translation."""
    lanes = range(width)
    first = next((lane for lane in lanes if active[lane]), 0)
    vcc = [active[lane] and lane & 1 == 1 for lane in lanes]
    out = {register: [0] * width for register in OUTPUTS[width]}
    clobbered = False

    def word(mask, lane, low):
        return low | sum(mask[:lane]) << 8 | sum(mask) << 16

    for index, (name, form) in enumerate(chunk):
        encoding, first_text, second_text = forms_of(name)[form]
        result = []
        for lane in lanes:
            x = source(first_text, lane, a, b, first)
            y = source(second_text, lane, a, b, first)
            if "_class_" in name:
                hit = bool(y >> float_class(x) & 1)
            else:
                keep = form == "denorm"
                hit = relation(name.split("_")[2], value(x, keep), value(y, keep))
            result.append(active[lane] and hit)
        if name.startswith("v_cmpx_"):
            taken = [bit if "execz" in faults else any(result) for bit in result]
            low = [int(bit) | (2 if branch else 0) for bit, branch in zip(result, taken)]
        else:
            low = [int(bit) for bit in result]
            if encoding == "e32":
                vcc = result
            elif "s25" in faults:
                clobbered = True
        for lane in lanes:
            if active[lane]:
                out[10 + index][lane] = word(result, lane, low[lane] & 0xff)
    for lane in lanes:
        if active[lane]:
            out[18][lane] = word(vcc, lane, int(vcc[lane]))
            if width == 32:
                out[19][lane] = 0 if clobbered else S25
    return out


def instrument(hlsl, width):
    """Load the inputs where the translation starts and store the outputs where it ends."""
    start = "    v0 = gtid.x; v1 = gtid.y; v2 = gtid.z;"
    if hlsl.count(start) != 1 or f"[WaveSize({width})]" not in hlsl:
        raise ValueError("fixture must run in waves of its own width")
    load = ("    uint2 tb_entry = output.Load2(%du);\n" % (4 * EXEC_WORD) +
            "    v0 = output.Load(4u * gtid.x); v1 = output.Load(4u * (64u + gtid.x)); v2 = gtid.z;\n"
            "    exec_lo = ((gtid.x < 32u ? tb_entry.x : tb_entry.y) >> (gtid.x & 31u)) & 1u;")
    stores = "".join(f"    output.Store(4u * ({64 * (FIRST_SLOT + n - 10)}u + gtid.x), "
                     f"{register(hlsl, f'v{n}')});\n" for n in OUTPUTS[width])
    hlsl = hlsl.replace(start, load)
    end = hlsl.rstrip().rfind("}")
    return "RWByteAddressBuffer output : register(u0);\n" + hlsl[:end] + stores + "}\n"


def register(hlsl, name):
    """A register the program never touches is not declared."""
    # A two-lane register `v7__[0]` is the pair `v7__`, which the code indexes by lane.
    base = name.split("[", 1)[0]
    return name if re.search(rf"\b{base}\b", hlsl) else "0u"


def instrument_paired(hlsl):
    """Two console lanes per thread: lane `l` in one half and `l + 32` in the other."""
    start = re.search(r"^    s\d+ = gid\.x; s\d+ = gid\.y; s\d+ = gid\.z;\n", hlsl, re.M)
    if not start or "tb_pair" not in hlsl or "[WaveSize(32)]" not in hlsl:
        raise ValueError("fixture must take the paired translation")
    lanes = [f"(tb_first + tb_pair + {32 * half}u)" for half in (0, 1)]
    # The two-lane form holds each vector register as a pair indexed by the lane.
    load = "".join(f"    v0__[{half}] = output.Load(4u * {lanes[half]}); v1__[{half}] = output.Load(4u * (64u + {lanes[half]}));\n"
                   for half in (0, 1))
    load += f"    uint2 tb_entry = output.Load2({4 * EXEC_WORD}u); exec_lo = tb_entry.x; exec_hi = tb_entry.y;\n"
    stores = "".join(f"    output.Store(4u * ({64 * (FIRST_SLOT + n - 10)}u + {lanes[half]}), "
                     f"{register(hlsl, f'v{n}__[{half}]')});\n" for n in OUTPUTS[64] for half in (0, 1))
    hlsl = hlsl[:start.end()] + load + hlsl[start.end():]
    end = hlsl.rstrip().rfind("}")
    return "RWByteAddressBuffer output : register(u0);\n" + hlsl[:end] + stores + "}\n"


def inputs(width, classes, rng):
    """(name, entry EXEC, v0 per lane, v1 per lane) for one program."""
    full = (1 << width) - 1
    seconds = CLASS_MASKS if classes else EDGES
    pairs = [(x, y) for x in EDGES for y in seconds]
    for number, start in enumerate(range(0, len(pairs), width)):
        part = [pairs[(start + lane) % len(pairs)] for lane in range(width)]
        yield f"grid{number}", full, [x for x, _ in part], [y for _, y in part]

    def pick():
        if rng.random() < 0.5:
            return rng.choice(EDGES)
        return struct.unpack("<I", struct.pack("<f", rng.uniform(-1, 1) * 2.0 ** rng.randint(-20, 20)))[0]

    for number in range(2):
        a = [pick() for _ in range(width)]
        b = [rng.choice(CLASS_MASKS) if classes else (a[lane] if rng.random() < 0.2 else pick())
             for lane in range(width)]
        yield f"random{number}", rng.getrandbits(width), a, b
    a = [rng.choice(EDGES) for _ in range(width)]
    b = [rng.choice(seconds) for _ in range(width)]
    yield "one_lane", 1 << rng.randrange(width), a, b
    yield "empty", 0, a, b
    yield "odd", full & 0xaaaaaaaaaaaaaaaa, a, b
    if width == 64:
        yield "upper_half", 0xffffffff00000000, a, b


def seed_for(mask, a, b):
    seed = [SEED + i for i in range(BUFFER_WORDS)]
    view = VIEW_START
    seed[view + A_WORD:view + A_WORD + len(a)] = a
    seed[view + B_WORD:view + B_WORD + len(b)] = b
    seed[view + EXEC_WORD:view + EXEC_WORD + 2] = [mask & 0xffffffff, mask >> 32]
    return seed


def expected(chunk, width, mask, a, b, faults=frozenset()):
    seed = seed_for(mask, a, b)
    active = [bool(mask >> lane & 1) for lane in range(width)]
    for register, values in model(chunk, width, active, a, b, faults).items():
        start = VIEW_START + 64 * (FIRST_SLOT + register - 10)
        seed[start:start + width] = values
    return seed


def describe(chunk, width, index):
    """Which output a wrong word belongs to."""
    slot, lane = divmod(index - VIEW_START, 64)
    register = slot - FIRST_SLOT + 10
    if register in OUTPUTS[width] and lane < width:
        if register < 10 + len(chunk):
            return f"{chunk[register - 10][0]} {chunk[register - 10][1]}", lane
        return {18: "VCC after the program", 19: "s25 after a wave32 compare"}.get(register, f"v{register}"), lane
    return "guard", lane


def fields(word):
    return dict(bit=word & 1, branch=word >> 1 & 1, rank=word >> 8 & 0xff, count=word >> 16 & 0xff)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("emitter", "llvm-mc", "dxc", "dxv", "provider", "proton", "prefix", "compiler-dir", "work",
                 "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--adapter", required=True)
    args = parser.parse_args(argv)
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    rng = random.Random(0x46434d50)
    with TemporaryDirectory(dir=work) as tmp:
        trial = Path(tmp)
        trials, shaders, native, refused = {}, {}, set(), {}
        for number, chunk in enumerate(chunks_of(items())):
            classes = "_class_" in chunk[0][0]
            for form, width in (("32", 32), ("64", 64), ("64-paired", 64)):
                paired = form == "64-paired"
                words = words_of(program(chunk, width), args.llvm_mc, width)
                run = subprocess.run([args.emitter.resolve(), form, *(f"{word:08x}" for word in words)],
                                     capture_output=True, text=True, timeout=60)
                shader = f"compare{number:02}w{width}" + ("paired" if paired else "")
                if run.returncode == 65:
                    refused[shader] = run.stderr.strip()[:300]
                    continue
                run.check_returncode()
                source = trial / f"{shader}.hlsl"
                source.write_text(instrument_paired(run.stdout) if paired else instrument(run.stdout, width))
                blob = trial / f"{shader}.cso"
                subprocess.run([args.dxc, "-T", "cs_6_6", "-E", "main", *COMPUTE_FLAGS, "-Fo", blob, source],
                               check=True, timeout=60, capture_output=True)
                subprocess.run([args.dxv, blob], check=True, timeout=30, capture_output=True)
                shaders[shader] = dict(items=[" ".join(pair) for pair in chunk], hlsl_sha256=digest(source),
                                       dxil_sha256=digest(blob))
                for case, mask, a, b in inputs(width, classes, rng):
                    name = f"{shader}_{case}"
                    faults = known_for(form)
                    trials[name] = (chunk, width, expected(chunk, width, mask, a, b),
                                    {fault: expected(chunk, width, mask, a, b, faults & {fault}) for fault in faults},
                                    expected(chunk, width, mask, a, b, faults))
                    (trial / f"{name}.seed").write_bytes(struct.pack(f"<{BUFFER_WORDS}I", *seed_for(mask, a, b)))
                    (trial / f"{name}.cso").hardlink_to(blob)
                    if form == "64":
                        (trial / f"{name}.minwave").write_text("64\n")
                        native.add(name)
        (trial / "cases.txt").write_text("".join(f"{name} 0 raw-wide 1\n" for name in trials))
        exe, status, fields_ = run_gpu_guest(
            ROOT / "tools/reference/independent-shaders/compute.cpp", "float-compares.exe", trial, work,
            args.proton.resolve(), args.prefix, args.provider, args.compiler_dir)
        # Only a host that reports narrower waves may leave the native 64-lane programs out.
        narrow = int(fields_.get("wave_lanes_max", 0)) in range(1, 64)
        skipped = {name for name in native if narrow and fields_.get(f"{name}_skipped") == "64"}
        wrong, seen, fixed = {}, {fault: 0 for fault in KNOWN}, {}
        for name, (chunk, width, want, single, faulty) in trials.items():
            if name in skipped:
                continue
            path = trial / f"{name}.bin"
            data = path.read_bytes() if path.is_file() else b""
            if len(data) != 4 * BUFFER_WORDS:
                wrong[name] = "no readback"
                continue
            got = list(struct.unpack(f"<{BUFFER_WORDS}I", data))
            # A known fault that changes this case must show, exactly as modelled.
            if faulty != want and got == faulty:
                for fault, words in single.items():
                    seen[fault] += words != want
                continue
            if faulty != want and (got == want or got in single.values()):
                # One fault or both no longer show.
                fixed[name] = sorted(fault for fault, words in single.items() if words != want and got != words)
                continue
            for index, (have, should) in enumerate(zip(got, want)):
                if have != should:
                    owner, lane = describe(chunk, width, index)
                    detail = dict(lane=lane, got=have, wanted=should)
                    if owner not in ("guard", "s25 after a wave32 compare"):
                        detail.update(got_fields=fields(have), wanted_fields=fields(should))
                    wrong.setdefault(name, {}).setdefault(owner, detail)
        failing = sorted({owner for found in wrong.values() if isinstance(found, dict) for owner in found})
        ran = len(trials) - len(skipped)
        passed = status == 0 and fields_.get("adapter") == args.adapter and not wrong and not refused and not fixed
        report = dict(passed=passed, scope="Synthetic float compares read as whole-wave lane masks",
                      adapter=fields_.get("adapter"), guest_exit=status, cases=ran, compared_words=ran * BUFFER_WORDS,
                      instructions=mnemonics(), refused=refused, wrong_outputs=failing, wrong=wrong,
                      known_faults={fault: dict(cause=KNOWN[fault]["cause"], cases=count) for fault, count in seen.items()},
                      known_faults_not_seen=fixed, shaders=shaders,
                      native_wave64_cases_not_run=len(skipped), wave_lanes_max=fields_.get("wave_lanes_max"),
                      emitter_sha256=digest(args.emitter), guest_sha256=digest(exe))
        if fields_.get("error"):
            report["error"] = fields_["error"]
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    for shader, reason in refused.items():
        print(f"REFUSED {shader}: {reason}")
    for owner in failing:
        print(f"WRONG {owner}")
    for name, faults in fixed.items():
        print(f"FIXED? {name} matches the console, not the known fault {', '.join(faults)}: update KNOWN")
    if not passed:
        print(f"FAIL: {len(wrong)} of {ran} cases, {len(failing)} outputs wrong, {len(fixed)} known faults not seen, "
              f"{len(refused)} programs refused, guest exit {status}")
        return 1
    note = f", {len(skipped)} native wave64 cases not run (host has no 64-lane waves)" if skipped else ""
    known = "; ".join(f"{count} cases show the known fault: {KNOWN[fault]['cause']}" for fault, count in seen.items() if count)
    print(f"PASS: {len(mnemonics())} instructions, {ran} cases, {ran * BUFFER_WORDS} words, "
          f"adapter {fields_['adapter']}{note}" + (f". {known}" if known else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
