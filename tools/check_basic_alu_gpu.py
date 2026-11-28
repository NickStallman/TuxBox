#!/usr/bin/env python3
"""LLVM-assembled common vector, scalar, float and comparison controls on a D3D12 GPU.

Each instruction is translated, run on dynamic edge and random inputs and
compared with an independent model of the console instruction. Instructions
the translator refuses are listed in the receipt; a wrong result fails the
check. Synthetic inputs and guard words; no game is used.
"""
import argparse
from fractions import Fraction
import json
import math
import operator
from pathlib import Path
import random
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from check_packed_integer_gpu import assemble, digest, instrument, translate, INIT, STRIDE, GROUPS  # noqa: E402
from check_half_exact_gpu import EDGE_F16, encode_half, f32_value, half_value  # noqa: E402
from frosty_gpu_guest import run_gpu_guest  # noqa: E402

M = 0xffffffff
EDGE = [0, 1, 2, 31, 32, 33, 0xffff, 0x10000, 0x7fffff, 0x800000, 0xffffff, 0x1000000,
        0x7fffffff, 0x80000000, 0x80000001, 0xfffffffe, 0xffffffff, 0xaaaaaaaa]


def s32(value):
    return value - (1 << 32) if value >> 31 else value


def s24(value):
    value &= 0xffffff
    return value - (1 << 24) if value >> 23 else value


def leading_zeros(value):
    return 32 - value.bit_length()


def bit_field(a, b, c):
    return (a >> (b & 31)) & ((1 << (c & 31)) - 1)


def signed_bit_field(a, b, c):
    offset, width = b & 31, c & 31
    if not width:
        return 0
    if offset + width < 32:
        field = bit_field(a, b, c)
        return (field - (1 << width) if field >> (width - 1) else field) & M
    return (s32(a) >> offset) & M


def float_bits(value):
    return struct.unpack("<I", struct.pack("<f", float(value)))[0]


def byte_permute(a, b, selectors):
    """`v_perm_b32`: each selector byte picks a byte of {S0, S1}, a sign fill or a constant.
    S1 holds bytes 0 to 3 and S0 bytes 4 to 7."""
    source = (a << 32) | b
    out = 0
    for position in range(4):
        select = (selectors >> (8 * position)) & 0xff
        if select >= 13:
            byte = 0xff
        elif select == 12:
            byte = 0
        elif select >= 8:
            byte = 0xff * ((source >> (16 * (select - 8) + 15)) & 1)
        else:
            byte = (source >> (8 * select)) & 0xff
        out |= byte << (8 * position)
    return out


def median(values):
    return sorted(values)[1]


# name: (source count, model of the 32-bit result)
OPERATIONS = {
    "v_not_b32": (1, lambda a, b, c: ~a & M),
    "v_bfrev_b32": (1, lambda a, b, c: int(f"{a:032b}"[::-1], 2)),
    "v_ffbh_u32": (1, lambda a, b, c: leading_zeros(a) if a else M),
    "v_ffbl_b32": (1, lambda a, b, c: (a & -a).bit_length() - 1 if a else M),
    "v_ffbh_i32": (1, lambda a, b, c: M if a in (0, M) else leading_zeros(a ^ M if a >> 31 else a)),
    "v_cvt_f32_u32": (1, lambda a, b, c: float_bits(a)),
    "v_cvt_f32_i32": (1, lambda a, b, c: float_bits(s32(a))),
    "v_add_nc_u32": (2, lambda a, b, c: (a + b) & M),
    "v_sub_nc_u32": (2, lambda a, b, c: (a - b) & M),
    "v_subrev_nc_u32": (2, lambda a, b, c: (b - a) & M),
    "v_and_b32": (2, lambda a, b, c: a & b),
    "v_or_b32": (2, lambda a, b, c: a | b),
    "v_xor_b32": (2, lambda a, b, c: a ^ b),
    "v_xnor_b32": (2, lambda a, b, c: ~(a ^ b) & M),
    "v_lshlrev_b32": (2, lambda a, b, c: (b << (a & 31)) & M),
    "v_lshrrev_b32": (2, lambda a, b, c: b >> (a & 31)),
    "v_ashrrev_i32": (2, lambda a, b, c: (s32(b) >> (a & 31)) & M),
    "v_min_u32": (2, lambda a, b, c: min(a, b)),
    "v_max_u32": (2, lambda a, b, c: max(a, b)),
    "v_min_i32": (2, lambda a, b, c: min(s32(a), s32(b)) & M),
    "v_max_i32": (2, lambda a, b, c: max(s32(a), s32(b)) & M),
    "v_mul_u32_u24": (2, lambda a, b, c: ((a & 0xffffff) * (b & 0xffffff)) & M),
    "v_mul_i32_i24": (2, lambda a, b, c: (s24(a) * s24(b)) & M),
    "v_mul_hi_u32_u24": (2, lambda a, b, c: (((a & 0xffffff) * (b & 0xffffff)) >> 32) & M),
    "v_mul_hi_i32_i24": (2, lambda a, b, c: ((s24(a) * s24(b)) >> 32) & M),
    "v_mul_lo_u32": (2, lambda a, b, c: (a * b) & M),
    "v_mul_hi_u32": (2, lambda a, b, c: (a * b) >> 32),
    "v_mul_hi_i32": (2, lambda a, b, c: ((s32(a) * s32(b)) >> 32) & M),
    "v_bfe_u32": (3, bit_field),
    "v_bfe_i32": (3, signed_bit_field),
    "v_bfi_b32": (3, lambda a, b, c: ((a & b) | (~a & c)) & M),
    "v_alignbit_b32": (3, lambda a, b, c: (((a << 32) | b) >> (c & 31)) & M),
    "v_alignbyte_b32": (3, lambda a, b, c: (((a << 32) | b) >> (8 * (c & 3))) & M),
    "v_add3_u32": (3, lambda a, b, c: (a + b + c) & M),
    "v_lshl_add_u32": (3, lambda a, b, c: ((a << (b & 31)) + c) & M),
    "v_add_lshl_u32": (3, lambda a, b, c: ((a + b) << (c & 31)) & M),
    "v_lshl_or_b32": (3, lambda a, b, c: ((a << (b & 31)) | c) & M),
    "v_and_or_b32": (3, lambda a, b, c: (a & b) | c),
    "v_or3_b32": (3, lambda a, b, c: a | b | c),
    "v_xor3_b32": (3, lambda a, b, c: a ^ b ^ c),
    "v_min3_u32": (3, lambda a, b, c: min(a, b, c)),
    "v_max3_u32": (3, lambda a, b, c: max(a, b, c)),
    "v_med3_u32": (3, lambda a, b, c: median([a, b, c])),
    "v_min3_i32": (3, lambda a, b, c: min(s32(a), s32(b), s32(c)) & M),
    "v_max3_i32": (3, lambda a, b, c: max(s32(a), s32(b), s32(c)) & M),
    "v_med3_i32": (3, lambda a, b, c: median([s32(a), s32(b), s32(c)]) & M),
    "v_mad_u32_u24": (3, lambda a, b, c: ((a & 0xffffff) * (b & 0xffffff) + c) & M),
    "v_mad_i32_i24": (3, lambda a, b, c: (s24(a) * s24(b) + s32(c)) & M),
    "v_sad_u32": (3, lambda a, b, c: (abs(a - b) + c) & M),
    "v_xad_u32": (3, lambda a, b, c: ((a ^ b) + c) & M),
    "v_bcnt_u32_b32": (2, lambda a, b, c: (bin(a).count("1") + b) & M),
    "v_bfm_b32": (2, lambda a, b, c: (((1 << (a & 31)) - 1) << (b & 31)) & M),
    "v_perm_b32": (3, lambda a, b, c: byte_permute(a, b, c)),
    "v_pack_b32_f16": (2, lambda a, b, c: (a & 0xffff) | (b & 0xffff) << 16),
    **{f"v_cvt_f32_ubyte{n}": (1, lambda a, b, c, n=n: float_bits((a >> (8 * n)) & 0xff)) for n in range(4)},
}


def signed_overflow(value):
    return not -2**31 <= value < 2**31


def scalar_field(a, b, signed):
    """`s_bfe`: the offset is in bits 4:0 of the second source, the width in 22:16."""
    offset, width = b & 31, min((b >> 16) & 127, 32)
    if not width:
        return 0
    if not signed:
        return (a >> offset) & ((1 << width) - 1)
    if offset + width < 32:
        field = (a >> offset) & ((1 << width) - 1)
        return (field - (1 << width) if field >> (width - 1) else field) & M
    return (s32(a) >> offset) & M


def nonzero(value):
    return value, int(value != 0)


