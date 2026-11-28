#!/usr/bin/env python3
"""LLVM-assembled operand forms of float multiply, fused multiply-add,
fraction and byte conversion instructions on a D3D12 GPU.

The arithmetic check (`check_basic_alu_gpu.py`) runs v_mul_legacy_f32,
v_fmac_f32, v_fract_f32 and v_cvt_f32_ubyte0-3 with register sources. This
check runs the other forms ProjectScarlett's recorded shaders use: scalar and
literal sources, VOP3 abs, neg, CLAMP and OMOD, SDWA byte and word selection,
every op_sel and op_sel_hi of v_fma_mix_f32, f32 denormal inputs, and a fused
multiply-add whose product another instruction also uses. Each form is one
line in `FORMS` with an independent model of the console instruction.
Synthetic inputs and guard words; no game is used.

The models follow MODE[7:0] = 0xc0, the value at offset 0x44 of every
ProjectScarlett stage descriptor sampled: round to nearest even, f32 input and
output denormals flushed, f16 and f64 denormals kept (AMD RDNA 2 ISA, section
3.5). Where the manual or the unmeasured MODE bits leave the console's result
open, every candidate passes and the receipt counts, under `alternatives`,
which forms gave one other than the first:

- rounded twice: HLSL has no single-precision fused operation a host must
  honour (as in the arithmetic check);
- NaN clamped to 0: CLAMP passes NaN through when MODE.DX10_CLAMP is off;
- OMOD kept -0: OMOD turns -0 into +0, unless MODE.IEEE disables it;
- the sign of a legacy zero product, and whether a fraction rounded to 1.0
  is kept or replaced by the float below it, which the manual doesn't give.

`KNOWN` lists faults of the translation that one kind of host shows, each
modelled exactly, so the check fails when one disappears.
"""
import argparse
from fractions import Fraction
import json
import math
from pathlib import Path
import random
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from check_basic_alu_gpu import Float, fused, is_denormal, is_nan, matches, to_bits, to_float  # noqa: E402
from check_packed_integer_gpu import assemble, digest, instrument, translate, INIT, STRIDE, GROUPS  # noqa: E402
from frosty_gpu_guest import run_gpu_guest  # noqa: E402

M = 0xffffffff
SIGN = 0x80000000
ONE = 0x3f800000
QUIET = 0x7fc00000
BELOW_ONE = 0x3f7fffff
OUTPUTS = 8


class Open:
    """An expected result with the alternatives the console may also give.
    `options` are (reason, Float) pairs; the first has no reason."""

    def __init__(self, options):
        self.options = options

    def reason(self, got):
        """None for the first option, the reason of the first alternative that
        matches, or False for no match."""
        for reason, option in self.options:
            if option.matches(got):
                return reason
        return False

    def matches(self, got):
        return self.reason(got) is not False

    @property
    def bits(self):
        return self.options[0][1].bits

    def __repr__(self):
        return " or ".join(repr(option) + (f" ({reason})" if reason else "") for reason, option in self.options)


def joined(*reasons):
    return "; ".join(r for r in reasons if r) or None


def matches_any(got, want):
    return want.matches(got) if isinstance(want, Open) else matches(got, want)


def flush(bits):
    """An f32 source as MODE 0xc0 reads it: denormals become zero of the same sign."""
    return bits & SIGN if is_denormal(bits) else bits


def nearest(q):
    """The f32 nearest to a rational, ties to even, overflowing to infinity."""
    if not q:
        return 0
    near = to_bits(float(q))
    if near & 0x7fffffff >= 0x7f800000:
        return near
    # float() rounds to double first, so check the neighbours of that result.
    candidates = [near + step for step in (-1, 0, 1) if 0 <= (near & 0x7fffffff) + step <= 0x7f7fffff]
    return min(candidates, key=lambda bits: (abs(Fraction(to_float(bits)) - q), bits & 1))


def half_bits(h):
    """An f16 widened exactly to f32. f16 denormals are kept under MODE 0xc0."""
    return to_bits(struct.unpack("<e", struct.pack("<H", h & 0xffff))[0])


# ----- Instruction models on f32 source words (after modifiers) -----

# Each returns candidate results as (reason, bits) pairs, the first without a reason.

