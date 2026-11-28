"""The synthetic Xbox programs of the raster self-tests.

All are small hand-written console programs, not taken from any game. The
vertex program draws a fullscreen triangle. The `raster-tables` pixel program
reads one dword through table 1 of a root with two descriptor tables, from the
table pointer in s9 at slot 2, and writes it to a uint render target. Its FSSM
records and PSV binding name that descriptor `t3, space2`, a register the
title's root does not declare, so the pipeline needs the root its tables are
reflected into.

The `raster-samplers` pixel program samples a texture at table 1 slot 0 with a
linear clamped sampler it builds from literal words, as the console compiles a
static sampler into the program. It declares that sampler as `s0`, where the
title's root has a point-filtered static sampler, so only the literal words
give the console's result.

The `mesh-samplers` pixel program is the same, drawn through a mesh pipeline. It
declares two samplers, `s0` and `s1`, both of which the title's root has as
point-filtered static samplers, so no declared register can stand in for the
literal state. Its mesh program's FSSM records name root constant `b501` where
the root declares `b500`, so the provider rewrites the root for the mesh stage,
and the shader worker translates its console code against that root.

`mesh-sampler-rootless` draws through that mesh program under a root with no
static samplers, first with the `raster-samplers` pixel program and then with the
two-sampler one carrying an empty feature part (`SFI0`), which shows that it
reads no sampler from the heap.
"""
from pathlib import Path
import struct
import sys
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from tools.reference.portable_link_fixture import VERTEX_CODE, pack  # noqa: E402

# gfx1030, checked with `llvm-mc -mcpu=gfx1030 -show-encoding`.
PIXEL_CODE = (
    0xBE800309,              # s_mov_b32 s0, s9          table 1 pointer
    0xBE810380,              # s_mov_b32 s1, 0
    0xF4080100, 0xFA000040,  # s_load_dwordx4 s[4:7], s[0:1], 0x40   slot 2
    0xBF8CC07F,              # s_waitcnt lgkmcnt(0)
    0xE0300000, 0x80010000,  # buffer_load_dword v0, off, s[4:7], 0
    0xBF8C3F70,              # s_waitcnt vmcnt(0)
    0xF8001801, 0x00000000,  # exp mrt0 v0, off, off, off done vm
    0xBF810000,              # s_endpgm
)
# FSSM rows [class, dword offset, width or register, register or shape, location].
# The user-data row puts the descriptor in table 1 (s9) at dword 16; the used row
# names it a raw SRV, register 3, with its table in the location's high half.
PIXEL_USER = ([3, 16, 1, 3, 0x109],)
PIXEL_USED = ([0x300, 16, 3, 1, 0x1_0904],)
# PSV binding [type, space, lower, upper]: raw SRV t3, space2.
PIXEL_RESOURCES = ([4, 2, 3, 3],)
# The value the pixel program must read, and what the other descriptors hold.
EXPECTED = 0x7AB1E001
DECOY = 0xD0000000

# gfx1030, checked with `llvm-mc -mcpu=gfx1030 -show-encoding`.
SAMPLER_PIXEL_CODE = (
    0xBE800309,              # s_mov_b32 s0, s9          table 1 pointer
    0xBE810380,              # s_mov_b32 s1, 0
    0xF40C0100, 0xFA000000,  # s_load_dwordx8 s[4:11], s[0:1], 0x0   slot 0
    0xB0140092,              # s_movk_i32 s20, 0x92      sampler word 0: clamp in U, V and W
    0xBE9503FF, 0x00C00000,  # s_mov_b32 s21, 0xc00000   word 1: max LOD 12
    0xBE9603FF, 0x18500000,  # s_mov_b32 s22, 0x18500000 word 2: linear min, mag and mip filters
    0xBE970380,              # s_mov_b32 s23, 0          word 3: border colour
    0x7E0802FF, 0x3EC00000,  # v_mov_b32 v4, 0.375       u
    0x7E0A02F0,              # v_mov_b32 v5, 0.5         v
    0xBF8CC07F,              # s_waitcnt lgkmcnt(0)
    0xF09C0108, 0x00A10704,  # image_sample_lz v7, v[4:5], s[4:11], s[20:23] dmask:0x1 dim:2D
    0xBF8C3F70,              # s_waitcnt vmcnt(0)
    0xF8001801, 0x00000007,  # exp mrt0 v7, off, off, off done vm
    0xBF810000,              # s_endpgm
)
# The texture descriptor is table 1 (s9) at dword 0, a typed SRV, register 0;
# the sampler is an inlined static sampler, register 0.
SAMPLER_PIXEL_USER = ([3, 0, 1, 0, 0x109],)
SAMPLER_PIXEL_USED = ([0x300, 0, 0, 0, 0x1_0908], [0x401, 0, 0, 0, 0])
# PSV bindings: texture t0, space1 (what the title's root has at that slot) and sampler s0, space0.
SAMPLER_PIXEL_RESOURCES = ([3, 1, 0, 0], [1, 0, 0, 0])
# A 2x1 texture of 1.0 and 3.0 sampled at u = 0.375: a quarter of the way
# between the texel centres. Linear filtering gives 1.5, point filtering 1.0.
SAMPLER_TEXELS = (1.0, 3.0)
SAMPLER_LINEAR, SAMPLER_POINT = 1.5, 1.0

