"""The synthetic Xbox compute shaders the Lanes self-test loads.

All are small hand-written console programs, not taken from any game. One is a
`v_mov_b32` and `s_endpgm` with an empty root signature embedded. Another sums
the lane numbers of each 64-lane console wave and writes a word per thread. The
two `compute-root` programs carry no root and store one word through table 1.
The `large-dispatch` program stores its group ID.
"""
import hashlib
from pathlib import Path
import struct
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from examples.lanes.raster_fixture import container as stage_container, image  # noqa: E402
from tools.reference.scarlett_worker_fixture import container, pack, uav_shader  # noqa: E402

CODE = (0x7E0002FF, 0x12345678, 0xBF810000)
# Root signature 1.0 with no parameters, samplers or flags.
EMPTY_ROOT = pack([1, 0, 24, 0, 24, 0])


def shader():
    base = container(list(CODE))
    count = struct.unpack_from("<I", base, 28)[0]
    offsets = struct.unpack_from(f"<{count}I", base, 32)
    chunks = [base[start:start + 8 + struct.unpack_from("<I", base, start + 4)[0]] for start in offsets]
    chunks.append(b"RTS0" + pack([len(EMPTY_ROOT)]) + EMPTY_ROOT)
    start = 32 + 4 * len(chunks)
    table = []
    for chunk in chunks:
        table.append(start)
        start += len(chunk)
    return b"DXBC" + bytes(16) + pack([1, start, len(chunks), *table]) + b"".join(chunks)


# Two console waves in one group. v0 is the thread number.
WAVE_THREADS = 128
WAVE_CODE = (
    0xF4080402, 0xFA000000,  # s_load_dwordx4 s[16:19], s[4:5]: the u0 descriptor
    0xBF8CC07F,  # s_waitcnt lgkmcnt(0)
    # The 64-lane sum of v0, left in s7: an EXEC-saving prologue, DPP and
    # cross-row exchanges, then reads of lanes 31 and 63.
    0xBE8025C1, 0xD5010002, 0x00020080, 0x4A0404E9, 0xDE54C102, 0x4A0404E9, 0x93601202, 0x4A0404E9, 0x00092402,
    0x4A0404FA, 0xFF094002, 0xD7780003, 0x02010102, 0x4A040503, 0xD7600006, 0x00014102, 0x4A040406, 0xD7600007,
    0x00010102, 0xBEFE0400,
    0x4A080007,  # v_add_nc_u32 v4, s7, v0
    0x340A0082,  # v_lshlrev_b32 v5, 2, v0
    0xE0701000, 0x80040405,  # buffer_store_dword v4, v5, s[16:19], offen
    0xBF810000,  # s_endpgm
)


def wave_shader():
    return uav_shader(list(WAVE_CODE), width=WAVE_THREADS)


def wave_expected():
    """The word each thread writes: its number plus the sum over its wave."""
    return [n + sum(range(n & ~63, (n & ~63) + 64)) for n in range(WAVE_THREADS)]


# A gfx1030 Wave32 program with a vector register that is also used by
# WRITELANE. Lane 12 is the first with EXEC set, so its v5 selects lane 9.
# WRITELANE and READLANE must both reach lane 9 with its EXEC bit clear.
LANE_WRITE_WORD = 0x7F15CAFE
LANE_CODE = (
    0xF4080402, 0xFA000000,     # s_load_dwordx4 s[16:19], s[4:5], 0
    0x3A120093,                 # v_xor_b32 v9, 19, v0
    0xD5260005, 0x02010700,     # v_sub_nc_u32 v5, v0, 3
    0xD4C6000B, 0x02011900,     # v_cmp_ge_u32 s11, v0, 12
    0xBE8A3C0B,                 # s_and_saveexec_b32 s10, s11
    0xBE8803FF, LANE_WRITE_WORD, # s_mov_b32 s8, LANE_WRITE_WORD
    0xD7610009, 0x02011208,     # v_writelane_b32 v9, s8, 9
    0x7E0E0505,                 # v_readfirstlane_b32 s7, v5
    0xD7600007, 0x02000F09,     # v_readlane_b32 s7, v9, s7
    0xBEFE030A,                 # s_mov_b32 exec_lo, s10
    0x3A0C1207,                 # v_xor_b32 v6, s7, v9
    0x34100082,                 # v_lshlrev_b32 v8, 2, v0
    0xBF8CC07F,                 # s_waitcnt lgkmcnt(0)
    0xE0701000, 0x80040608,     # buffer_store_dword v6, v8, s[16:19], 0 offen
    0xBF810000,                 # s_endpgm
)