# name: (source count, model returning the result and SCC, or None when the
# instruction leaves SCC alone)
SCALAR = {
    "s_add_u32": (2, lambda a, b: ((a + b) & M, int(a + b > M))),
    "s_sub_u32": (2, lambda a, b: ((a - b) & M, int(b > a))),
    "s_add_i32": (2, lambda a, b: ((a + b) & M, int(signed_overflow(s32(a) + s32(b))))),
    "s_sub_i32": (2, lambda a, b: ((a - b) & M, int(signed_overflow(s32(a) - s32(b))))),
    "s_min_u32": (2, lambda a, b: (min(a, b), int(a < b))),
    "s_max_u32": (2, lambda a, b: (max(a, b), int(a > b))),
    "s_min_i32": (2, lambda a, b: (min(s32(a), s32(b)) & M, int(s32(a) < s32(b)))),
    "s_max_i32": (2, lambda a, b: (max(s32(a), s32(b)) & M, int(s32(a) > s32(b)))),
    "s_and_b32": (2, lambda a, b: nonzero(a & b)),
    "s_or_b32": (2, lambda a, b: nonzero(a | b)),
    "s_xor_b32": (2, lambda a, b: nonzero(a ^ b)),
    "s_andn2_b32": (2, lambda a, b: nonzero(a & ~b & M)),
    "s_orn2_b32": (2, lambda a, b: nonzero((a | ~b) & M)),
    "s_nand_b32": (2, lambda a, b: nonzero(~(a & b) & M)),
    "s_nor_b32": (2, lambda a, b: nonzero(~(a | b) & M)),
    "s_xnor_b32": (2, lambda a, b: nonzero(~(a ^ b) & M)),
    "s_lshl_b32": (2, lambda a, b: nonzero((a << (b & 31)) & M)),
    "s_lshr_b32": (2, lambda a, b: nonzero(a >> (b & 31))),
    "s_ashr_i32": (2, lambda a, b: nonzero((s32(a) >> (b & 31)) & M)),
    "s_bfm_b32": (2, lambda a, b: ((((1 << (a & 31)) - 1) << (b & 31)) & M, None)),
    "s_mul_i32": (2, lambda a, b: ((a * b) & M, None)),
    "s_mul_hi_u32": (2, lambda a, b: ((a * b) >> 32, None)),
    "s_mul_hi_i32": (2, lambda a, b: (((s32(a) * s32(b)) >> 32) & M, None)),
    "s_bfe_u32": (2, lambda a, b: nonzero(scalar_field(a, b, False))),
    "s_bfe_i32": (2, lambda a, b: nonzero(scalar_field(a, b, True))),
    "s_lshl1_add_u32": (2, lambda a, b: (((a << 1) + b) & M, int((a << 1) + b > M))),
    "s_lshl2_add_u32": (2, lambda a, b: (((a << 2) + b) & M, int((a << 2) + b > M))),
    "s_lshl3_add_u32": (2, lambda a, b: (((a << 3) + b) & M, int((a << 3) + b > M))),
    "s_lshl4_add_u32": (2, lambda a, b: (((a << 4) + b) & M, int((a << 4) + b > M))),
    "s_pack_ll_b32_b16": (2, lambda a, b: ((a & 0xffff) | (b & 0xffff) << 16, None)),
    "s_pack_lh_b32_b16": (2, lambda a, b: ((a & 0xffff) | (b & 0xffff0000), None)),
    "s_pack_hh_b32_b16": (2, lambda a, b: ((a >> 16) | (b & 0xffff0000), None)),
    "s_not_b32": (1, lambda a: nonzero(~a & M)),
    "s_brev_b32": (1, lambda a: (int(f"{a:032b}"[::-1], 2), None)),
    "s_bcnt0_i32_b32": (1, lambda a: nonzero(32 - bin(a).count("1"))),
    "s_bcnt1_i32_b32": (1, lambda a: nonzero(bin(a).count("1"))),
    "s_ff1_i32_b32": (1, lambda a: ((a & -a).bit_length() - 1 if a else M, None)),
    "s_flbit_i32_b32": (1, lambda a: (leading_zeros(a) if a else M, None)),
    "s_sext_i32_i8": (1, lambda a: (((a & 0xff) - (0x100 if a & 0x80 else 0)) & M, None)),
    "s_sext_i32_i16": (1, lambda a: (((a & 0xffff) - (0x10000 if a & 0x8000 else 0)) & M, None)),
    "s_abs_i32": (1, lambda a: nonzero(abs(s32(a)) & M)),
    "s_wqm_b32": (1, lambda a: nonzero(quads(a))),
    # SCC is 1 before each instruction, so these two carry or borrow one in.
    "s_addc_u32": (2, lambda a, b: ((a + b + 1) & M, int(a + b + 1 > M))),
    "s_subb_u32": (2, lambda a, b: ((a - b - 1) & M, int(b + 1 > a))),
}
M64 = (1 << 64) - 1


def halves64(value):
    return value & M, value >> 32 & M


def s64(value):
    return value - (1 << 64) if value >> 63 else value


def flagged(value):
    return value & M64, int(value & M64 != 0)


# 64-bit scalar instructions: model of X, Y and the entry SCC, returning the
# result and SCC, or None for an SCC left alone. A shift takes Y's low word.
PAIRS = {
    "s_and_b64": lambda x, y, scc: flagged(x & y),
    "s_or_b64": lambda x, y, scc: flagged(x | y),
    "s_xor_b64": lambda x, y, scc: flagged(x ^ y),
    "s_andn2_b64": lambda x, y, scc: flagged(x & ~y),
    "s_orn2_b64": lambda x, y, scc: flagged(x | ~y),
    "s_nand_b64": lambda x, y, scc: flagged(~(x & y)),
    "s_nor_b64": lambda x, y, scc: flagged(~(x | y)),
    "s_xnor_b64": lambda x, y, scc: flagged(~(x ^ y)),
    "s_lshl_b64": lambda x, y, scc: flagged(x << (y & 63)),
    "s_lshr_b64": lambda x, y, scc: flagged(x >> (y & 63)),
    "s_ashr_i64": lambda x, y, scc: flagged(s64(x) >> (y & 63)),
    "s_cselect_b64": lambda x, y, scc: (x if scc else y, None),
    "s_cmp_eq_u64": lambda x, y, scc: int(x == y),
    "s_cmp_lg_u64": lambda x, y, scc: int(x != y),
}
# Group memory, in one thread: v20 is an address from the first input, and the
# second and third inputs are data. `{0}` and `{1}` are the result registers.
# name: (instructions, model of the results)
WRITE, READ = "ds_write_b32 v20, v5", "ds_read_b32 {0}, v20"
GROUP_MEMORY = {
    "ds_or_b32": ([WRITE, "ds_or_b32 v20, v6", READ], lambda b, c: [b | c]),
    "ds_and_b32": ([WRITE, "ds_and_b32 v20, v6", READ], lambda b, c: [b & c]),
    "ds_xor_b32": ([WRITE, "ds_xor_b32 v20, v6", READ], lambda b, c: [b ^ c]),
    "ds_add_u32": ([WRITE, "ds_add_u32 v20, v6", READ], lambda b, c: [(b + c) & M]),
    "ds_min_u32": ([WRITE, "ds_min_u32 v20, v6", READ], lambda b, c: [min(b, c)]),
    "ds_max_u32": ([WRITE, "ds_max_u32 v20, v6", READ], lambda b, c: [max(b, c)]),
    # The returning form also gives the word as it was.
    "ds_add_rtn_u32": ([WRITE, "ds_add_rtn_u32 {1}, v20, v6", READ], lambda b, c: [(b + c) & M, b]),
    "ds_write2_b32": (["ds_write2_b32 v20, v5, v6 offset1:1", READ, "ds_read_b32 {1}, v20 offset:4"],
                      lambda b, c: [b, c]),
    "ds_read2_b32": ([WRITE, "ds_write_b32 v20, v6 offset:4", "ds_read2_b32 v[{0n}:{1n}], v20 offset1:1"],
                     lambda b, c: [b, c]),
    # The second offset counts in units of 64 words.
    "ds_write2st64_b32": (["ds_write2st64_b32 v20, v5, v6 offset1:1", READ, "ds_read_b32 {1}, v20 offset:256"],
                          lambda b, c: [b, c]),
    "ds_read2st64_b32": ([WRITE, "ds_write_b32 v20, v6 offset:256", "ds_read2st64_b32 v[{0n}:{1n}], v20 offset1:1"],
                         lambda b, c: [b, c]),
    "ds_write_b16": ([WRITE, "ds_write_b16 v20, v6", READ], lambda b, c: [(b & 0xffff0000) | (c & 0xffff)]),
    "ds_write_b8": ([WRITE, "ds_write_b8 v20, v6", READ], lambda b, c: [(b & 0xffffff00) | (c & 0xff)]),
    "ds_read_u16": ([WRITE, "ds_read_u16 {0}, v20"], lambda b, c: [b & 0xffff]),
    "ds_read_u8": ([WRITE, "ds_read_u8 {0}, v20"], lambda b, c: [b & 0xff]),
    "ds_read_i16": ([WRITE, "ds_read_i16 {0}, v20"], lambda b, c: [((b & 0xffff) - (0x10000 if b & 0x8000 else 0)) & M]),
    "ds_read_i8": ([WRITE, "ds_read_i8 {0}, v20"], lambda b, c: [((b & 0xff) - (0x100 if b & 0x80 else 0)) & M]),
    # The D16_HI forms use the high half of the register and leave its low half.
    "ds_read_u16_d16_hi": ([WRITE, "v_mov_b32_e32 {0}, v6", "ds_read_u16_d16_hi {0}, v20"],
                           lambda b, c: [(b & 0xffff) << 16 | (c & 0xffff)]),
    "ds_write_b16_d16_hi": ([WRITE, "ds_write_b16_d16_hi v20, v6", READ], lambda b, c: [(b & 0xffff0000) | c >> 16]),
}
def quads(a):
    """`s_wqm`: every bit of a group of four is set where any of them is."""
    return sum(0xf << shift for shift in range(0, 32, 4) if a >> shift & 0xf)