def legacy_product(a, b):
    """DX9 rule: 0.0 times anything, infinity and NaN included, is 0.0. The
    manual says only "0.0", so a zero of either sign passes."""
    a, b = flush(a), flush(b)
    if not a & 0x7fffffff or not b & 0x7fffffff:
        return [(None, 0), ("legacy zero product is -0", SIGN)]
    return [(None, to_bits(to_float(a) * to_float(b)))]


def twice(a, b, c):
    """a * b rounded, then the sum rounded."""
    return to_bits(to_float(to_bits(to_float(a) * to_float(b))) + to_float(c))


def fused_or_twice(a, b, c):
    once = fused(a, b, c)
    other = twice(a, b, c)
    return [(None, once)] + ([("rounded twice", other)] if other != once else [])


def fused_flushed(a, b, c):
    """`fma` with f32 input denormals flushed: one rounding of a * b + c."""
    return fused_or_twice(flush(a), flush(b), flush(c))


def fraction(a):
    """D = S0 + -floor(S0), 0.5 ULP. A tiny negative input rounds to 1.0; RDNA
    is said to return the float below 1 (the translator's helper, LLVM), but
    the manual doesn't say, so 1.0 passes too."""
    a = flush(a)
    value = to_float(a)
    if not math.isfinite(value):
        return [(None, QUIET)]
    exact = Fraction(value) - math.floor(Fraction(value))
    bits = nearest(exact)
    return [(None, BELOW_ONE), ("fraction rounded to 1.0", ONE)] if bits == ONE else [(None, bits)]


def byte_value(k):
    return lambda a: [(None, to_bits(float((a >> (8 * k)) & 0xff)))]


# ----- Source and result modifiers -----

def modified(bits, absolute, negate):
    if absolute:
        bits &= 0x7fffffff
    return bits ^ SIGN if negate else bits


def scaled(bits, omod):
    """OMOD with output denormals off: the rounded result is multiplied, a
    denormal is flushed and -0 becomes +0 (ISA section 6.2). With MODE.IEEE set
    OMOD is ignored, which only shows on a zero: -0 is then kept. Whether a
    denormal is flushed before it is scaled is not documented."""
    if is_nan(bits) or omod == 1:
        return [(None, bits)]
    out = []
    for reason, value in ([(None, 0), ("denormal scaled", bits)] if is_denormal(bits) else [(None, bits)]):
        product = to_bits(to_float(value) * omod)
        if is_denormal(product):
            # As for plain results, a kept denormal also passes (`Float`).
            out.append((reason, product))
        elif not product & 0x7fffffff:
            out.append((reason, 0))
            if product == SIGN:
                out.append((joined(reason, "OMOD kept -0"), SIGN))
        else:
            out.append((reason, product))
    return out


def clamped(bits):
    """CLAMP to [0, 1]. NaN passes through with MODE.DX10_CLAMP off, which
    isn't measured, so 0 passes too. The sign of a clamped zero isn't documented."""
    if is_nan(bits):
        return [(None, Float(QUIET)), ("NaN clamped to 0", Float(0))]
    value = to_float(bits)
    if value >= 1:
        return [(None, Float(ONE))]
    if value <= 0:
        return [(None, Float(0, any_zero=True))]
    return [(None, Float(bits))]


def finish(candidates, omod=1, clamp=False):
    """VOP3 result modifiers, OMOD then CLAMP, applied to each candidate."""
    options = []
    for reason, bits in candidates:
        for scale_reason, value in scaled(bits, omod):
            ends = clamped(value) if clamp else [(None, Float(value))]
            options += [(joined(reason, scale_reason, clamp_reason), want) for clamp_reason, want in ends]
    unique, seen = [], set()
    for reason, want in options:
        key = (want.bits, want.any_zero)
        if key not in seen:
            seen.add(key)
            unique.append((reason, want))
    return Open(unique)


def vop(base, arity, absolute=0, negate=0, omod=1, clamp=False, literal=None):
    """A model of a VOP1/VOP2/VOP3 form: source modifiers, then `base`, then
    result modifiers. `literal` replaces source 0."""
    def model(a, b, c):
        sources = [literal if literal is not None else a, b, c]
        sources = [modified(x, absolute >> j & 1, negate >> j & 1) for j, x in enumerate(sources)]
        return finish(base(*sources[:arity]), omod, clamp)
    return model