# The mesh pipeline's pixel program declares a second static sampler, s1, so
# that neither declared register is the program's only one.
MESH_SAMPLER_PIXEL_USED = (*SAMPLER_PIXEL_USED, [0x401, 0, 1, 0, 0])
MESH_SAMPLER_PIXEL_RESOURCES = (*SAMPLER_PIXEL_RESOURCES, [1, 0, 1, 1])
# The mesh program's records: root constant parameter 2 in s8, named b500 by the
# title's root and b501 by the program, so the mesh stage gets a root of its own.
MESH_USER = ([0, 0, 1, 501, 0x200],)
MESH_USED = ([2, 0, 501, 0, 0x2_0004],)
MESH_RESOURCES = ([2, 0, 501, 501],)
# Synthetic RDNA2 NGG, 67 vertices, a guarded FP16-to-FP32 position export.
# Assembled from tools.check_translated_mesh_gpu.program(67, half=True).
MESH_CODE = (
    0xd7650000, 0x020100c1, 0x7e000505, 0x9381ff03, 0x00040018, 0xd7660000,
    0x020200c1, 0x900a8700, 0xd7460001, 0x04010c01, 0xbe9403ff, 0x00000043,
    0x7e2802ff, 0x80000000, 0x7e3602ff, 0x04210440, 0x7d840280, 0x02283714,
    0x7d8402ff, 0x00000042, 0xd5010015, 0x01a9fef3, 0x40400000, 0x7d8402ff,
    0x00000041, 0xd5010016, 0x01a9fef3, 0x40400000, 0x7e2e02f0, 0x7e3002f2,
    0xbeac047e, 0x7da80214, 0xbf880002, 0x7e0e1515, 0x7e0e1707, 0xbefe042c,
    0xbf068014, 0x8580807e, 0xb5940080, 0x8582807e, 0x88800000, 0x88820202,
    0x88800002, 0xd501000a, 0x00010280, 0x7e00050a, 0x87000081, 0xbf068100,
    0xbe80047e, 0x8582807e, 0xd501000b, 0x00090214, 0x8984c102, 0x7e0c050b,
    0x7da40280, 0xbf88000d, 0x8f078c06, 0x8aea027e, 0x88060607, 0xbefc0306,
    0xbf900009, 0xbf8c0000, 0xbf870006, 0xbf8a0000, 0x7e147081, 0xf80008c0,
    0x00000000, 0xf8000941, 0x0000000a, 0xbf8cff0f, 0xbefe0400, 0xbf8a0000,
    0x7d880214, 0x8780046a, 0xbe862400, 0xbf880012, 0xd548000c, 0x02291514,
    0xd548000d, 0x02292914, 0x361c28ff, 0x000003ff, 0x7d861814, 0xd4c30002,
    0x00021a14, 0xd4c30000, 0x00021c14, 0x8882026a, 0x88800200, 0xd501000f,
    0x0001ff14, 0x80000000, 0xf8000941, 0x0000000f, 0xbf8cff0f, 0xbefe0406,
    0x7d880214, 0x8780046a, 0xbe802400, 0xbf880002, 0xf80008cf, 0x18171607,
    0xbf810000,
)