def borrow(minuend, subtrahend, carry):
    return [(minuend - subtrahend - carry) & M, int(subtrahend + carry > minuend)]


# Vector forms that need more than one instruction to set up or to read.
# name: (instructions with `{0}` and `{1}` the result registers, model of the three inputs)
CARRY = "v_cmp_lt_u32_e32 vcc_lo, v4, v6"
CARRY_OUT = "v_cndmask_b32_e64 {1}, 0, 1, vcc_lo"
SEQUENCES = {
    # A long jump: s_getpc_b64 gives the next instruction's address, and the jump
    # lands 16 bytes after it, past a move that would replace the result.
    "s_setpc_b64": (["v_mov_b32_e32 {0}, v5", "s_getpc_b64 s[20:21]", "s_add_u32 s20, s20, 16",
                     "s_addc_u32 s21, s21, 0", "s_setpc_b64 s[20:21]", "v_mov_b32_e32 {0}, v4", "s_nop 0"],
                    lambda a, b, c: [b]),
    # The count is the first source; the value is the pair in the next two.
    "v_lshlrev_b64": (["v_lshlrev_b64 v[{0n}:{1n}], v4, v[5:6]"],
                      lambda a, b, c: list(halves64((b | c << 32) << (a & 63)))),
    "v_lshrrev_b64": (["v_lshrrev_b64 v[{0n}:{1n}], v4, v[5:6]"],
                      lambda a, b, c: list(halves64((b | c << 32) >> (a & 63)))),
    "v_ashrrev_i64": (["v_ashrrev_i64 v[{0n}:{1n}], v4, v[5:6]"],
                      lambda a, b, c: list(halves64(s64(b | c << 32) >> (a & 63)))),
    # VCC holds a carry of 1 where the first input is below the third.
    "v_add_co_ci_u32": ([CARRY, "v_add_co_ci_u32_e32 {0}, vcc_lo, v4, v5, vcc_lo", CARRY_OUT],
                        lambda a, b, c: [(a + b + (a < c)) & M, int(a + b + (a < c) > M)]),
    "v_sub_co_ci_u32": ([CARRY, "v_sub_co_ci_u32_e32 {0}, vcc_lo, v4, v5, vcc_lo", CARRY_OUT],
                        lambda a, b, c: borrow(a, b, int(a < c))),
    "v_subrev_co_ci_u32": ([CARRY, "v_subrev_co_ci_u32_e32 {0}, vcc_lo, v4, v5, vcc_lo", CARRY_OUT],
                           lambda a, b, c: borrow(b, a, int(a < c))),
    "v_swap_b32": (["v_mov_b32_e32 {0}, v4", "v_mov_b32_e32 {1}, v5", "v_swap_b32 {0}, {1}"], lambda a, b, c: [b, a]),
    # The pairs are {second, third} and {first, third}, equal where the first two inputs are.
    **{f"v_cmp_{name}_u64": (["v_mov_b32_e32 v21, v4", "v_mov_b32_e32 v22, v6", f"v_cmp_{name}_u64_e32 vcc_lo, v[5:6], v[21:22]",
                              "v_cndmask_b32_e64 {0}, 0, 1, vcc_lo"],
                             lambda a, b, c, name=name: [int(RELATIONS[name](b | c << 32, a | c << 32))])
       for name in ("eq", "ne", "lt", "gt", "le", "ge")},
    **{f"v_cmpx_{name}_u64": (["v_mov_b32_e32 v21, v4", "v_mov_b32_e32 v22, v6", "v_mov_b32_e32 {0}, 0", "s_mov_b32 s26, exec_lo",
                               f"v_cmpx_{name}_u64_e32 v[5:6], v[21:22]", "v_mov_b32_e32 {0}, 1", "s_mov_b32 exec_lo, s26"],
                              lambda a, b, c, name=name: [int(RELATIONS[name](b | c << 32, a | c << 32))])
       for name in ("eq", "ne", "lt", "gt", "le", "ge")},
    # A hint and a wait change nothing a program can read.
    "s_inst_prefetch": (["s_inst_prefetch 0x3", "v_mov_b32_e32 {0}, v4"], lambda a, b, c: [a]),
    "s_waitcnt_vscnt": (["s_waitcnt_vscnt null, 0x0", "v_mov_b32_e32 {0}, v5"], lambda a, b, c: [b]),
    # EXEC becomes the source without the lanes EXEC held, and the destination gets
    # a copy. From an empty EXEC that is the comparison's mask.
    "s_andn2_wrexec_b64": (["v_mov_b32_e32 {0}, 0", "s_mov_b64 s[26:27], exec", "v_cmp_lt_u32_e32 vcc_lo, v4, v5",
                            "s_mov_b64 exec, 0", "s_andn2_wrexec_b64 s[30:31], vcc", "v_mov_b32_e32 {0}, 1",
                            "s_mov_b64 exec, s[26:27]", "s_and_b32 s30, s30, 1", "v_mov_b32_e32 {1}, s30"],
                           lambda a, b, c: [int(a < b), int(a < b)]),
    # M0 is added to the register number: v4 + 1 is v5, and v7 + 1 the second result.
    "v_movrels_b32": (["s_mov_b32 m0, 1", "v_movrels_b32 {0}, v4"], lambda a, b, c: [b]),
    "v_movreld_b32": (["s_mov_b32 m0, 1", "v_mov_b32_e32 {0}, v6", "v_movreld_b32 {0}, v4"], lambda a, b, c: [c, a]),
}
OUTPUTS = 8

INFINITY = float("inf")


def to_float(bits):
    return struct.unpack("<f", struct.pack("<I", bits))[0]


def to_bits(value):
    """The nearest f32 of a Python float, rounding to even and overflowing to infinity."""
    try:
        return struct.unpack("<I", struct.pack("<f", value))[0]
    except OverflowError:
        return 0xff800000 if value < 0 else 0x7f800000


def is_nan(bits):
    return bits & 0x7fffffff > 0x7f800000


def is_denormal(bits):
    return 0 < bits & 0x7fffffff < 0x00800000


class Float:
    """An expected f32. Any NaN matches a NaN. A denormal may be flushed to a
    zero of the same sign, as the float mode decides. `ulps` allows for
    approximate console instructions, and `any_zero` for an undefined zero sign."""

    def __init__(self, bits, ulps=0, any_zero=False, absolute=0.0, also=()):
        self.bits, self.ulps, self.any_zero = bits, ulps, any_zero
        # `absolute` allows an error in value, for results near zero. `also`
        # lists other results the documentation leaves open.
        self.absolute, self.also = absolute, also

    def matches(self, got):
        want = self.bits
        if is_nan(want):
            return is_nan(got)
        if got == want or (is_denormal(want) and got == want & 0x80000000):
            return True
        if self.any_zero and not want & 0x7fffffff and not got & 0x7fffffff:
            return True
        if got in self.also:
            return True
        if self.absolute and not is_nan(got) and got & 0x7fffffff < 0x7f800000 \
                and abs(to_float(got) - to_float(want)) <= self.absolute:
            return True
        same_sign = not (got ^ want) & 0x80000000
        return same_sign and not is_nan(got) and abs((got & 0x7fffffff) - (want & 0x7fffffff)) <= self.ulps

    def __repr__(self):
        return f"{self.bits:#010x}" + (f"~{self.ulps}ulp" if self.ulps else "")


def distance(got, want):
    """How many representable floats lie between a result and an approximate
    instruction's exact one, or None where that has no meaning."""
    if not isinstance(want, Float) or not want.ulps or is_nan(want.bits) or is_nan(got) or (got ^ want.bits) >> 31 \
            or want.bits & 0x7fffffff < 0x00800000 or 0x7f800000 in (got & 0x7fffffff, want.bits & 0x7fffffff):
        return None
    return abs((got & 0x7fffffff) - (want.bits & 0x7fffffff))