def mix(opsel, opsel_hi, negate=0, absolute=0, clamp=False):
    """v_fma_mix_f32: OPSEL_HI bit i makes source i an f16 from the half OPSEL
    bit i picks; otherwise it is the f32 word. NEG_HI is abs for this
    instruction, applied before NEG. One rounding of S0 * S1 + S2."""
    def model(a, b, c):
        sources = []
        for j, word in enumerate((a, b, c)):
            if opsel_hi >> j & 1:
                bits = half_bits(word >> 16 if opsel >> j & 1 else word)
            else:
                bits = flush(word)
            sources.append(modified(bits, absolute >> j & 1, negate >> j & 1))
        return finish(fused_or_twice(*sources), 1, clamp)
    return model


def mix_form(opsel, opsel_hi, negate=0, absolute=0, clamp=False):
    operands = []
    for j in range(3):
        operand = f"v{4 + j}"
        if absolute >> j & 1:
            operand = f"|{operand}|"
        if negate >> j & 1:
            operand = "-" + operand
        operands.append(operand)
    bits = lambda value: ",".join(str(value >> j & 1) for j in range(3))
    text = f"v_fma_mix_f32 v{{d}}, {', '.join(operands)} op_sel:[{bits(opsel)}] op_sel_hi:[{bits(opsel_hi)}]"
    return text + (" clamp" if clamp else "")


def operands(absolute=0, negate=0, first="v4", count=2):
    out = []
    for j, operand in enumerate([first, "v5", "v6"][:count]):
        if absolute >> j & 1:
            operand = f"|{operand}|"
        if negate >> j & 1:
            operand = "-" + operand
        out.append(operand)
    return ", ".join(out)


def suffix(omod=1, clamp=False):
    text = {1: "", 2: " mul:2", 4: " mul:4", 0.5: " div:2"}[omod]
    return text + (" clamp" if clamp else "")


def legacy_e64(absolute=0, negate=0, omod=1, clamp=False):
    label = f"v_mul_legacy_f32_e64 v{{d}}, {operands(absolute, negate)}{suffix(omod, clamp)}"
    return (label, "float", [label], vop(legacy_product, 2, absolute, negate, omod, clamp))


def fmac_e64(absolute=0, negate=0, omod=1, clamp=False, first="v4", literal=None):
    text = f"v_fmac_f32_e64 v{{d}}, {operands(absolute, negate, first)}{suffix(omod, clamp)}"
    return (text, "float", ["v_mov_b32_e32 v{d}, v6", text],
            vop(fused_flushed, 3, absolute, negate, omod, clamp, literal))


def fract_e64(absolute=0, negate=0, omod=1, clamp=False):
    text = f"v_fract_f32_e64 v{{d}}, {operands(absolute, negate, count=1)}{suffix(omod, clamp)}"
    return (text, "float", [text], vop(fraction, 1, absolute, negate, omod, clamp))


def shared_product(a, b, c):
    """The fused result, or the plain product in v15 on the rare lanes where
    the first and third inputs are equal (which keeps the product live)."""
    if a == c:
        return finish([(None, to_bits(to_float(flush(a)) * to_float(flush(b))))])
    return finish(fused_flushed(a, b, c))


KEEP_PRODUCT = ["v_cmp_eq_u32_e32 vcc_lo, v4, v6", "v_cndmask_b32_e32 v{d}, v{d}, v15, vcc_lo"]

# Scalar sources come from v4 and v5, read before the forms run.
PRELUDE = ["v_readfirstlane_b32 s20, v4", "v_readfirstlane_b32 s21, v5"]
QUARTER, THREE = 0x3e800000, 0x40400000