# The `raster-heap-stride` programs, checked the same way. The vertex program
# takes the descriptor of a structured buffer from the resource heap (its base in
# s8, the one FSSM class-5 user record) and reads its position at its vertex
# number: an indexed raw load, whose byte address needs the buffer's stride.
HEAP_STRIDE_INDEX = 100
HEAP_STRIDE_VERTEX_CODE = (
    0xB0140000 | HEAP_STRIDE_INDEX,  # s_movk_i32 s20, the buffer's heap index
    0x8F158514,                      # s_lshl_b32 s21, s20, 5
    0xF4080304, 0x2A000000,          # s_load_dwordx4 s[12:15], s[8:9], s21
    0xBF8CC07F,                      # s_waitcnt lgkmcnt(0)
    0xE0382000, 0x80030400,          # buffer_load_dwordx4 v[4:7], v0, s[12:15], 0 idxen
    0xBF8C3F70,                      # s_waitcnt vmcnt(0)
    0xF80008CF, 0x07060504,          # exp pos0 v4, v5, v6, v7 done
    0xBF810000,                      # s_endpgm
)
HEAP_STRIDE_USER = ([5, 0, 1, 0, 8],)
HEAP_STRIDE_EXPECTED = 0x7AB1E005
HEAP_STRIDE_PIXEL_CODE = (
    0x7E0002FF, HEAP_STRIDE_EXPECTED,  # v_mov_b32 v0, the word
    0xF8001801, 0x00000000,            # exp mrt0 v0, off, off, off done vm
    0xBF810000,                        # s_endpgm
)