def matches(got, want):
    return got == want if isinstance(want, int) else want.matches(got)


def fused(a, b, c):
    """a * b + c rounded once."""
    values = [to_float(x) for x in (a, b, c)]
    if not all(math.isfinite(v) for v in values):
        return to_bits(values[0] * values[1] + values[2])
    exact = Fraction(values[0]) * Fraction(values[1]) + Fraction(values[2])
    if not exact:
        return to_bits(values[0] * values[1] + values[2])
    near = to_bits(float(exact))
    candidates = [near + step for step in (-1, 0, 1)
                  if 0 <= (near & 0x7fffffff) + step <= 0x7f7fffff]

    def distance(bits):
        return abs(Fraction(to_float(bits)) - exact), bits & 1

    if near & 0x7fffffff >= 0x7f800000:
        return near
    return min(candidates, key=distance)


def arithmetic(function, ulps=0):
    def model(a, b, c):
        try:
            return Float(to_bits(function(to_float(a), to_float(b), to_float(c))), ulps)
        except (ValueError, ZeroDivisionError):
            return Float(0x7fc00000)
    return model


def extreme(pick, count):
    """min or max of `count` sources, ignoring NaN as the console's IEEE form does."""
    def model(a, b, c):
        numbers = [to_float(x) for x in (a, b, c)[:count] if not is_nan(x)]
        return Float(to_bits(pick(numbers)) if numbers else 0x7fc00000, any_zero=True)
    return model


def whole(function):
    def model(a, b, c):
        value = to_float(a)
        if not math.isfinite(value):
            return Float(a)
        result = function(value)
        # A result of zero keeps the input's sign.
        return Float(to_bits(math.copysign(0.0, value) if result == 0 else float(result)))
    return model


def to_integer(function, low, high):
    def model(a, b, c):
        value = to_float(a)
        if math.isnan(value):
            return 0
        if math.isinf(value):
            return (high if value > 0 else low) & M
        return max(low, min(high, function(value))) & M
    return model


def legacy_multiply(a, b, c):
    """DX9 rules: zero times anything, infinity and NaN included, is zero."""
    if not a & 0x7fffffff or not b & 0x7fffffff:
        return Float(0, any_zero=True)
    return Float(to_bits(to_float(a) * to_float(b)))


def fraction(a, b, c):
    """`x - floor(x)`. A tiny negative input rounds to 1.0; the manual doesn't say
    whether the console returns that or the float below it, so either passes."""
    value = to_float(a)
    if not math.isfinite(value):
        return Float(0x7fc00000)
    result = to_bits(float(Fraction(value) - math.floor(value)))
    return Float(result, any_zero=True, also=(0x3f7fffff,) if result == 0x3f800000 else ())


def turns(function, at_zero):
    """Sine or cosine of an angle in turns. The manual gives no accuracy, so a
    small error in value passes. An input too large to hold a fraction is whole."""
    def model(a, b, c):
        value = to_float(a)
        if not math.isfinite(value):
            return Float(0x7fc00000)
        part = Fraction(value) - math.floor(value)
        result = function(2 * math.pi * float(part)) if part else at_zero
        return Float(to_bits(result), any_zero=True, absolute=TRIG_ERROR)
    return model


def cube(z_major, y_major, x_major):
    """A cube map coordinate, chosen by the axis of largest magnitude as the manual lists it."""
    def model(a, b, c):
        x, y, z = (to_float(v) for v in (a, b, c))
        if abs(z) >= abs(x) and abs(z) >= abs(y):
            result = z_major(x, y, z)
        elif abs(y) >= abs(x):
            result = y_major(x, y, z)
        else:
            result = x_major(x, y, z)
        return Float(to_bits(result))
    return model


def number_class(bits):
    """The bit of `v_cmp_class_f32`'s mask that names this float's class."""
    negative, magnitude = bits >> 31, bits & 0x7fffffff
    if magnitude > 0x7f800000:
        return 1 << ((magnitude >> 22) & 1)
    kind = 0 if magnitude == 0x7f800000 else 1 if magnitude >= 0x00800000 else 2 if magnitude else 3
    return 1 << (2 + kind if negative else 9 - kind)


def median_float(a, b, c):
    values = [to_float(x) for x in (a, b, c)]
    if any(math.isnan(v) for v in values):
        return extreme(min, 3)(a, b, c)
    return Float(to_bits(sorted(values)[1]), any_zero=True)


def power_of_two(value):
    try:
        return 2.0 ** value
    except OverflowError:
        return INFINITY


def log_two(value):
    if value == 0:
        return -INFINITY
    return math.log2(value) if value > 0 else float("nan")


def multiply_add(a, b, c):
    """The console's fused result. HLSL has no fused single-precision operation
    a host must honour, so a host that rounds the product first also passes;
    the receipt names the instructions seen doing that under `alternatives`."""
    product = to_float(to_bits(to_float(a) * to_float(b)))
    return Float(fused(a, b, c), also=(to_bits(product + to_float(c)),))


def separate(a, b, c, legacy=False):
    """A product rounded, then a sum rounded, as the RDNA 1 multiply-adds do. A
    host may fuse the two, so the fused result passes as well."""
    if legacy and (not a & 0x7fffffff or not b & 0x7fffffff):
        return Float(to_bits(0.0 + to_float(c)), any_zero=True)
    product = to_float(to_bits(to_float(a) * to_float(b)))
    return Float(to_bits(product + to_float(c)), also=(fused(a, b, c),))


TRIG_ERROR = 2.0 ** -20
# The manual gives these 1 ULP on the console, and both hosts measured stay within it.
APPROXIMATE = 1
# The translator's own logarithm near 1 and the hosts' elsewhere were within 2 ULP on
# both cards measured. NVIDIA's exp2 was 2 ULP out on one input; RADV's was within 1.
LOGARITHM = 4
EXPONENTIAL = 4
ADDEND = 0x40490fdb

# name: (source count, model returning a word or an expected `Float`)
FLOATS = {
    "v_mul_legacy_f32": (2, legacy_multiply),
    "v_fract_f32": (1, fraction),
    "v_sin_f32": (1, turns(math.sin, 0.0)),
    "v_cos_f32": (1, turns(math.cos, 1.0)),
    "v_med3_f32": (3, median_float),
    "v_cubeid_f32": (3, cube(lambda x, y, z: 5.0 if z < 0 else 4.0, lambda x, y, z: 3.0 if y < 0 else 2.0,
                             lambda x, y, z: 1.0 if x < 0 else 0.0)),
    "v_cubesc_f32": (3, cube(lambda x, y, z: -x if z < 0 else x, lambda x, y, z: x,
                             lambda x, y, z: z if x < 0 else -z)),
    "v_cubetc_f32": (3, cube(lambda x, y, z: -y, lambda x, y, z: -z if y < 0 else z, lambda x, y, z: -y)),
    "v_cubema_f32": (3, cube(lambda x, y, z: 2.0 * z, lambda x, y, z: 2.0 * y, lambda x, y, z: 2.0 * x)),
    "v_rsq_f32": (1, arithmetic(lambda a, b, c: 1 / math.sqrt(a) if a > 0 else
                                (math.copysign(INFINITY, a) if a == 0 else float("nan")), ulps=APPROXIMATE)),
    "v_rcp_iflag_f32": (1, arithmetic(lambda a, b, c: 1 / a if a else math.copysign(INFINITY, a), ulps=1)),
    "v_exp_f32": (1, arithmetic(lambda a, b, c: power_of_two(a), ulps=EXPONENTIAL)),
    "v_log_f32": (1, arithmetic(lambda a, b, c: log_two(a), ulps=LOGARITHM)),
    # The destination is the third source.
    "v_fmac_f32": (3, multiply_add),
    # The addend is a literal in the instruction.
    "v_fmaak_f32": (2, lambda a, b, c: multiply_add(a, b, ADDEND)),
    "v_add_f32": (2, arithmetic(lambda a, b, c: a + b)),
    "v_sub_f32": (2, arithmetic(lambda a, b, c: a - b)),
    "v_subrev_f32": (2, arithmetic(lambda a, b, c: b - a)),
    "v_mul_f32": (2, arithmetic(lambda a, b, c: a * b)),
    "v_min_f32": (2, extreme(min, 2)),
    "v_max_f32": (2, extreme(max, 2)),
    "v_min3_f32": (3, extreme(min, 3)),
    "v_max3_f32": (3, extreme(max, 3)),
    "v_fma_f32": (3, lambda a, b, c: multiply_add(a, b, c)),
    "v_floor_f32": (1, whole(math.floor)),
    "v_ceil_f32": (1, whole(math.ceil)),
    "v_trunc_f32": (1, whole(math.trunc)),
    "v_rndne_f32": (1, whole(round)),
    "v_sqrt_f32": (1, arithmetic(lambda a, b, c: math.sqrt(a) if a >= 0 else float("nan"), ulps=1)),
    "v_rcp_f32": (1, arithmetic(lambda a, b, c: 1 / a if a else math.copysign(INFINITY, a), ulps=1)),
    "v_cvt_i32_f32": (1, to_integer(math.trunc, -2**31, 2**31 - 1)),
    "v_cvt_u32_f32": (1, to_integer(math.trunc, 0, M)),
    "v_cvt_flr_i32_f32": (1, to_integer(math.floor, -2**31, 2**31 - 1)),
    "v_cvt_rpi_i32_f32": (1, to_integer(lambda v: math.floor(v + 0.5), -2**31, 2**31 - 1)),
}
# RDNA 1 instructions the console also runs. The assembler needs its own target for them.
LEGACY = {
    "v_mad_f32": (3, separate),
    "v_mad_legacy_f32": (3, lambda a, b, c: separate(a, b, c, legacy=True)),
    "v_mac_f32": (3, separate),
    "v_mac_legacy_f32": (3, lambda a, b, c: separate(a, b, c, legacy=True)),
    "v_madak_f32": (2, lambda a, b, c: separate(a, b, ADDEND)),
    "v_madmk_f32": (2, lambda a, b, c: separate(a, ADDEND, b)),
}
FLOATS.update(LEGACY)
FLOAT_EDGE = [0x00000000, 0x80000000, 0x3f800000, 0xbf800000, 0x3f000000, 0xbf000000, 0x3fc00000,
              0xc0200000, 0x4b000000, 0x4effffff, 0x4f000000, 0xcf000000, 0x4f800000, 0x7f7fffff,
              0xff7fffff, 0x7f800000, 0xff800000, 0x7fc00000, 0x00800000, 0x3effffff]