# (label, input kind, assembly with {d} for the destination, model of the result word)
FORMS = [
    # v_mul_legacy_f32. Register sources are in the arithmetic check.
    ("v_mul_legacy_f32 v{d}, s20, v5", "float", ["v_mul_legacy_f32 v{d}, s20, v5"], vop(legacy_product, 2)),
    ("v_mul_legacy_f32 v{d}, 0x3e800000, v5", "float", ["v_mul_legacy_f32 v{d}, 0x3e800000, v5"],
     vop(legacy_product, 2, literal=QUARTER)),
    ("v_mul_legacy_f32 v{d}, 0, v5", "float", ["v_mul_legacy_f32 v{d}, 0, v5"], vop(legacy_product, 2, literal=0)),
    legacy_e64(clamp=True),
    legacy_e64(negate=1),
    legacy_e64(negate=2),
    legacy_e64(negate=3),
    legacy_e64(negate=2, clamp=True),
    legacy_e64(absolute=1),
    legacy_e64(absolute=3),
    legacy_e64(absolute=1, clamp=True),
    legacy_e64(omod=2),
    legacy_e64(omod=0.5),
    # v_fmac_f32: D = S0 * S1 + D, rounded once
    ("v_fmac_f32 v{d}, s20, v5", "float", ["v_mov_b32_e32 v{d}, v6", "v_fmac_f32 v{d}, s20, v5"],
     vop(fused_flushed, 3)),
    ("v_fmac_f32 v{d}, 0x40400000, v5", "float", ["v_mov_b32_e32 v{d}, v6", "v_fmac_f32 v{d}, 0x40400000, v5"],
     vop(fused_flushed, 3, literal=THREE)),
    # The same product is also stored by v_mul_f32, so the host compiler can
    # compute it once. The sum must still be rounded once.
    ("v_fmac_f32 v{d}, v4, v5 (product also used)", "float",
     ["v_mul_f32 v15, v4, v5", "v_mov_b32_e32 v{d}, v6", "v_fmac_f32 v{d}, v4, v5", *KEEP_PRODUCT],
     shared_product),
    ("v_fma_f32 v{d}, v4, v5, v6 (product also used)", "float",
     ["v_mul_f32 v15, v4, v5", "v_fma_f32 v{d}, v4, v5, v6", *KEEP_PRODUCT], shared_product),
    fmac_e64(negate=1),
    fmac_e64(negate=2),
    fmac_e64(negate=3),
    fmac_e64(clamp=True),
    fmac_e64(negate=1, clamp=True),
    fmac_e64(absolute=1),
    fmac_e64(absolute=2, clamp=True),
    fmac_e64(omod=2),
    fmac_e64(omod=0.5),
    fmac_e64(first="0x3e800000", clamp=True, literal=QUARTER),
    # v_fract_f32
    ("v_fract_f32 v{d}, s20", "float", ["v_fract_f32 v{d}, s20"], vop(fraction, 1)),
    fract_e64(absolute=1),
    fract_e64(negate=1),
    fract_e64(omod=0.5),
    fract_e64(omod=2),
    # f32 input denormals: MODE 0xc0 reads them as zero.
    ("v_mul_legacy_f32 v{d}, v4, v5 (denormals)", "denormal", ["v_mul_legacy_f32 v{d}, v4, v5"],
     vop(legacy_product, 2)),
    ("v_fmac_f32 v{d}, v4, v5 (denormals)", "denormal", ["v_mov_b32_e32 v{d}, v6", "v_fmac_f32 v{d}, v4, v5"],
     vop(fused_flushed, 3)),
    ("v_fract_f32 v{d}, v4 (denormals)", "denormal", ["v_fract_f32 v{d}, v4"], vop(fraction, 1)),
    (mix_form(0, 0) + " (denormals)", "denormal", [mix_form(0, 0)], mix(0, 0)),
    # v_cvt_f32_ubyte0-3
    *[(f"v_cvt_f32_ubyte{k} v{{d}}, s20", "int", [f"v_cvt_f32_ubyte{k} v{{d}}, s20"], vop(byte_value(k), 1))
      for k in range(4)],
    *[(f"v_cvt_f32_ubyte0_sdwa v{{d}}, v4 src0_sel:BYTE_{k}", "int",
       [f"v_cvt_f32_ubyte0_sdwa v{{d}}, v4 dst_sel:DWORD dst_unused:UNUSED_PAD src0_sel:BYTE_{k}"],
       vop(byte_value(k), 1)) for k in range(4)],
    ("v_cvt_f32_ubyte1_sdwa v{d}, v4 src0_sel:WORD_1", "int",
     ["v_cvt_f32_ubyte1_sdwa v{d}, v4 dst_sel:DWORD dst_unused:UNUSED_PAD src0_sel:WORD_1"], vop(byte_value(3), 1)),
    ("v_cvt_f32_ubyte0_e64 v{d}, v4 mul:2", "int", ["v_cvt_f32_ubyte0_e64 v{d}, v4 mul:2"],
     vop(byte_value(0), 1, omod=2)),
    ("v_cvt_f32_ubyte2_e64 v{d}, v4 div:2", "int", ["v_cvt_f32_ubyte2_e64 v{d}, v4 div:2"],
     vop(byte_value(2), 1, omod=0.5)),
    ("v_cvt_f32_ubyte3_e64 v{d}, v4 clamp", "int", ["v_cvt_f32_ubyte3_e64 v{d}, v4 clamp"],
     vop(byte_value(3), 1, clamp=True)),
    # v_fma_mix_f32: every op_sel_hi, with every op_sel of the f16 sources,
    # plus one op_sel on f32 sources, which the ISA says is ignored.
    *[(mix_form(sel, hi), "mix", [mix_form(sel, hi)], mix(sel, hi))
      for hi in range(8) for sel in range(8) if not sel & ~hi],
    (mix_form(7, 0), "mix", [mix_form(7, 0)], mix(7, 0)),
    *[(mix_form(sel, hi, negate, absolute, clamp), "mix", [mix_form(sel, hi, negate, absolute, clamp)],
       mix(sel, hi, negate, absolute, clamp))
      for sel, hi, negate, absolute, clamp in [
          (4, 4, 4, 0, False), (0, 3, 4, 0, False), (1, 5, 4, 0, False), (0, 4, 1, 0, False),
          (0, 1, 2, 0, False), (0, 1, 6, 0, False), (0, 3, 3, 0, False),
          (0, 1, 0, 1, False), (0, 5, 0, 5, False), (0, 3, 1, 3, False), (2, 7, 0, 7, False),
          (0, 2, 0, 0, True), (4, 6, 0, 0, True), (0, 1, 0, 0, True), (1, 5, 0, 0, True),
          (0, 4, 1, 0, True), (0, 1, 2, 0, True), (0, 7, 0, 7, True)]],
]