# The `raster-sampler-waterfall` vertex program, checked the same way. It takes a
# texture from the resource heap (base in s8, the FSSM class-5 record) and a
# sampler from the sampler heap at a constant index (base in s10, class 6), the way
# RAGE's VfxVBuffer vertex programs do: both arms of an if/else copy the sampler
# into v[10:13], and a waterfall loop reads it back for the sample. The sampled
# value is the position's w, so a wrong sampler moves the triangle off the target.
WATERFALL_TEXTURE_INDEX, WATERFALL_SAMPLER_INDEX = 100, 5
WATERFALL_VERTEX_CODE = (
    0xBE9C030A,               # s_mov_b32 s28, s10
    0xBE9D0380,               # s_mov_b32 s29, 0
    0xB0140064,               # s_movk_i32 s20, 0x64
    0x8F158514,               # s_lshl_b32 s21, s20, 5
    0xF40C0304, 0x2A000000,   # s_load_dwordx8 s[12:19], s[8:9], s21
    0x7D880081,               # v_cmp_gt_u32_e32 vcc, 1, v0
    0xBE96246A,               # s_and_saveexec_b64 s[22:23], vcc
    0xBF880007,               # s_cbranch_execz 7
    0xF408060E, 0xFA000050,   # s_load_dwordx4 s[24:27], s[28:29], 0x50
    0xBF8CC07F,               # s_waitcnt lgkmcnt(0)
    0x7E140218,               # v_mov_b32_e32 v10, s24
    0x7E160219,               # v_mov_b32_e32 v11, s25
    0x7E18021A,               # v_mov_b32_e32 v12, s26
    0x7E1A021B,               # v_mov_b32_e32 v13, s27
    0x8AFE7E16,               # s_andn2_b64 exec, s[22:23], exec
    0xBF880007,               # s_cbranch_execz 7
    0xF408060E, 0xFA000050,   # s_load_dwordx4 s[24:27], s[28:29], 0x50
    0xBF8CC07F,               # s_waitcnt lgkmcnt(0)
    0x7E140218,               # v_mov_b32_e32 v10, s24
    0x7E160219,               # v_mov_b32_e32 v11, s25
    0x7E18021A,               # v_mov_b32_e32 v12, s26
    0x7E1A021B,               # v_mov_b32_e32 v13, s27
    0xBEFE0416,               # s_mov_b64 exec, s[22:23]
    0x7E0802FF, 0x3EC00000,   # v_mov_b32_e32 v4, 0x3ec00000
    0x7E0A02F0,               # v_mov_b32_e32 v5, 0.5
    0xBE9E047E,               # s_mov_b64 s[30:31], exec
    0x7E40050A,               # v_readfirstlane_b32 s32, v10
    0x7E42050B,               # v_readfirstlane_b32 s33, v11
    0x7E44050C,               # v_readfirstlane_b32 s34, v12
    0x7E46050D,               # v_readfirstlane_b32 s35, v13
    0x7DC41420,               # v_cmp_eq_u64_e32 vcc, s[32:33], v[10:11]
    0xD4E20024, 0x00021822,   # v_cmp_eq_u64_e64 s[36:37], s[34:35], v[12:13]
    0x87A4246A,               # s_and_b64 s[36:37], vcc, s[36:37]
    0xBEA42424,               # s_and_saveexec_b64 s[36:37], s[36:37]
    0xBF8CC07F,               # s_waitcnt lgkmcnt(0)
    0xF09C0108, 0x01030E04,   # image_sample_lz v14, v[4:5], s[12:19], s[32:35] dmask:0x1 dim:SQ_RSRC_IMG_2D
    0x89FE247E,               # s_xor_b64 exec, exec, s[36:37]
    0xBF89FFF2,               # s_cbranch_execnz -14
    0xBEFE041E,               # s_mov_b64 exec, s[30:31]
    0xBF8C3F70,               # s_waitcnt vmcnt(0)
    0x7E1202FF, 0x40400000,   # v_mov_b32_e32 v9, 0x40400000
    0x7D840082,               # v_cmp_eq_u32_e32 vcc, 2, v0
    0x020C12F3,               # v_cndmask_b32_e32 v6, -1.0, v9, vcc
    0x7D840081,               # v_cmp_eq_u32_e32 vcc, 1, v0
    0x020E12F3,               # v_cndmask_b32_e32 v7, -1.0, v9, vcc
    0x7E100280,               # v_mov_b32_e32 v8, 0
    0xF80008CF, 0x0E080706,   # exp pos0 v6, v7, v8, v14 done
    0xBF810000,               # s_endpgm
)
WATERFALL_USER = ([5, 0, 1, 0, 8], [6, 0, 1, 0, 10])
# The texture is the sampler programs' 2x1 one, sampled at (0.375, 0.5): the point
# sampler at the program's index gives w = 1.0, the linear ones 1.5.
# One 32-byte record per vertex of a fullscreen triangle: the position, then four
# zero floats. A load with any other stride reads a different triangle.
HEAP_STRIDE_BYTES = 32
HEAP_STRIDE_POSITIONS = ((-1.0, -1.0), (-1.0, 3.0), (3.0, -1.0))

ABSENT = 0xDEADBEEFDEADBEEF
ROWS_AT, CODE_AT = 0x240, 0x280