def lane_shader():
    return uav_shader(list(LANE_CODE), width=32)


def lane_expected():
    """Every lane combines the broadcast with its own retained VGPR value."""
    return [0 if lane == 9 else LANE_WRITE_WORD ^ (lane ^ 19) for lane in range(32)]


# The `compute-root` programs, checked with `llvm-mc -mcpu=gfx1030 -show-encoding`.
# Each stores ROOT_EXPECTED through the descriptor at a byte offset of table 1,
# whose pointer arrives in s5. Their FSSM record and PSV binding name it a raw
# UAV u3, space2, which the scenario's root does not declare: a translation
# without that root cannot be bound. The root declares u0-u3, space1 in table 1,
# so slot 2 (0x40) is u2, space1 there, and slot 5 (0xa0) is past the table.
ROOT_EXPECTED = 0x7AB1E002
ROOT_THREADS = 64
ROOT_RESOURCES = ([7, 2, 3, 3],)  # PSV binding [type, space, lower, upper]: raw UAV


def root_code(offset):
    return (
        0xBE800305,                       # s_mov_b32 s0, s5
        0xBE810380,                       # s_mov_b32 s1, 0
        0xF4080200, 0xFA000000 | offset,  # s_load_dwordx4 s[8:11], s[0:1], offset
        0xBF8CC07F,                       # s_waitcnt lgkmcnt(0)
        0x7E0002FF, ROOT_EXPECTED,        # v_mov_b32 v0, ROOT_EXPECTED
        0xE0700000, 0x80020000,           # buffer_store_dword v0, off, s[8:11], 0
        0xBF810000,                       # s_endpgm
    )