# Known faults of the translation on one kind of host, which the check expects
# until they are fixed. Each is modelled exactly: on that host a result that
# differs from the console must be the modelled one, and the console's result
# there fails as "FIXED?" so the entry is removed with the fix. Entries are
# dict(hosts=(PCI vendor IDs,), forms={label: model of the faulty word}, cause=text).
KNOWN = {}


def known_for(adapter, label):
    """(fault, model) pairs of `KNOWN` for a form on this adapter."""
    vendor = adapter.lower().split(":")[0]
    return [(name, fault["forms"][label]) for name, fault in KNOWN.items()
            if vendor in fault["hosts"] and label in fault["forms"]]


# ----- Inputs -----

EDGE = [0, 1, 0x7f, 0x80, 0xff, 0x100, 0xff00, 0x8000ff, 0x12345678, 0x80000000, 0x7fffffff,
        0xffffffff, 0xff00ff00, 0x00ff00ff, 0xfefdfcfb, 0x01020304]
FLOAT_EDGE = [0x00000000, 0x80000000, 0x3f800000, 0xbf800000, 0x3f000000, 0xbf000000, 0x3fc00000,
              0xc0200000, 0x4b000000, 0x4b000001, 0xcb000001, 0x4f000000, 0x7f7fffff, 0xff7fffff,
              0x7f800000, 0xff800000, 0x7fc00000, 0xffc00000, 0x00800000, 0x80800000,
              # Around 1 and just below zero, where a fraction rounds up to 1.
              BELOW_ONE, 0xbf7fffff, 0x3f800001, 0xbf800001, 0xb3800000, 0xb3000000, 0xb2800000,
              0xbe800000, 0x40490fdb, 0xc0490fdb]
DENORMAL_EDGE = [0x00000001, 0x80000001, 0x007fffff, 0x807fffff, 0x00400000, 0x80400000, 0x00800000,
                 0x5f800000, 0xdf800000, 0x7f800000, 0x3f800000, 0x00000000, 0x80000000, 0x7fc00000,
                 0x01000000, 0x3e800000]