RELATIONS = {"eq": operator.eq, "ne": operator.ne, "lt": operator.lt, "gt": operator.gt,
             "le": operator.le, "ge": operator.ge, "lg": operator.ne}


def float_relation(name):
    def model(a, b, c):
        x, y = to_float(a), to_float(b)
        unordered = math.isnan(x) or math.isnan(y)
        if name in ("o", "u"):
            return int(unordered == (name == "u"))
        if name == "lg":
            return int(not unordered and x != y)
        if name == "neq":
            return int(unordered or x != y)
        if name.startswith("n"):
            return int(not RELATIONS[name[1:]](x, y) or unordered)
        return int(not unordered and RELATIONS[name](x, y))
    return model


# Vector comparisons: the lane's bit of VCC, selected into 0 or 1.
COMPARES = {
    **{f"v_cmp_{name}_u32": (2, lambda a, b, c, name=name: int(RELATIONS[name](a, b)))
       for name in ("eq", "ne", "lt", "gt", "le", "ge")},
    **{f"v_cmp_{name}_i32": (2, lambda a, b, c, name=name: int(RELATIONS[name](s32(a), s32(b))))
       for name in ("eq", "ne", "lt", "gt", "le", "ge")},
}
FLOAT_COMPARES = {f"v_cmp_{name}_f32": (2, float_relation(name))
                  for name in ("eq", "lt", "gt", "le", "ge", "lg", "o", "u", "neq", "nlt", "ngt", "nle", "nge")}
FLOAT_COMPARES["v_cmp_class_f32"] = (2, lambda a, b, c: int(bool(number_class(a) & b)))
# ----- Half precision -----
# The exact rounding is `check_half_exact_gpu`'s, which was compared with an RDNA 2 card.
NEAREST, TOWARD_ZERO = 0xc0, 0xcc
HALF_NAN = 0x7e00


def half_float(bits):
    return struct.unpack("<e", struct.pack("<H", bits & 0xffff))[0]


def half_is_nan(bits):
    return bits & 0x7fff > 0x7c00


class Half:
    """An expected f16 in the low 16 bits of a word whose high bits are zero.
    Any NaN matches a NaN, and a zero of either sign where `any_zero`."""

    def __init__(self, bits, ulps=0, any_zero=False, absolute=0.0):
        self.bits, self.ulps, self.any_zero, self.absolute = bits & 0xffff, ulps, any_zero, absolute

    def matches(self, got):
        want = self.bits
        if got >> 16:
            return False
        if half_is_nan(want):
            return half_is_nan(got)
        if got == want or (self.any_zero and not want & 0x7fff and not got & 0x7fff):
            return True
        if half_is_nan(got):
            return False
        if self.absolute and got & 0x7fff < 0x7c00 and want & 0x7fff < 0x7c00 \
                and abs(half_float(got) - half_float(want)) <= self.absolute:
            return True
        return not (got ^ want) & 0x8000 and abs((got & 0x7fff) - (want & 0x7fff)) <= self.ulps

    def __repr__(self):
        return f"{self.bits:#06x}" + (f"~{self.ulps}ulp" if self.ulps else "")


class Halves:
    """Two expected f16 values packed in one word, low first."""

    def __init__(self, low, high):
        self.low, self.high = low, high

    def matches(self, got):
        return self.low.matches(got & 0xffff) and self.high.matches(got >> 16)

    def __repr__(self):
        return f"[{self.low!r}, {self.high!r}]"


def round_half(value, mode=NEAREST):
    """The f16 nearest an exact or a Python float value."""
    if isinstance(value, float):
        if math.isnan(value):
            return Half(HALF_NAN)
        if math.isinf(value):
            return Half(0xfc00 if value < 0 else 0x7c00)
        value = Fraction(value)
    if not value:
        return Half(0, any_zero=True)
    return Half(encode_half(abs(value), int(value < 0), mode))


def half_arithmetic(function, ulps=0, absolute=0.0, count=3):
    """A model over f16 sources. Infinities and NaN go through Python's floats;
    finite sources are exact fractions, so the result is rounded once."""
    def model(a, b, c):
        floats = [half_float(x) for x in (a, b, c)]
        try:
            # Sources the instruction doesn't read hold anything.
            if all(math.isfinite(v) for v in floats[:count]) and not ulps and not absolute:
                result = round_half(function(*(half_value(x & 0xffff) for x in (a, b, c))))
            else:
                result = round_half(float(function(*floats)))
        except (ValueError, ZeroDivisionError, OverflowError):
            result = Half(HALF_NAN)
        result.ulps, result.absolute = ulps, absolute
        return result
    return model


def half_extreme(pick, count):
    def model(a, b, c):
        numbers = [x & 0xffff for x in (a, b, c)[:count] if not half_is_nan(x)]
        return Half(pick(numbers, key=half_float) if numbers else HALF_NAN, any_zero=True)
    return model


def half_median(a, b, c):
    if any(half_is_nan(x) for x in (a, b, c)):
        return half_extreme(min, 3)(a, b, c)
    return Half(sorted((x & 0xffff for x in (a, b, c)), key=half_float)[1], any_zero=True)


def divide(a, b):
    return a / b if b else math.copysign(INFINITY, a) * math.copysign(1.0, b)


def half_turns(function, at_zero):
    def model(a, b, c):
        value = half_float(a)
        if not math.isfinite(value):
            return Half(HALF_NAN)
        part = Fraction(value) - math.floor(value)
        result = round_half(function(2 * math.pi * float(part)) if part else at_zero)
        result.any_zero, result.absolute = True, 2.0 ** -9
        return result
    return model


def halves_of(word):
    return word & 0xffff, word >> 16


def packed(model):
    """An f16 model applied to the low halves and to the high halves."""
    def both(a, b, c):
        low, high = zip(*(halves_of(x) for x in (a, b, c)))
        return Halves(model(*low), model(*high))
    return both