def root_shader(offset):
    """A root-less compute program storing through table 1 at `offset` bytes."""
    info = bytearray(48)
    info[24] = 5
    struct.pack_into("<3I", info, 36, ROOT_THREADS, 1, 1)
    psv = pack([len(info)]) + info + pack([len(ROOT_RESOURCES), 16, *ROOT_RESOURCES[0]])
    # FSSM row [class, dword offset, register, shape, location]: a raw UAV,
    # register 3, in table 1 (s5).
    used = ([0x303, offset // 4, 3, 1, 0x1_0504],)
    return stage_container(0x50060, image(7, root_code(offset), (), used), [(b"PSV0", psv)])


def recovered_root_shader():
    """Two stores, one in an original table and one in an uncovered second-table slot.

    Only the second descriptor has a corroborating user-data record: whole
    reflection fails, so the provider must append the missing range rather
    than replace the original layout.
    """
    info = bytearray(48)
    info[24] = 5
    struct.pack_into("<3I", info, 36, ROOT_THREADS, 1, 1)
    resources = ([7, 0, 0, 0], [7, 2, 3, 3])
    psv = pack([len(info)]) + info + pack([2, 16, *resources[0], *resources[1]])
    first = list(root_code(0)[:-1])
    first[0] = 0xBE800304  # s4, table 0
    second = list(root_code(0x40))
    second[0] = 0xBE800305  # s5, table 1
    user = ([3, 16, 1, 3, 0x105],)
    used = ([0x303, 0, 0, 1, 0x404], [0x303, 16, 3, 1, 0x10504])
    return stage_container(0x50060, image(7, first + second, user, used), [(b"PSV0", psv)])


def root_srv_shader():
    """A store through table 1 by a program whose records also name a root SRV.

    The scenario's root is three tables and a root SRV. The record of that
    root SRV names parameter 3, whose address arrives in s7-s8, and PSV names
    it a raw buffer, t5, space7; the program does not read through it. The raw
    UAV at slot 2 (0x40) of table 1 is u4, space2, where the root declares an
    SRV. Both have user-data records.
    """
    info = bytearray(48)
    info[24] = 5
    struct.pack_into("<3I", info, 36, ROOT_THREADS, 1, 1)
    resources = ([4, 7, 5, 5], [7, 2, 4, 4])
    psv = pack([len(info)]) + info + pack([2, 16, *resources[0], *resources[1]])
    user = ([1, 0, 2, 5, 0x307], [3, 16, 1, 4, 0x105])
    used = ([0x100, 0, 5, 1, 0x30704], [0x303, 16, 4, 1, 0x10504])
    return stage_container(0x50060, image(7, root_code(0x40), user, used), [(b"PSV0", psv)])


BINDLESS_INDEX = 80
BINDLESS_EXPECTED = 0x7AB1E002


def bindless_compute_shader(table=False):
    """Direct-indexed raw UAV store at a guest index outside the paged limit.

    With `table` the root also has a descriptor table of four UAVs, which the
    program does not read: it still picks its UAV by heap index.
    """
    code = [0xbe8403ff, BINDLESS_INDEX, 0x8f048504,
            0xf4080205, 0x08000000, 0xbf8cc07f,
            0x7e0002ff, BINDLESS_EXPECTED, 0xe0700000, 0x80020000, 0xbf810000]
    info = bytearray(48)
    info[24] = 5
    struct.pack_into("<3I", info, 36, 64, 1, 1)
    root = pack([2, 0, 24, 0, 24, 0x400])
    if table:
        # One table parameter at 24, its header at 36 and one 1.1 range at 44.
        root = pack([2, 1, 24, 0, 68, 0x400, 0, 0, 36, 1, 44, 1, 4, 0, 0, 0, 0])
    return stage_container(0x50066, image(7, code, [(5, 0, 1, 0, 10)]), [
        (b"SFI0", struct.pack("<Q", 1 << 25)),
        (b"PSV0", pack([48]) + info + pack([0])),
        (b"RTS0", root),
    ])


# The `bindless-sampler` program, checked with `llvm-mc -mcpu=gfx1030 -show-encoding`.
# It samples the heap texture at one guest index with a linear clamp sampler
# built from literal words, which no root declares, and stores the value through
# the heap UAV at another. Both indices are past the paged limit of the test.
BINDLESS_TEXTURE, BINDLESS_OUTPUT = 90, 85


def bindless_sampler_shader():
    """Direct-indexed sample with a literal sampler, stored through a direct-indexed UAV."""
    code = [
        0xB0100000 | BINDLESS_TEXTURE,  # s_movk_i32 s16, the texture's heap index
        0x8F008510,                     # s_lshl_b32 s0, s16, 5
        0xF40C0101, 0x00000000,         # s_load_dwordx8 s[4:11], s[2:3], s0
        0xB0110000 | BINDLESS_OUTPUT,   # s_movk_i32 s17, the UAV's heap index
        0x8F018511,                     # s_lshl_b32 s1, s17, 5
        0xF4080301, 0x02000000,         # s_load_dwordx4 s[12:15], s[2:3], s1
        0xB0140092,                     # s_movk_i32 s20, 0x92      sampler word 0: clamp in U, V and W
        0xBE9503FF, 0x00C00000,         # s_mov_b32 s21, 0xc00000   word 1: max LOD 12
        0xBE9603FF, 0x18500000,         # s_mov_b32 s22, 0x18500000 word 2: linear min, mag and mip filters
        0xBE970380,                     # s_mov_b32 s23, 0          word 3: border colour
        0x7E0802FF, 0x3EC00000,         # v_mov_b32 v4, 0.375       u
        0x7E0A02F0,                     # v_mov_b32 v5, 0.5         v
        0xBF8CC07F,                     # s_waitcnt lgkmcnt(0)
        0xF09C0108, 0x00A10704,         # image_sample_lz v7, v[4:5], s[4:11], s[20:23] dmask:0x1 dim:2D
        0xBF8C3F70,                     # s_waitcnt vmcnt(0)
        0xE0700000, 0x80030700,         # buffer_store_dword v7, off, s[12:15], 0
        0xBF810000,                     # s_endpgm
    ]
    info = bytearray(48)
    info[24] = 5
    struct.pack_into("<3I", info, 36, 64, 1, 1)
    root = pack([2, 0, 24, 0, 24, 0x400])
    # FSSM user data: class 5, the resource heap base in s2. Used resources and
    # PSV: one inlined static sampler, register 0.
    return stage_container(0x50066, image(7, code, [(5, 0, 1, 0, 2)], [(0x401, 0, 0, 0, 0)]), [
        (b"SFI0", struct.pack("<Q", 1 << 25)),
        (b"PSV0", pack([48]) + info + pack([1, 16, 1, 0, 0, 0])),
        (b"RTS0", root),
    ])


# The `bindless-texture-store` program, checked with `llvm-mc -mcpu=gfx1030
# -show-encoding`. It stores one word to one texel of the heap texture at a
# guest index: the only kind of write a placed texture can receive through the
# heap. `bindless_compute_shader` above writes a buffer and no image.
BINDLESS_STORE_TEXTURE = 100
BINDLESS_STORE_TEXEL = (3, 2)
BINDLESS_STORE_EXPECTED = 0x7AB1E003


def bindless_texture_store_shader():
    """Direct-indexed store to one texel of a heap texture."""
    code = [
        0xB0100000 | BINDLESS_STORE_TEXTURE,  # s_movk_i32 s16, the texture's heap index
        0x8F008510,                           # s_lshl_b32 s0, s16, 5
        0xF40C0101, 0x00000000,               # s_load_dwordx8 s[4:11], s[2:3], s0
        0x7E0002FF, BINDLESS_STORE_EXPECTED,  # v_mov_b32 v0, the word
        0x7E020280 | BINDLESS_STORE_TEXEL[0],  # v_mov_b32 v1, x
        0x7E040280 | BINDLESS_STORE_TEXEL[1],  # v_mov_b32 v2, y
        0xBF8CC07F,                           # s_waitcnt lgkmcnt(0)
        0xF0201108, 0x00010001,               # image_store v0, v[1:2], s[4:11] dmask:0x1 dim:2D unorm
        0xBF810000,                           # s_endpgm
    ]
    info = bytearray(48)
    info[24] = 5
    struct.pack_into("<3I", info, 36, 64, 1, 1)
    root = pack([2, 0, 24, 0, 24, 0x400])
    # FSSM user data: class 5, the resource heap base in s2.
    return stage_container(0x50066, image(7, code, [(5, 0, 1, 0, 2)]), [
        (b"SFI0", struct.pack("<Q", 1 << 25)),
        (b"PSV0", pack([48]) + info + pack([0])),
        (b"RTS0", root),
    ])


# The `bindless-table-store` program, checked with `llvm-mc -mcpu=gfx1030
# -show-encoding`. Its root allows direct heap indexing, but it stores its word
# through a descriptor table: the typed UAV u1 at slot 1 of table 0, whose
# pointer arrives in s4. It can therefore write that one texture and no other.
BINDLESS_TABLE_SLOT = 1
BINDLESS_TABLE_EXPECTED = 0x7AB1E004


def bindless_table_store_shader():
    """A store to one texel of the texture at a table slot, under a root that allows heap indexing."""
    code = [
        0xBE800304,                           # s_mov_b32 s0, s4
        0xBE810380,                           # s_mov_b32 s1, 0
        0xF40C0200, 0xFA000000 | 32 * BINDLESS_TABLE_SLOT,  # s_load_dwordx8 s[8:15], s[0:1], slot
        0x7E0002FF, BINDLESS_TABLE_EXPECTED,  # v_mov_b32 v0, the word
        0x7E020280 | BINDLESS_STORE_TEXEL[0],  # v_mov_b32 v1, x
        0x7E040280 | BINDLESS_STORE_TEXEL[1],  # v_mov_b32 v2, y
        0xBF8CC07F,                           # s_waitcnt lgkmcnt(0)
        0xF0201108, 0x00020001,               # image_store v0, v[1:2], s[8:15] dmask:0x1 dim:2D unorm
        0xBF810000,                           # s_endpgm
    ]
    info = bytearray(48)
    info[24] = 5
    struct.pack_into("<3I", info, 36, 64, 1, 1)
    resources = ([6, 0, BINDLESS_TABLE_SLOT, BINDLESS_TABLE_SLOT],)  # PSV: typed UAV u1, space0
    psv = pack([len(info)]) + info + pack([len(resources), 16, *resources[0]])
    # FSSM user data: descriptor u1 at dword offset 8 of table 0 (s4). Used
    # resources: a typed UAV there, an eight-dword image descriptor.
    user = ([3, 8 * BINDLESS_TABLE_SLOT, 1, BINDLESS_TABLE_SLOT, 0x004],)
    used = ([0x303, 8 * BINDLESS_TABLE_SLOT, BINDLESS_TABLE_SLOT, 1, 0x408],)
    return stage_container(0x50066, image(7, code, user, used), [
        (b"SFI0", struct.pack("<Q", 1 << 25)),
        (b"PSV0", psv),
    ])


# The `large-dispatch` program, checked with `llvm-mc -mcpu=gfx1030 -show-encoding`.
# Under its root of one table the group ID arrives in s5, s6 and s7. Every
# thread of a group stores the ID and GROUP_MARK as four words at record
# x + 2y, so a dispatch two groups wide covers each record once.
GROUP_MARK = 0x6B1D0000
GROUP_CODE = (
    0xBE800304,                  # s_mov_b32 s0, s4: table 0
    0xBE810380,                  # s_mov_b32 s1, 0
    0xF4080200, 0xFA000000,      # s_load_dwordx4 s[8:11], s[0:1], 0: the u0 descriptor
    0x8F0C8106,                  # s_lshl_b32 s12, s6, 1
    0x800C050C,                  # s_add_u32 s12, s12, s5
    0x8F0C840C,                  # s_lshl_b32 s12, s12, 4
    0x7E020205,                  # v_mov_b32 v1, s5
    0x7E040206,                  # v_mov_b32 v2, s6
    0x7E060207,                  # v_mov_b32 v3, s7
    0x7E0802FF, GROUP_MARK,      # v_mov_b32 v4, GROUP_MARK
    0x7E00020C,                  # v_mov_b32 v0, s12
    0xBF8CC07F,                  # s_waitcnt lgkmcnt(0)
    0xE0781000, 0x80020100,      # buffer_store_dwordx4 v[1:4], v0, s[8:11], 0 offen
    0xBF810000,                  # s_endpgm
)


def group_shader():
    """A program that stores its group ID, for dispatches over 65,535 groups."""
    return uav_shader(list(GROUP_CODE), width=64)


# The `compute-heap-stride` program, checked with `llvm-mc -mcpu=gfx1030
# -show-encoding`. It is a root-less compute program: each thread takes the
# thread number, indexes the input structured buffer in the resource heap with
# it (an IDXEN raw load, which needs the buffer's record stride) and stores the
# word it found into the same record of the output structured buffer, through
# the other heap descriptor (a byte-offset store one record per thread). The
# load cannot be expressed without the stride: the runtime has to retry the
# translation with the heap stride table appended, as it does for the Reflected
# and Original arms. The heap base arrives in s2 (the one FSSM class-5 user
# record) and the descriptors are loaded with a 32-byte step, the way the
# console stores a heap slot's descriptor there.
HEAP_STRIDE_INPUT_SLOT = 100
HEAP_STRIDE_OUTPUT_SLOT = 110
HEAP_STRIDE_THREADS = 64
HEAP_STRIDE_RECORD_BYTES = 32


def heap_stride_compute_shader():
    code = [
        0xB0140000 | HEAP_STRIDE_INPUT_SLOT,  # s_movk_i32 s20, the input's heap slot
        0x8F158514,                           # s_lshl_b32 s21, s20, 5   (slot * 32 bytes)
        0xF4080201, 0x2A000000,               # s_load_dwordx4 s[8:11], s[2:3], s21
        0xBF8CC07F,                           # s_waitcnt lgkmcnt(0)
        0xE0302000, 0x80020400,               # buffer_load_dword v4, v0, s[8:11], 0 idxen
        0xBF8C3F70,                           # s_waitcnt vmcnt(0)
        0xB0140000 | HEAP_STRIDE_OUTPUT_SLOT,  # s_movk_i32 s20, the output's heap slot
        0x8F158514,                           # s_lshl_b32 s21, s20, 5   (slot * 32 bytes)
        0xF4080601, 0x2A000000,               # s_load_dwordx4 s[24:27], s[2:3], s21
        0xBF8CC07F,                           # s_waitcnt lgkmcnt(0)
        0x34120085,                           # v_lshlrev_b32 v9, 5, v0
        0xE0701000, 0x80060409,               # buffer_store_dword v4, v9, s[24:27], 0
        0xBF810000,                           # s_endpgm
    ]
    info = bytearray(48)
    info[24] = 5
    struct.pack_into("<3I", info, 36, HEAP_STRIDE_THREADS, 1, 1)
    psv = pack([48]) + info + pack([0])
    # FSSM: one class-5 user record, the heap base (s2). No used-resource rows.
    return stage_container(0x50060, image(7, code, [(5, 0, 1, 0, 2)]), [
        (b"SFI0", struct.pack("<Q", 1 << 25)),
        (b"PSV0", psv),
    ])


# The `late-pipeline` program: each of 64 threads runs its index through a
# chain of LATE_STEPS dependent `v_xor_b32`, `v_mul_u32_u24` and
# `v_add_nc_u32` with literals, which DXC cannot fold, and stores the result as
# word tid of u0. The chain makes the DXIL larger than the provider's large
# compile size (512 KiB), so under a short pipeline deadline the program gets a
# stand-in and its compile finishes later.
LATE_STEPS = 60000


def late_shader():
    """The program and the word each thread stores."""
    mask, seed, literals = 0xFFFFFFFF, 0x2545F491, []
    code = [0xBE800304,              # s_mov_b32 s0, s4: table 0
            0xBE810380,              # s_mov_b32 s1, 0
            0xF4080200, 0xFA000000,  # s_load_dwordx4 s[8:11], s[0:1], 0: the u0 descriptor
            0x7E020300]              # v_mov_b32 v1, v0
    ops = (0x3A0202FF, 0x160202FF, 0x4A0202FF)  # v_xor_b32, v_mul_u32_u24, v_add_nc_u32 v1, literal, v1
    for step in range(LATE_STEPS):
        seed = (seed * 1103515245 + 12345) & mask
        literals.append(seed | 1)
        code += [ops[step % 3], seed | 1]
    code += [0x34000082,              # v_lshlrev_b32 v0, 2, v0
             0xBF8CC07F,              # s_waitcnt lgkmcnt(0)
             0xE0701000, 0x80020100,  # buffer_store_dword v1, v0, s[8:11], 0 offen
             0xBF810000]              # s_endpgm
    expected = []
    for tid in range(64):
        x = tid
        for step, literal in enumerate(literals):
            kind = step % 3
            x = x ^ literal if kind == 0 else ((literal & 0xFFFFFF) * (x & 0xFFFFFF)) & mask if kind == 1 else (x + literal) & mask
        expected.append(x)
    return uav_shader(code, width=64), expected


def declaration(name, data):
    values = ",".join(str(byte) for byte in data)
    return (f"constexpr unsigned char {name}[] = {{{values}}};\n"
            "// The name its translation is cached under.\n"
            f"constexpr wchar_t {name}_digest[] = L\"{hashlib.sha256(data).hexdigest()}\";\n")


def header():
    words = ",".join(f"{word}u" for word in wave_expected())
    lane_words = ",".join(f"{word}u" for word in lane_expected())
    return ("// Generated from shader_fixture.py.\n"
            + declaration("xbox_shader", shader()) + declaration("wave_shader", wave_shader())
            + f"constexpr unsigned wave_shader_expected[] = {{{words}}};\n"
            + declaration("lane_shader", lane_shader())
            + f"constexpr unsigned lane_shader_expected[] = {{{lane_words}}};\n"
            + declaration("recovered_root_shader", recovered_root_shader())
            + declaration("root_srv_shader", root_srv_shader())
            + declaration("bindless_compute_shader", bindless_compute_shader())
            + declaration("bindless_table_compute_shader", bindless_compute_shader(table=True))
            + f"constexpr unsigned bindless_index = {BINDLESS_INDEX};\nconstexpr unsigned bindless_expected = {BINDLESS_EXPECTED:#x};\n"
            + declaration("bindless_sampler_shader", bindless_sampler_shader())
            + f"constexpr unsigned bindless_texture = {BINDLESS_TEXTURE}, bindless_output = {BINDLESS_OUTPUT};\n"
            + declaration("bindless_texture_store_shader", bindless_texture_store_shader())
            + f"constexpr unsigned bindless_store_texture = {BINDLESS_STORE_TEXTURE}, "
              f"bindless_store_x = {BINDLESS_STORE_TEXEL[0]}, bindless_store_y = {BINDLESS_STORE_TEXEL[1]}, "
              f"bindless_store_expected = {BINDLESS_STORE_EXPECTED:#x};\n"
            + declaration("bindless_table_store_shader", bindless_table_store_shader())
            + f"constexpr unsigned bindless_table_slot = {BINDLESS_TABLE_SLOT}, "
              f"bindless_table_expected = {BINDLESS_TABLE_EXPECTED:#x};\n"
            + declaration("group_shader", group_shader())
            + f"constexpr unsigned group_shader_mark = {GROUP_MARK:#x};\n"
            + declaration("root_shader", root_shader(0x40)) + declaration("root_refused_shader", root_shader(0xA0))
            + f"constexpr unsigned root_shader_expected = {ROOT_EXPECTED:#x};\n"
            + declaration("compute_heap_stride_shader", heap_stride_compute_shader())
            + f"constexpr unsigned compute_heap_stride_input_slot = {HEAP_STRIDE_INPUT_SLOT}u, compute_heap_stride_output_slot = {HEAP_STRIDE_OUTPUT_SLOT}u;\n"
            + f"constexpr unsigned compute_heap_stride_bytes = {HEAP_STRIDE_RECORD_BYTES}u, compute_heap_stride_records = {HEAP_STRIDE_THREADS}u;\n"
            + late_declarations())


def late_declarations():
    data, expected = late_shader()
    words = ",".join(f"{word:#x}u" for word in expected)
    return declaration("late_shader", data) + f"constexpr unsigned late_shader_expected[64] = {{{words}}};\n"


if __name__ == "__main__":
    Path(sys.argv[1]).write_text(header())