HALF_EDGE = [0x0000, 0x8000, 0x3c00, 0xbc00, 0x3800, 0x7bff, 0xfbff, 0x7c00, 0xfc00, 0x7e00,
             0x0001, 0x8001, 0x03ff, 0x0400, 0x3555, 0xc900, 0x5640, 0x1400]
# Whole input rows. Rounding once and rounding twice differ on each of the
# first five; the rest cover zero, infinity and NaN.
FLOAT_ROWS = [
    (0x3f800800, 0x3f800800, 0xbf801000),  # (1 + 2^-12)^2 - (1 + 2^-11) = 2^-24, 0 if rounded twice
    (0x3f800001, 0x3f800001, 0xbf800002),
    (0x7f7fffff, 0x40000000, 0xff7fffff),  # 2 MAX - MAX = MAX, infinity if rounded twice
    (0x3fffffff, 0x3fffffff, 0xc07ffffe),
    (0xbf800001, 0x3f7fffff, 0x3f800000),
    (0x00000000, 0x7f800000, 0x3f800000),  # 0 * inf: NaN, but +0 for the legacy product
    (0x80000000, 0x3f800000, 0x80000000),  # -0 * 1 + -0 = -0; legacy +0
    (0x80000000, 0x3f800000, 0x00000000),
    (0x7f800000, 0x3f800000, 0xff800000),  # inf - inf
    (0x00000000, 0x7fc00000, 0x3f800000),
    (0xc0000000, 0x00000000, 0x80000000),  # -2 * 0 + -0: -0, but the legacy product is +0
    (0x3f000000, 0xbf800000, 0x3f000000),  # exact zero sum: +0
]
DENORMAL_ROWS = [
    (0x00000001, 0x7f800000, 0x3f800000),  # flushed: legacy +0, fma 0 * inf = NaN
    (0x00400000, 0x5f800000, 0x00000000),  # 2^-127 * 2^64 is 2^-63 if kept, +0 if flushed
    (0x807fffff, 0x5f800000, 0x3f800000),
    (0x3f800000, 0x3f800000, 0x80000001),  # 1 * 1 + flushed denormal = 1
]


def half_word(rng):
    """Two f16 values of ordinary magnitude, both signs."""
    pick = lambda: rng.getrandbits(1) << 15 | rng.randint(5, 25) << 10 | rng.getrandbits(10)
    return pick() << 16 | pick()