def image(slot, code, user=(), used=(), inputs=0):
    """An FSSM 1.1 image with one stage descriptor at 0x68, after the directory."""
    # The code follows the rows; more than three rows move it later.
    code_at = max(CODE_AT, -(-(ROWS_AT + 20 * (len(user) + len(used))) // 0x40) * 0x40)
    data = bytearray(code_at)
    struct.pack_into("<Q", data, 0, 6)
    for n in range(12):
        struct.pack_into("<Q", data, 8 + 8 * n, 0x68 if n == slot else ABSENT)
    struct.pack_into("<QQ", data, 0x68, code_at, 4 * len(code))
    # Descriptor fields at 0xd8: user-data and used-resource row offsets and counts.
    used_at = ROWS_AT + 20 * len(user)
    struct.pack_into("<QQII", data, 0xd8, ROWS_AT if user else 0, used_at if used else 0, len(user), len(used))
    for index, row in enumerate((*user, *used)):
        struct.pack_into("<5I", data, ROWS_AT + 20 * index, *row)
    struct.pack_into("<I", data, 0x238, inputs)  # SPI_PS_INPUT_ENA
    data.extend(pack(code))
    return bytes(data)


def signature(rows):
    """Signature part rows: (name, index, system value, component type, register, mask, read/write mask)."""
    data = bytearray(pack([len(rows), 8]))
    names = bytearray()
    for name, index, sysval, kind, register, mask, used in rows:
        data.extend(pack([8 + 24 * len(rows) + len(names), index, sysval, kind, register, mask | used << 8]))
        names.extend(name.encode("ascii") + b"\0")
    return bytes(data + names)


def validation(stage, resources=()):
    info = bytearray(48)
    info[24] = stage
    psv = pack([len(info)]) + info + pack([len(resources)])
    if resources:
        psv += pack([16, *(word for row in resources for word in row)])
    return psv


def container(version, fssm_image, parts):
    fssm = b"FSSM" + pack([0x101]) + struct.pack("<Q", len(fssm_image)) + fssm_image
    compressed = zlib.compress(fssm)
    section = b"XBCH" + pack([3]) + struct.pack("<QQ", len(compressed), len(fssm)) + compressed
    xdat = b"XDAT" + pack([2, 36, 13, 60, len(section), 7, 60 + ((len(section) + 3) // 4) * 4, 4]) + section
    xdat += bytes((-len(xdat)) % 4) + b"NOBC"
    body = pack([version, (24 + len(xdat)) // 4]) + b"DXIL" + pack([0x100, 16 + len(xdat) - 4, 4]) + xdat
    chunks = [(b"DXIL", body), *parts]
    start = 32 + 4 * len(chunks)
    offsets, data = [], bytearray()
    for tag, part in chunks:
        offsets.append(start + len(data))
        data.extend(tag + pack([len(part)]) + part)
        data.extend(bytes((-len(data)) % 4))
    return b"DXBC" + bytes(16) + pack([1, start + len(data), len(chunks), *offsets]) + data


def vertex():
    return container(0x10060, image(7, VERTEX_CODE), [
        (b"ISGN", signature([("SV_VertexID", 0, 6, 1, 0, 1, 1)])),
        (b"OSGN", signature([("SV_Position", 0, 1, 3, 0, 15, 0), ("TEXCOORD", 0, 0, 3, 1, 15, 0)])),
        (b"PSV0", validation(1)),
    ])


def pixel():
    return container(0x60, image(5, PIXEL_CODE, PIXEL_USER, PIXEL_USED, inputs=2), [
        (b"ISGN", signature([("SV_Position", 0, 1, 3, 0, 15, 0)])),
        (b"OSGN", signature([("SV_Target", 0, 64, 1, 0, 1, 0)])),
        (b"PSV0", validation(0, PIXEL_RESOURCES)),
    ])


def sampler_pixel():
    return container(0x60, image(5, SAMPLER_PIXEL_CODE, SAMPLER_PIXEL_USER, SAMPLER_PIXEL_USED, inputs=2), [
        (b"ISGN", signature([("SV_Position", 0, 1, 3, 0, 15, 0)])),
        (b"OSGN", signature([("SV_Target", 0, 64, 3, 0, 1, 0)])),
        (b"PSV0", validation(0, SAMPLER_PIXEL_RESOURCES)),
    ])


def mesh_sampler_pixel():
    return container(0x60, image(5, SAMPLER_PIXEL_CODE, SAMPLER_PIXEL_USER, MESH_SAMPLER_PIXEL_USED, inputs=2), [
        (b"ISGN", signature([("SV_Position", 0, 1, 3, 0, 15, 0)])),
        (b"OSGN", signature([("SV_Target", 0, 64, 3, 0, 1, 0)])),
        (b"PSV0", validation(0, MESH_SAMPLER_PIXEL_RESOURCES)),
    ])


def rootless_sampler_pixel():
    """The two-sampler pixel program with a feature part that names no heap indexing."""
    return container(0x60, image(5, SAMPLER_PIXEL_CODE, SAMPLER_PIXEL_USER, MESH_SAMPLER_PIXEL_USED, inputs=2), [
        (b"SFI0", bytes(8)),
        (b"ISGN", signature([("SV_Position", 0, 1, 3, 0, 15, 0)])),
        (b"OSGN", signature([("SV_Target", 0, 64, 3, 0, 1, 0)])),
        (b"PSV0", validation(0, MESH_SAMPLER_PIXEL_RESOURCES)),
    ])


def heap_stride_vertex():
    return container(0x10060, image(7, HEAP_STRIDE_VERTEX_CODE, HEAP_STRIDE_USER), [
        (b"SFI0", struct.pack("<Q", 1 << 25)),
        (b"ISGN", signature([("SV_VertexID", 0, 6, 1, 0, 1, 1)])),
        (b"OSGN", signature([("SV_Position", 0, 1, 3, 0, 15, 0)])),
        (b"PSV0", validation(1)),
    ])


def sampler_waterfall_vertex():
    return container(0x10060, image(7, WATERFALL_VERTEX_CODE, WATERFALL_USER), [
        (b"SFI0", struct.pack("<Q", 3 << 25)),
        (b"ISGN", signature([("SV_VertexID", 0, 6, 1, 0, 1, 1)])),
        (b"OSGN", signature([("SV_Position", 0, 1, 3, 0, 15, 0)])),
        (b"PSV0", validation(1)),
    ])


def heap_stride_pixel():
    return container(0x60, image(5, HEAP_STRIDE_PIXEL_CODE, inputs=2), [
        (b"ISGN", signature([("SV_Position", 0, 1, 3, 0, 15, 0)])),
        (b"OSGN", signature([("SV_Target", 0, 64, 1, 0, 1, 0)])),
        (b"PSV0", validation(0)),
    ])


def mesh():
    """Synthetic NGG mesh program with guarded FP16 position (stage 13)."""
    info = bytearray(validation(13, MESH_RESOURCES))
    info[31] = 2
    struct.pack_into("<HH", info, 16, 128, 128)
    struct.pack_into("<3I", info, 40, 128, 1, 1)
    return container(0xD0065, image(2, MESH_CODE, MESH_USER, MESH_USED), [
        (b"OSGN", signature([("SV_Position", 0, 1, 3, 0, 15, 0)])),
        (b"PSV0", info),
    ])


def header(vertex_data, pixel_data, sampler_data=None):
    def array(name, data):
        return f"constexpr unsigned char {name}[] = {{{','.join(str(byte) for byte in data)}}};\n"
    sampler_data = sampler_pixel() if sampler_data is None else sampler_data
    texels = ", ".join(f"{texel!r}f" for texel in SAMPLER_TEXELS)
    records = ", ".join(f"{value!r}f" for x, y in HEAP_STRIDE_POSITIONS
                        for value in (x, y, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0))
    return ("// Generated from raster_fixture.py.\n"
            + array("raster_vertex", vertex_data) + array("raster_pixel", pixel_data)
            + f"constexpr UINT raster_expected = {EXPECTED:#x}, raster_decoy = {DECOY:#x};\n"
            + array("raster_sampler_pixel", sampler_data)
            + f"constexpr float raster_sampler_texels[] = {{{texels}}};\n"
            + f"constexpr float raster_sampler_linear = {SAMPLER_LINEAR!r}f, raster_sampler_point = {SAMPLER_POINT!r}f;\n"
            + array("mesh_sampler_mesh", mesh()) + array("mesh_sampler_pixel", mesh_sampler_pixel())
            + array("mesh_rootless_pixel", rootless_sampler_pixel())
            + array("heap_stride_vertex", heap_stride_vertex()) + array("heap_stride_pixel", heap_stride_pixel())
            + f"constexpr UINT heap_stride_index = {HEAP_STRIDE_INDEX}, heap_stride_bytes = {HEAP_STRIDE_BYTES}, "
              f"heap_stride_expected = {HEAP_STRIDE_EXPECTED:#x};\n"
            + f"constexpr float heap_stride_records[] = {{{records}}};\n"
            + array("waterfall_vertex", sampler_waterfall_vertex())
            + f"constexpr UINT waterfall_texture_index = {WATERFALL_TEXTURE_INDEX}, "
              f"waterfall_sampler_index = {WATERFALL_SAMPLER_INDEX};\n")


if __name__ == "__main__":
    Path(sys.argv[1]).write_text(header(vertex(), pixel()))