# name: (source count, model of the low 16 bits)
HALF = {
    "v_add_f16": (2, half_arithmetic(lambda a, b, c: a + b, count=2)),
    "v_sub_f16": (2, half_arithmetic(lambda a, b, c: a - b, count=2)),
    "v_subrev_f16": (2, half_arithmetic(lambda a, b, c: b - a, count=2)),
    "v_mul_f16": (2, half_arithmetic(lambda a, b, c: a * b, count=2)),
    "v_fma_f16": (3, half_arithmetic(lambda a, b, c: a * b + c)),
    # The destination is the third source.
    "v_fmac_f16": (3, half_arithmetic(lambda a, b, c: a * b + c)),
    "v_min_f16": (2, half_extreme(min, 2)),
    "v_max_f16": (2, half_extreme(max, 2)),
    "v_med3_f16": (3, half_median),
    "v_trunc_f16": (1, half_arithmetic(lambda a, b, c: Fraction(math.trunc(a)) if isinstance(a, Fraction) else a,
                                       count=1)),
    "v_rcp_f16": (1, half_arithmetic(lambda a, b, c: divide(1.0, a), ulps=1)),
    "v_sqrt_f16": (1, half_arithmetic(lambda a, b, c: math.sqrt(a) if a >= 0 else float("nan"), ulps=1)),
    "v_rsq_f16": (1, half_arithmetic(lambda a, b, c: divide(1.0, math.sqrt(a)) if a >= 0 else float("nan"), ulps=1)),
    "v_exp_f16": (1, half_arithmetic(lambda a, b, c: power_of_two(a), ulps=1)),
    "v_sin_f16": (1, half_turns(math.sin, 0.0)),
    "v_cos_f16": (1, half_turns(math.cos, 1.0)),
}
PACKED = {
    "v_pk_add_f16": (2, packed(HALF["v_add_f16"][1])),
    "v_pk_mul_f16": (2, packed(HALF["v_mul_f16"][1])),
    "v_pk_fma_f16": (3, packed(HALF["v_fma_f16"][1])),
    "v_pk_min_f16": (2, packed(HALF["v_min_f16"][1])),
    "v_pk_max_f16": (2, packed(HALF["v_max_f16"][1])),
}


def half_relation(name):
    """`float_relation` over the f16 in each source's low half."""
    relation = float_relation(name)
    return lambda a, b, c: relation(to_bits(half_float(a)), to_bits(half_float(b)), 0)


HALF_NAMES = ("eq", "lt", "gt", "le", "ge", "lg", "o", "u", "neq", "nlt", "ngt", "nle", "nge")
HALF_COMPARES = {
    **{f"v_cmp_{name}_f16": (2, half_relation(name)) for name in HALF_NAMES},
    **{f"v_cmp_{name}_u16": (2, lambda a, b, c, name=name: int(RELATIONS[name](a & 0xffff, b & 0xffff)))
       for name in ("eq", "ne", "lt", "gt", "le", "ge")},
}
HALF_EXEC_COMPARES = {
    **{f"v_cmpx_{name}_f16": (2, half_relation(name)) for name in HALF_NAMES},
    **{f"v_cmpx_{name}_u16": (2, lambda a, b, c, name=name: int(RELATIONS[name](a & 0xffff, b & 0xffff)))
       for name in ("eq", "ne", "lt", "gt", "le", "ge")},
}


def rounded_toward_zero(bits):
    """An f32 as an f16, toward zero: `v_cvt_pkrtz_f16_f32` ignores the rounding mode."""
    if is_nan(bits):
        return Half(HALF_NAN)
    if bits & 0x7fffffff == 0x7f800000:
        return Half(0x7c00 | bits >> 16 & 0x8000)
    value = f32_value(bits)
    return Half(encode_half(abs(value), bits >> 31, TOWARD_ZERO)) if value else Half(bits >> 16 & 0x8000)


def mixed(selection):
    """The sources of a MIX instruction as f32 bit patterns: `f` takes the word
    as an f32, `l` and `h` its low or high half as an f16, which an f32 holds exactly.
    `1` is the inline constant 1.0 read as an f16 source."""
    def sources(a, b, c):
        return [word if pick == "f" else to_bits(1.0) if pick == "1"
                else to_bits(half_float(word >> (16 if pick == "h" else 0)))
                for pick, word in zip(selection, (a, b, c))]
    return sources


def clamped(value):
    """CLAMP: the result held to [0, 1], with NaN as 0. The sign of a zero is left open."""
    value = to_float(value)
    return 0.0 if math.isnan(value) else min(max(value, 0.0), 1.0)


def mix_f32(selection):
    sources = mixed(selection.rstrip("c"))
    if not selection.endswith("c"):
        return lambda a, b, c: multiply_add(*sources(a, b, c))

    def model(a, b, c):
        expected = multiply_add(*sources(a, b, c))
        bits = [to_bits(clamped(x)) for x in (expected.bits, *expected.also)]
        return Float(bits[0], any_zero=True, also=tuple(bits[1:]))
    return model


def mix_to_half(selection):
    clamp = selection.endswith("c")

    def model(a, b, c):
        values = [to_float(x) for x in mixed(selection.rstrip("c"))(a, b, c)]
        if not all(math.isfinite(v) for v in values):
            value = values[0] * values[1] + values[2]
        else:
            value = Fraction(values[0]) * Fraction(values[1]) + Fraction(values[2])
        if clamp:
            value = 0.0 if value != value else Fraction(min(max(value, 0), 1))
        return round_half(value)
    return model


# name@sources: the instruction with each source taken as `mixed` describes,
# and a trailing `c` for CLAMP.
MIX = {
    **{f"v_fma_mix_f32@{sel}": (3, mix_f32(sel)) for sel in ("fff", "lll", "hlf", "fhl", "fffc", "hlfc", "f1f")},
    **{f"v_fma_mixlo_f16@{sel}": (3, mix_to_half(sel)) for sel in ("lll", "hhh", "fff", "lhf", "lhfc")},
    **{f"v_fma_mixhi_f16@{sel}": (3, mix_to_half(sel)) for sel in ("lll", "hhh", "fff", "hhhc")},
}
def byte_pack(a, b, c):
    """`v_cvt_pk_u8_f32`: the float as a byte, in the byte of the third source the
    second names. The manual doesn't give the conversion; this is the nearest
    integer, ties to even, held to 0..255, with NaN as 0."""
    value = to_float(a)
    byte = 0 if math.isnan(value) else max(0, min(255, round(max(-1.0, min(256.0, value)))))
    shift = 8 * (b & 3)
    return (c & ~(0xff << shift) & M) | byte << shift


CONVERSIONS = {
    "v_cvt_pk_u8_f32": (3, byte_pack),
    "v_cvt_pkrtz_f16_f32": (2, lambda a, b, c: Halves(rounded_toward_zero(a), rounded_toward_zero(b))),
}
FLOATS.update(CONVERSIONS)
FLOATS.update(MIX)
HALF_TABLES = (HALF, PACKED, HALF_COMPARES, HALF_EXEC_COMPARES)


def is_half(name):
    return any(name in table for table in HALF_TABLES)


# Comparisons into EXEC: 1 where the lane stays active.
EXEC_COMPARES = {
    **{f"v_cmpx_{name}_u32": (2, lambda a, b, c, name=name: int(RELATIONS[name](a, b)))
       for name in ("eq", "ne", "lt", "gt", "le", "ge")},
    **{f"v_cmpx_{name}_i32": (2, lambda a, b, c, name=name: int(RELATIONS[name](s32(a), s32(b))))
       for name in ("eq", "ne", "lt", "gt", "le", "ge")},
}
FLOAT_EXEC_COMPARES = {f"v_cmpx_{name}_f32": (2, float_relation(name))
                       for name in ("eq", "lt", "gt", "le", "ge", "lg", "o", "u", "neq", "nlt", "ngt", "nle", "nge")}
SCALAR_COMPARES = {
    **{f"s_cmp_{name}_u32": (2, lambda a, b, name=name: int(RELATIONS[name](a, b)))
       for name in ("eq", "lg", "lt", "gt", "le", "ge")},
    **{f"s_cmp_{name}_i32": (2, lambda a, b, name=name: int(RELATIONS[name](s32(a), s32(b))))
       for name in ("eq", "lg", "lt", "gt", "le", "ge")},
}


# Instructions that change their destination, which holds the second input: (model).
IN_PLACE = {
    "s_bitset0_b32": lambda a, b: (b & ~(1 << (a & 31)) & M, None),
    "s_bitset1_b32": lambda a, b: (b | 1 << (a & 31), None),
}
SIGNED_CONSTANT, UNSIGNED_CONSTANT = -2, 0xffff
# Instructions with a 16-bit constant: (the constant, model of the first input).
CONSTANT = {
    "s_addk_i32": (-1234, lambda a: ((a - 1234) & M, int(signed_overflow(s32(a) - 1234)))),
    "s_mulk_i32": (-3, lambda a: ((a * -3) & M, None)),
}
CONSTANT_COMPARES = {
    **{f"s_cmpk_{name}_i32": (SIGNED_CONSTANT, lambda a, name=name: int(RELATIONS[name](s32(a), SIGNED_CONSTANT)))
       for name in ("eq", "lg", "gt", "ge", "lt", "le")},
    **{f"s_cmpk_{name}_u32": (UNSIGNED_CONSTANT, lambda a, name=name: int(RELATIONS[name](a, UNSIGNED_CONSTANT)))
       for name in ("eq", "lg", "gt", "ge", "lt", "le")},
}


def is_float(name):
    return name in FLOATS or name in FLOAT_COMPARES or name in FLOAT_EXEC_COMPARES