def row(kind, index, rng):
    """Input words v4, v5 and v6 of one lane."""
    if kind == "int":
        if index < len(EDGE):
            return [EDGE[(index + 5 * j) % len(EDGE)] for j in range(3)]
        return [rng.getrandbits(32) for _ in range(3)]
    if kind == "mix":
        pairs = len(HALF_EDGE)
        if index < pairs * 2:
            return [HALF_EDGE[(index + 5 * j) % pairs] << 16 | HALF_EDGE[(index // 2 + 3 * j + 1) % pairs]
                    for j in range(3)]
        if rng.random() < 0.15:
            return [rng.getrandbits(32) for _ in range(3)]
        return [half_word(rng) for _ in range(3)]
    rows, edges = (FLOAT_ROWS, FLOAT_EDGE) if kind == "float" else (DENORMAL_ROWS, DENORMAL_EDGE)
    if index < len(rows):
        return list(rows[index])
    index -= len(rows)
    if index < len(edges):
        return [edges[(index + 7 * j) % len(edges)] for j in range(3)]
    if kind == "denormal":
        tiny = lambda: rng.getrandbits(1) << 31 | rng.getrandbits(23)
        big = lambda: to_bits(rng.choice((-1, 1)) * 2.0 ** rng.randint(20, 126))
        return [tiny(), big(), rng.choice((tiny(), big(), 0))]
    values = [to_bits(rng.uniform(-1, 1) * 2.0 ** rng.randint(-30, 30)) for _ in range(3)]
    roll = rng.random()
    if roll < 0.2:
        # Cancel the product, so the exactness of the rounding shows.
        values[2] = to_bits(-to_float(values[0]) * to_float(values[1]))
    elif roll < 0.3:
        values[1] = values[0]
    return values


# ----- Programs -----

def program(chunk):
    out = [f"v_mov_b32_e32 v{4 + j}, 0x{value:x}" for j, value in enumerate(INIT)] + PRELUDE
    for destination, form in enumerate(chunk, 7):
        out += [line.format(d=destination) for line in form[2]]
    return out + ["s_endpgm"]


def chunks_of(forms):
    """Shaders of up to eight forms, each holding one kind of input."""
    by_kind = {}
    for form in forms:
        by_kind.setdefault(form[1], []).append(form)
    for kind in sorted(by_kind):
        group = by_kind[kind]
        for start in range(0, len(group), OUTPUTS):
            yield group[start:start + OUTPUTS]


def probe(forms, emitter, llvm_mc):
    supported, refused = [], {}
    for form in forms:
        try:
            words = assemble(program([form]), llvm_mc)
        except (ValueError, subprocess.CalledProcessError) as error:
            refused[form[0]] = f"not assembled: {getattr(error, 'stderr', '') or error}"[:200]
            continue
        got = translate(emitter, words)
        if got.returncode == 65:
            refused[form[0]] = got.stderr.strip()
        elif got.returncode:
            raise RuntimeError(f"emitter failed on {form[0]}: {got.returncode} {got.stderr[-300:]}")
        else:
            supported.append(form)
    return supported, refused


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    for n in ("emitter", "llvm-mc", "dxc", "dxv", "provider", "proton", "prefix", "compiler-dir", "work", "output"):
        ap.add_argument("--" + n, type=Path, required=True)
    ap.add_argument("--adapter", required=True)
    ap.add_argument("--rounds", type=int, default=8, help=f"dispatches per shader, {GROUPS} inputs each")
    ap.add_argument("--only", action="append",
                    help="check only forms whose label contains this text (repeatable)")
    ap.add_argument("--no-gpu", action="store_true",
                    help="assemble, translate, compile and validate only; the receipt records no results")
    a = ap.parse_args()
    if a.output.exists():
        ap.error("choose a fresh receipt")
    a.work.mkdir(parents=True, exist_ok=False)
    rng = random.Random(0x464d4144)
    forms = [form for form in FORMS if not a.only or any(text in form[0] for text in a.only)]
    if not forms:
        ap.error("no form matches --only")
    supported, refused = probe(forms, a.emitter, a.llvm_mc)
    expected, chunks = {}, {}
    wrong, alternatives = {}, {}
    seen, fixed = {name: 0 for name in KNOWN}, {}
    with TemporaryDirectory(dir=a.work) as td:
        trial = Path(td)
        guest = ROOT / "tools/reference/independent-shaders/compute.cpp"
        (trial / "guest.cpp").write_text(guest.read_text())
        (trial / "context.h").write_text((guest.parent / "context.h").read_text().replace(
            '"../compute_buffers.h"', '"' + str(ROOT / "tools/reference/compute_buffers.h") + '"'))
        for number, chunk in enumerate(chunks_of(supported)):
            got = translate(a.emitter, assemble(program(chunk), a.llvm_mc))
            got.check_returncode()
            shader = trial / f"float{number:02}.hlsl"
            shader.write_text(instrument(got.stdout, len(chunk)))
            blob = shader.with_suffix(".cso")
            subprocess.run([a.dxc, "-T", "cs_6_0", "-E", "main", "-O3", "-Fo", blob, shader],
                           check=True, capture_output=True, timeout=30)
            subprocess.run([a.dxv, blob], check=True, capture_output=True, timeout=15)
            for turn in range(a.rounds):
                label = f"float{number:02}r{turn:02}"
                (trial / (label + ".cso")).write_bytes(blob.read_bytes())
                seed = [rng.getrandbits(32) for _ in range(384)]
                want = seed.copy()
                for lane in range(GROUPS):
                    values = row(chunk[0][1], turn * GROUPS + lane, rng)
                    offset = 96 + STRIDE * lane
                    seed[offset:offset + 3] = values
                    want[offset:offset + 3] = values
                    want[offset + 3:offset + 3 + len(chunk)] = [form[3](*values) for form in chunk]
                (trial / (label + ".seed")).write_bytes(struct.pack("<384I", *seed))
                expected[label] = want
                chunks[label] = chunk
        if a.no_gpu:
            print(f"COMPILED {len(supported)} forms in {len(set(map(id, chunks.values())))} shaders, "
                  f"{len(refused)} refused; no GPU run")
            for name, reason in refused.items():
                print(f"refused {name}: {reason[:150]}")
            return 0
        (trial / "cases.txt").write_text("".join(f"{n} 4 structured {GROUPS}\n" for n in expected))
        _, status, fields = run_gpu_guest(trial / "guest.cpp", "float-forms.exe", trial, a.work, a.proton,
                                          a.prefix, a.provider, a.compiler_dir)
        if status or fields.get("adapter") != a.adapter:
            raise RuntimeError(f"guest failed: {status} {fields}")
        for label, chunk in chunks.items():
            observed = struct.unpack("<384I", (trial / (label + ".bin")).read_bytes())
            wanted = expected[label]
            for index, (got_word, want_word) in enumerate(zip(observed, wanted)):
                lane, column = divmod(index - 96, STRIDE)
                inside = 0 <= lane < GROUPS and 3 <= column < 3 + len(chunk)
                name = chunk[column - 3][0] if inside else "guard"
                inputs = list(wanted[96 + STRIDE * lane:99 + STRIDE * lane])
                faults = [(fault, model(*inputs)) for fault, model in known_for(a.adapter, name)] if inside else []
                faults = [(fault, bits) for fault, bits in faults if not want_word.matches(bits)]
                if faults:
                    if any(got_word == bits for _, bits in faults):
                        for fault, bits in faults:
                            seen[fault] += got_word == bits
                        continue
                    if matches_any(got_word, want_word):
                        for fault, _ in faults:
                            fixed.setdefault(fault, []).append(name)
                        continue
                if matches_any(got_word, want_word):
                    reason = want_word.reason(got_word) if isinstance(want_word, Open) else None
                    if reason:
                        counts = alternatives.setdefault(name, {})
                        counts[reason] = counts.get(reason, 0) + 1
                    continue
                detail = {"shader": label, "inputs": [f"{w:#010x}" for w in wanted[96 + STRIDE * lane:99 + STRIDE * lane]],
                          "got": f"{got_word:#010x}", "wanted": repr(want_word)}
                entry = wrong.setdefault(name, {"count": 0, "first": []})
                entry["count"] += 1
                if len(entry["first"]) < 4:
                    entry["first"].append(detail)
    receipt = {"schema": 1, "adapter": a.adapter, "mode": "0xc0",
               "tools_sha256": {n: digest(getattr(a, n)) for n in ("emitter", "dxc", "provider")},
               "checked": [form[0] for form in supported], "refused": refused, "wrong": wrong,
               # Results counted per form that matched an alternative, not the first expectation.
               "alternatives": alternatives,
               "known_faults": {name: dict(cause=KNOWN[name]["cause"], results=count) for name, count in seen.items()
                                if any(a.adapter.lower().startswith(host) for host in KNOWN[name]["hosts"])},
               "known_faults_not_seen": fixed, "inputs_per_form": a.rounds * GROUPS}
    a.output.write_text(json.dumps(receipt, indent=2) + "\n")
    for name, reason in refused.items():
        print(f"refused {name}: {reason[:150]}")
    for name, counts in alternatives.items():
        print(f"alternative {name}: " + ", ".join(f"{reason} x{n}" for reason, n in counts.items()))
    for name, detail in wrong.items():
        print(f"WRONG {name}: {detail['count']} results, first {detail['first'][0]}")
    for fault, names in fixed.items():
        print(f"FIXED? {sorted(set(names))[0]} matches the console, not the known fault {fault}: update KNOWN")
    known = "; ".join(f"{count} results show the known fault: {KNOWN[fault]['cause']}"
                      for fault, count in seen.items() if count)
    failed = bool(wrong or fixed)
    print(f"{'FAIL' if failed else 'PASS'} {len(supported)} forms on {a.rounds * GROUPS} inputs each, "
          f"{len(wrong)} wrong, {len(fixed)} known faults not seen, {len(refused)} refused"
          + (f". {known}" if known else ""))
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