def count(name):
    """Result words one instruction produces: scalar ones also report SCC."""
    if name in GROUP_MEMORY:
        return len(GROUP_MEMORY[name][1](1, 1))
    if name in SEQUENCES:
        return len(SEQUENCES[name][1](1, 1, 1))
    if name in PAIRS:
        result = PAIRS[name](1, 1, 1)
        return 1 if isinstance(result, int) else 2 if result[1] is None else 3
    if name in IN_PLACE:
        return 1
    if name in CONSTANT:
        return 1 if CONSTANT[name][1](1)[1] is None else 2
    if name not in SCALAR:
        return 1
    return 1 if SCALAR[name][1](*[1] * SCALAR[name][0])[1] is None else 2


def lines_for(name, destination):
    """Assembly leaving the instruction's results from `v{destination}` on."""
    if name in ("v_fmac_f32", "v_mac_f32", "v_mac_legacy_f32"):
        return [f"v_mov_b32_e32 v{destination}, v6", f"{name} v{destination}, v4, v5"]
    if name in ("v_fmaak_f32", "v_madak_f32"):
        return [f"{name} v{destination}, v4, v5, 0x{ADDEND:x}"]
    if name == "v_madmk_f32":
        return [f"{name} v{destination}, v4, 0x{ADDEND:x}, v5"]
    if name in PAIRS:
        compare, shift = name.startswith("s_cmp_"), "sh" in name
        # X is s[20:21]. Y is s[22:23]; a comparison's shares X's high word so the two can be equal.
        program = ["v_readfirstlane_b32 s20, v4", "v_readfirstlane_b32 s21, v5",
                   f"v_readfirstlane_b32 s22, v{5 if compare else 6}", f"v_readfirstlane_b32 s23, v{5 if compare else 4}",
                   # SCC from the inputs, so a select takes each side and a kept SCC is seen.
                   "s_cmp_lt_u32 s20, s22"]
        if compare:
            return program + [f"{name} s[20:21], s[22:23]", "s_cselect_b32 s24, 1, 0",
                              f"v_mov_b32_e32 v{destination}, s24"]
        program += [f"{name} s[24:25], s[20:21], {'s22' if shift else 's[22:23]'}"]
        if count(name) == 3:
            program += ["s_cselect_b32 s26, 1, 0", f"v_mov_b32_e32 v{destination + 2}, s26"]
        return program + [f"v_mov_b32_e32 v{destination}, s24", f"v_mov_b32_e32 v{destination + 1}, s25"]
    if name in GROUP_MEMORY or name in SEQUENCES:
        # Group memory gets a word-aligned address within the first 64 bytes.
        lines, start = (GROUP_MEMORY[name][0], ["v_and_b32_e32 v20, 0x3c, v4"]) if name in GROUP_MEMORY \
            else (SEQUENCES[name][0], [])
        return start + [
            line.format(f"v{destination}", f"v{destination + 1}") if "{0n}" not in line
            else line.replace("{0n}", str(destination)).replace("{1n}", str(destination + 1))
            for line in lines]
    if name in MIX:
        instruction, selection = name.split("@")
        clamp = " clamp" if selection.endswith("c") else ""
        selection = selection.rstrip("c")
        low = ",".join(str(int(pick == "h")) for pick in selection)
        high = ",".join(str(int(pick != "f")) for pick in selection)
        sources = ", ".join("1.0" if pick == "1" else f"v{4 + j}" for j, pick in enumerate(selection))
        # Only one half of the destination is written, so the other is shifted or masked away.
        keep = {"v_fma_mixlo_f16": [f"v_and_b32_e32 v{destination}, 0xffff, v{destination}"],
                "v_fma_mixhi_f16": [f"v_lshrrev_b32_e32 v{destination}, 16, v{destination}"]}.get(instruction, [])
        return [f"v_mov_b32_e32 v{destination}, 0",
                f"{instruction} v{destination}, {sources} op_sel:[{low}] op_sel_hi:[{high}]{clamp}"] + keep
    if name in HALF:
        sources = ", ".join(f"v{4 + j}" for j in range(HALF[name][0]))
        if name == "v_fmac_f16":
            return [f"v_mov_b32_e32 v{destination}, v6", f"{name} v{destination}, v4, v5",
                    f"v_and_b32_e32 v{destination}, 0xffff, v{destination}"]
        # The manual defines the low 16 bits of the result only.
        return [f"{name} v{destination}, {sources}", f"v_and_b32_e32 v{destination}, 0xffff, v{destination}"]
    if name in PACKED:
        sources = ", ".join(f"v{4 + j}" for j in range(PACKED[name][0]))
        return [f"{name} v{destination}, {sources}"]
    if name in HALF_COMPARES:
        return [f"{name} vcc_lo, v4, v5", f"v_cndmask_b32_e64 v{destination}, 0, 1, vcc_lo"]
    if name in EXEC_COMPARES or name in FLOAT_EXEC_COMPARES or name in HALF_EXEC_COMPARES:
        # The write of 1 runs only where the comparison leaves the lane active.
        return [f"v_mov_b32_e32 v{destination}, 0", "s_mov_b32 s26, exec_lo", f"{name}_e32 v4, v5",
                f"v_mov_b32_e32 v{destination}, 1", "s_mov_b32 exec_lo, s26"]
    if name in OPERATIONS or name in FLOATS:
        sources = ", ".join(f"v{4 + j}" for j in range({**OPERATIONS, **FLOATS}[name][0]))
        return [f"{name} v{destination}, {sources}"]
    if name in COMPARES or name in FLOAT_COMPARES:
        return [f"{name} vcc_lo, v4, v5", f"v_cndmask_b32_e64 v{destination}, 0, 1, vcc_lo"]
    if name in CONSTANT_COMPARES:
        return ["v_readfirstlane_b32 s20, v4", f"{name} s20, {CONSTANT_COMPARES[name][0] & 0xffff:#x}",
                "s_cselect_b32 s24, 1, 0", f"v_mov_b32_e32 v{destination}, s24"]
    if name in CONSTANT or name in IN_PLACE:
        # The destination starts as the first input, or the second where the first is an operand.
        operand = f"{CONSTANT[name][0] & 0xffff:#x}" if name in CONSTANT else "s20"
        program = ["v_readfirstlane_b32 s20, v4", f"v_readfirstlane_b32 s24, v{4 if name in CONSTANT else 5}",
                   "s_cmp_eq_u32 s20, s20", f"{name} s24, {operand}"]
        if count(name) == 2:
            program += ["s_cselect_b32 s25, 1, 0", f"v_mov_b32_e32 v{destination + 1}, s25"]
        return program + [f"v_mov_b32_e32 v{destination}, s24"]
    if name in SCALAR_COMPARES:
        return ["v_readfirstlane_b32 s20, v4", "v_readfirstlane_b32 s21, v5", f"{name} s20, s21",
                "s_cselect_b32 s24, 1, 0", f"v_mov_b32_e32 v{destination}, s24"]
    arity = SCALAR[name][0]
    sources = ", ".join(f"s{20 + j}" for j in range(arity))
    # Each input is uniform: a dispatch group holds one lane.
    program = [f"v_readfirstlane_b32 s{20 + j}, v{4 + j}" for j in range(arity)]
    # Give SCC a known value, so an instruction that should keep it is caught.
    program += ["s_cmp_eq_u32 s20, s20", f"{name} s24, {sources}"]
    if count(name) == 2:
        program += ["s_cselect_b32 s25, 1, 0", f"v_mov_b32_e32 v{destination + 1}, s25"]
    return program + [f"v_mov_b32_e32 v{destination}, s24"]


def results(name, values):
    for table in (OPERATIONS, FLOATS, COMPARES, FLOAT_COMPARES, EXEC_COMPARES, FLOAT_EXEC_COMPARES, *HALF_TABLES):
        if name in table:
            return [table[name][1](*values)]
    if name in SCALAR_COMPARES:
        return [SCALAR_COMPARES[name][1](*values[:2])]
    if name in PAIRS:
        a, b, c = values
        compare = name.startswith("s_cmp_")
        low, high = (b, b) if compare else (c, a)
        result = PAIRS[name](a | b << 32, low | high << 32, int(a < low))
        if compare:
            return [result]
        value, flag = result
        return [value & M, value >> 32] + ([] if flag is None else [flag])
    if name in GROUP_MEMORY:
        return GROUP_MEMORY[name][1](values[1], values[2])
    if name in SEQUENCES:
        return SEQUENCES[name][1](*values)
    if name in CONSTANT_COMPARES:
        return [CONSTANT_COMPARES[name][1](values[0])]
    if name in CONSTANT or name in IN_PLACE:
        value, flag = CONSTANT[name][1](values[0]) if name in CONSTANT else IN_PLACE[name](*values[:2])
        return [value] if flag is None else [value, flag]
    arity, model = SCALAR[name]
    value, flag = model(*values[:arity])
    return [value] if flag is None else [value, flag]


def program(chunk):
    out, destination = inputs(), 7
    for name in chunk:
        out += lines_for(name, destination)
        destination += count(name)
    return out + ["s_endpgm"]


def kind(name):
    """Float and integer instructions need different inputs, and RDNA 1 ones
    another assembler target, so a shader holds one kind."""
    return is_float(name), name in LEGACY, is_half(name)


def target(names):
    return ("-mcpu=gfx1013",) if any(name in LEGACY for name in names) else ()


def chunks_of(names):
    chunk, used = [], 0
    for name in sorted(names, key=kind):
        if used + count(name) > OUTPUTS or (chunk and kind(name) != kind(chunk[0])):
            yield chunk
            chunk, used = [], 0
        chunk.append(name)
        used += count(name)
    if chunk:
        yield chunk


def probe(names, emitter, llvm_mc):
    """Split `names` into those translated and those refused or unassembled."""
    supported, refused = [], {}
    for name in names:
        try:
            words = assemble(program([name]), llvm_mc, *target([name]))
        except (ValueError, subprocess.CalledProcessError) as error:
            refused[name] = f"not assembled: {str(error)[:120]}"
            continue
        got = translate(emitter, words)
        if got.returncode == 65:
            refused[name] = got.stderr.strip()
        elif got.returncode:
            raise RuntimeError(f"emitter failed on {name}: {got.returncode} {got.stderr[-300:]}")
        else:
            supported.append(name)
    return supported, refused


def inputs():
    return [f"v_mov_b32_e32 v{4 + j}, 0x{value:x}" for j, value in enumerate(INIT)]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    for n in ("emitter", "llvm-mc", "dxc", "dxv", "provider", "proton", "prefix", "compiler-dir", "work", "output"):
        ap.add_argument("--" + n, type=Path, required=True)
    ap.add_argument("--adapter", required=True)
    ap.add_argument("--rounds", type=int, default=8, help=f"dispatches per shader, {GROUPS} inputs each")
    ap.add_argument("--only", action="append", help="check only this instruction (repeatable)")
    a = ap.parse_args()
    if a.output.exists():
        ap.error("choose a fresh receipt")
    a.work.mkdir(parents=True, exist_ok=False)
    rng = random.Random(0x414c5531)
    every = [*OPERATIONS, *SCALAR, *GROUP_MEMORY, *SEQUENCES, *PAIRS, *IN_PLACE, *CONSTANT, *CONSTANT_COMPARES, *COMPARES, *SCALAR_COMPARES,
             *EXEC_COMPARES, *HALF, *PACKED, *HALF_COMPARES, *HALF_EXEC_COMPARES, *FLOATS, *FLOAT_COMPARES,
             *FLOAT_EXEC_COMPARES]
    if a.only:
        unknown = sorted(set(a.only) - set(every))
        if unknown:
            ap.error(f"not in this check: {', '.join(unknown)}")
        every = [name for name in every if name in a.only]
    supported, refused = probe(every, a.emitter, a.llvm_mc)
    expected, chunks = {}, {}
    with TemporaryDirectory(dir=a.work) as td:
        trial = Path(td)
        guest = ROOT / "tools/reference/independent-shaders/compute.cpp"
        (trial / "guest.cpp").write_text(guest.read_text())
        (trial / "context.h").write_text((guest.parent / "context.h").read_text().replace(
            '"../compute_buffers.h"', '"' + str(ROOT / "tools/reference/compute_buffers.h") + '"'))
        for number, chunk in enumerate(chunks_of(supported)):
            got = translate(a.emitter, assemble(program(chunk), a.llvm_mc, *target(chunk)))
            got.check_returncode()
            outputs = sum(count(name) for name in chunk)
            shader = trial / f"alu{number:02}.hlsl"
            shader.write_text(instrument(got.stdout, outputs))
            blob = shader.with_suffix(".cso")
            subprocess.run([a.dxc, "-T", "cs_6_0", "-E", "main", "-O3", "-Fo", blob, shader],
                           check=True, capture_output=True, timeout=30)
            subprocess.run([a.dxv, blob], check=True, capture_output=True, timeout=15)
            for turn in range(a.rounds):
                label = f"alu{number:02}r{turn:02}"
                (trial / (label + ".cso")).write_bytes(blob.read_bytes())
                seed = [rng.getrandbits(32) for _ in range(384)]
                want = seed.copy()
                for lane in range(GROUPS):
                    index = turn * GROUPS + lane
                    edges = FLOAT_EDGE if is_float(chunk[0]) else EDGE
                    # Edge values first, rotated so each source sees each of them.
                    if index < len(edges):
                        values = [edges[(index + 7 * j) % len(edges)] for j in range(3)]
                    elif is_half(chunk[0]):
                        # Edge halves in both halves of the word, or any bits.
                        values = [rng.choice(EDGE_F16) * 0x10001 if rng.random() < 0.3 else rng.getrandbits(32)
                                  for _ in range(3)]
                        if rng.random() < 0.15:
                            values[1] = values[0]
                    elif is_float(chunk[0]):
                        # Ordinary magnitudes of both signs, so results stay finite
                        # and rounding matters. Some pairs are equal.
                        values = [to_bits(rng.uniform(-1, 1) * 2.0 ** rng.randint(-30, 30)) for _ in range(3)]
                        if rng.random() < 0.2:
                            values[1] = values[0]
                        # Close to 1, where a logarithm's result is small and its error shows.
                        if rng.random() < 0.25:
                            values[0] = to_bits(1.0 + rng.uniform(-1, 1) * 2.0 ** rng.randint(-20, -1))
                    else:
                        values = [rng.getrandbits(32) for _ in range(3)]
                        if rng.random() < 0.1:
                            values[1] = values[0]
                    offset = 96 + STRIDE * lane
                    seed[offset:offset + 3] = values
                    want[offset:offset + 3] = values
                    row = [word for name in chunk for word in results(name, values)]
                    want[offset + 3:offset + 3 + outputs] = row
                (trial / (label + ".seed")).write_bytes(struct.pack("<384I", *seed))
                expected[label] = want
                chunks[label] = chunk
        (trial / "cases.txt").write_text("".join(f"{n} 4 structured {GROUPS}\n" for n in expected))
        _, status, fields = run_gpu_guest(trial / "guest.cpp", "basic-alu.exe", trial, a.work, a.proton,
                                          a.prefix, a.provider, a.compiler_dir)
        if status or fields.get("adapter") != a.adapter:
            raise RuntimeError(f"guest failed: {status} {fields}")
        wrong, worst, worst_input, alternatives = {}, {}, {}, set()
        for label, chunk in chunks.items():
            observed = struct.unpack("<384I", (trial / (label + ".bin")).read_bytes())
            wanted = expected[label]
            for index, (got_word, want_word) in enumerate(zip(observed, wanted)):
                lane, column = divmod(index - 96, STRIDE)
                owners = [name for name in chunk for _ in range(count(name))]
                name = owners[column - 3] if 0 <= lane < GROUPS and 3 <= column < 3 + len(owners) else "guard"
                apart = distance(got_word, want_word)
                if apart is not None and apart > worst.get(name, -1):
                    worst[name] = apart
                    worst_input[name] = f"{wanted[96 + STRIDE * lane]:#010x}"
                if isinstance(want_word, Float) and got_word != want_word.bits and got_word in want_word.also:
                    alternatives.add(name)
                if matches(got_word, want_word):
                    continue
                wrong.setdefault(name, {"shader": label, "inputs": list(wanted[96 + STRIDE * lane:99 + STRIDE * lane]),
                                        "got": got_word, "wanted": repr(want_word)})
    receipt = {"schema": 1, "adapter": a.adapter, "tools_sha256": {n: digest(getattr(a, n))
                                                                   for n in ("emitter", "dxc", "provider")},
               "checked": sorted({name.split("@")[0] for name in supported}), "forms": len(supported), "refused": refused, "wrong": wrong, "inputs": a.rounds * GROUPS,
               # The largest error seen from each approximate instruction, in units in the last place.
               "worst_ulps": dict(sorted(worst.items())), "worst_ulps_input": dict(sorted(worst_input.items())),
               # Instructions that gave a documented alternative to the console's result.
               "alternatives": sorted(alternatives)}
    a.output.write_text(json.dumps(receipt, indent=2) + "\n")
    for name, reason in refused.items():
        print(f"refused {name}: {reason[:150]}")
    print("gave an accepted alternative result: " + (", ".join(sorted(alternatives)) or "none"))
    print("largest errors in units in the last place: " + ", ".join(f"{n} {v}" for n, v in sorted(worst.items())))
    for name, detail in wrong.items():
        print(f"WRONG {name}: {detail}")
    print(f"{'FAIL' if wrong else 'PASS'} {len(supported)} instructions on {a.rounds * GROUPS} inputs, "
          f"{len(wrong)} wrong, {len(refused)} refused")
    return int(bool(wrong))


if __name__ == "__main__":
    raise SystemExit(main())
