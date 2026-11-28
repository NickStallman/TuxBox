"""Run translated compute shaders over generated resources and compare what they write.

The runner (tools/reference/execute-shaders/guest.cpp) binds each resource a
translated program declares, filled with contents seeded from the request's
name, dispatches a small grid and reads every writable resource back. Two
translations of the same request get the same inputs, so their outputs can be
compared: a change that alters what a real shader computes shows.

This module holds the parts that need no GPU: reading the bindings from the
HLSL, the root signature and resource layout handed to the runner, the seeded
contents, reading the runner's receipt, and sorting the outcomes. Nothing here
keeps shader bytes or HLSL; outcomes hold request names, digests and reasons.
"""
from dataclasses import dataclass
import hashlib
import re
import struct

# DXGI formats by component type and count; three components use four.
FORMATS = {("float", 1): 41, ("float", 2): 16, ("float", 4): 2,
           ("uint", 1): 42, ("uint", 2): 17, ("uint", 4): 3,
           ("sint", 1): 43, ("sint", 2): 18, ("sint", 4): 4}
TEXTURES = {"Texture1D": "1d", "Texture1DArray": "1darray", "Texture2D": "2d", "Texture2DArray": "2darray",
            "Texture3D": "3d", "TextureCube": "cube", "TextureCubeArray": "cubearray"}
STORAGE = {"RWTexture1D": "1d", "RWTexture1DArray": "1darray", "RWTexture2D": "2d",
           "RWTexture2DArray": "2darray", "RWTexture3D": "3d"}
# Width, height and depth or layers of each generated texture.
EXTENTS = {"1d": (64, 1, 1), "1darray": (64, 1, 4), "2d": (32, 32, 1), "2darray": (32, 32, 4),
           "3d": (16, 16, 8), "cube": (16, 16, 6), "cubearray": (16, 16, 12)}
TYPED_ELEMENTS = 1024
RAW_BYTES = 16384
GRID = (2, 2, 1)

DECLARATION = re.compile(
    r"^\s*(?:globallycoherent\s+)?(?P<type>\w+)(?:<(?P<element>[\w ]+)>)?\s+(?P<name>\w+)(?P<array>\[\d*\])?"
    r"\s*:\s*register\((?P<cls>[tusb])(?P<register>\d+)(?:\s*,\s*space(?P<space>\d+))?\)\s*;\s*$")
CBUFFER = re.compile(
    r"^\s*cbuffer\s+\w+\s*:\s*register\(b(?P<register>\d+)(?:\s*,\s*space(?P<space>\d+))?\)\s*\{(?P<body>[^}]*)\}\s*;?\s*$")
MEMBER = re.compile(r"^\s*(?:uint|int|float)([1-4]?)\s+\w+(?:\[(\d+)\])?\s*$")
WORD_STRUCT = re.compile(r"struct\s+(\w+)\s*\{\s*uint\s+w\[(\d+)\];\s*\};")
WAVE_SIZE = re.compile(r"\[WaveSize\((\d+)")
ELEMENT = re.compile(r"^(?:min16|min10)?(float|half|uint|int|dword)([1-4]?)$")


class Unsupported(Exception):
    """A program the runner can't give inputs to. The message is the skip reason."""


@dataclass(frozen=True)
class Binding:
    kind: str          # cbv, srv_tex, uav_tex, srv_typed, uav_typed, srv_raw, uav_raw, srv_struct, uav_struct, sampler, sampler_cmp
    register: str      # for example t191 or u31
    space: int
    dimension: str = "-"
    format: int = 0
    count: int = 1
    width: int = 0     # texels; elements of typed and structured buffers; bytes of constant and raw buffers
    height: int = 0
    depth: int = 0
    stride: int = 0
    family: str = "mixed"  # how seeded contents are chosen: float, uint, sint or mixed

    @property
    def writable(self):
        return self.kind.startswith("uav_")

    @property
    def sampler(self):
        return self.kind.startswith("sampler")

    def element_bytes(self):
        return {2: 16, 3: 16, 4: 16, 16: 8, 17: 8, 18: 8}.get(self.format, 4)

    def bytes(self):
        if self.kind in ("srv_tex", "uav_tex"):
            return self.width * self.height * self.depth * self.element_bytes()
        if self.kind in ("srv_typed", "uav_typed"):
            return self.width * self.element_bytes()
        if self.kind in ("srv_struct", "uav_struct"):
            return self.width * self.stride
        return self.width

    def line(self):
        return (f"{self.kind} {self.dimension} {self.format} {self.count} {self.width} {self.height} "
                f"{self.depth} {self.stride}")


def element_format(element):
    """The family and DXGI format for a typed resource's element type."""
    found = ELEMENT.match((element or "float4").replace(" ", ""))
    if not found:
        raise Unsupported(f"element type {element}")
    family = {"half": "float", "dword": "uint", "int": "sint"}.get(found[1], found[1])
    components = int(found[2] or 1)
    return family, FORMATS[family, 4 if components == 3 else components]


def cbuffer_bytes(body):
    """Bytes a constant buffer body declares, rounded to a whole 256-byte view."""
    total = 0
    for member in filter(str.strip, body.split(";")):
        found = MEMBER.match(member)
        if not found:
            raise Unsupported("constant buffer member that isn't a word or word vector")
        # Each array element takes a whole 16-byte register.
        total += 16 * int(found[2]) if found[2] else 16
    return min(65536, max(256, -(-total // 256) * 256))


def bindings(hlsl):
    """Every resource a translated program declares, in register order, or Unsupported."""
    if "DescriptorHeap" in hlsl:
        raise Unsupported("indexes the descriptor heap directly")
    strides = {name: 4 * int(words) for name, words in WORD_STRUCT.findall(hlsl)}
    found = []
    for line in hlsl.splitlines():
        if "register(" not in line or line.lstrip().startswith("//"):
            continue
        block = CBUFFER.match(line)
        if block:
            found.append(Binding("cbv", f"b{block['register']}", int(block["space"] or 0),
                                 width=cbuffer_bytes(block["body"])))
            continue
        decl = DECLARATION.match(line)
        if not decl:
            raise Unsupported("declaration the runner can't bind: " + re.sub(r"\d+", "N", line.strip())[:80])
        kind, element = decl["type"], decl["element"]
        if decl["array"] == "[]":
            raise Unsupported("unbounded descriptor array")
        count = int(decl["array"][1:-1]) if decl["array"] else 1
        where = {"register": f"{decl['cls']}{decl['register']}", "space": int(decl["space"] or 0), "count": count}
        if kind in TEXTURES or kind in STORAGE:
            dimension = TEXTURES.get(kind) or STORAGE[kind]
            family, fmt = element_format(element)
            width, height, depth = EXTENTS[dimension]
            found.append(Binding("uav_tex" if kind in STORAGE else "srv_tex", dimension=dimension, format=fmt,
                                 width=width, height=height, depth=depth, family=family, **where))
        elif kind in ("Buffer", "RWBuffer"):
            family, fmt = element_format(element)
            found.append(Binding("uav_typed" if kind == "RWBuffer" else "srv_typed", format=fmt,
                                 width=TYPED_ELEMENTS, family=family, **where))
        elif kind in ("ByteAddressBuffer", "RWByteAddressBuffer"):
            found.append(Binding("uav_raw" if kind.startswith("RW") else "srv_raw", width=RAW_BYTES, **where))
        elif kind in ("StructuredBuffer", "RWStructuredBuffer"):
            stride = strides.get(element) or (16 if element is None else None)
            if stride is None:
                family, fmt = element_format(element)
                stride = {2: 16, 3: 16, 4: 16, 16: 8, 17: 8, 18: 8}.get(fmt, 4)
            if stride > 2048:
                raise Unsupported("structure stride over 2048 bytes")
            found.append(Binding("uav_struct" if kind.startswith("RW") else "srv_struct", stride=stride,
                                 width=max(16, RAW_BYTES // stride), **where))
        elif kind in ("SamplerState", "SamplerComparisonState"):
            found.append(Binding("sampler_cmp" if kind == "SamplerComparisonState" else "sampler", **where))
        elif kind == "RaytracingAccelerationStructure":
            raise Unsupported("reads a ray tracing scene")
        else:
            raise Unsupported(f"{kind} resources")
    if not any(binding.writable for binding in found):
        raise Unsupported("writes no resource the runner can read back")
    return found


def root_signature(declared):
    """An HLSL root signature: one table of every view, then one of every sampler."""
    def range_of(binding):
        kind = "Sampler" if binding.sampler else {"c": "CBV", "s": "SRV", "u": "UAV"}[binding.kind[0]]
        return f"{kind}({binding.register}, space={binding.space}, numDescriptors={binding.count})"
    views = [range_of(b) for b in declared if not b.sampler]
    samplers = [range_of(b) for b in declared if b.sampler]
    tables = [f"DescriptorTable({', '.join(group)})" for group in (views, samplers) if group]
    return ", ".join(["RootFlags(0)", *tables])


def with_root_signature(hlsl, declared, define="TB_EXECUTE_ROOT"):
    """The program with a root signature for DXC's `-rootsig-define`."""
    return f'#define {define} "{root_signature(declared)}"\n{hlsl}'


def wave_size(hlsl):
    found = WAVE_SIZE.search(hlsl)
    return int(found[1]) if found else 0


def _stream(key):
    counter = 0
    while True:
        yield from hashlib.sha256(f"{key}\0{counter}".encode()).digest()
        counter += 1


def seeded(key, binding, element):
    """The initial bytes of one element of a binding, the same for both builds.

    Values stay small so a seeded count, index or loop bound can't run away:
    floats are multiples of 1/64 below 4, integers are below 256, and words
    that may be read either way (constant, raw and structured buffers) mix
    integers below 32 with those floats.
    """
    words = binding.bytes() // 4
    stream = _stream(f"{key}\0{binding.line()}\0{binding.register}\0{binding.space}\0{element}")
    out = bytearray()
    for _ in range(words):
        a, b = next(stream), next(stream)
        if binding.family == "float" or (binding.family == "mixed" and a & 1):
            out += struct.pack("<f", b / 64)
        elif binding.family == "mixed":
            out += struct.pack("<I", b & 31)
        else:
            out += struct.pack("<I", b)
    return bytes(out)


def case_files(name, key, declared, grid=GRID, minwave=0):
    """The files the runner reads for one case, as {file name: bytes}."""
    files = {f"{name}.bind": "".join(b.line() + "\n" for b in declared).encode(),
             f"{name}.grid": " ".join(map(str, grid)).encode() + b"\n"}
    if minwave:
        files[f"{name}.minwave"] = f"{minwave}\n".encode()
    for index, binding in enumerate(declared):
        if binding.sampler:
            continue
        for element in range(binding.count):
            files[f"{name}.r{index}.e{element}.seed"] = seeded(key, binding, element)
    return files


def output_digest(read, name, declared):
    """One digest over every writable element the runner read back. `read(file)` returns bytes."""
    digest = hashlib.sha256()
    for index, binding in enumerate(declared):
        if not binding.writable:
            continue
        for element in range(binding.count):
            data = read(f"{name}.u{index}.e{element}.bin")
            digest.update(struct.pack("<II", index, len(data)) + data)
    return digest.hexdigest()


def wrote(read, seed, name, declared):
    """Whether the program changed any writable element from its seeded contents."""
    for index, binding in enumerate(declared):
        if not binding.writable:
            continue
        for element in range(binding.count):
            if read(f"{name}.u{index}.e{element}.bin") != seed(f"{name}.r{index}.e{element}.seed"):
                return True
    return False


def receipt(text, names):
    """Each case's outcome from the runner's result.txt, and the cases it never reached.

    A case that began and has no outcome stopped the runner: it is counted as a
    hang. Cases after it are returned to be run again.
    """
    fields, began = {}, []
    for line in text.splitlines():
        key, _, value = line.partition("=")
        if key == "begin":
            began.append(value)
        else:
            fields[key] = value
    outcomes, pending = {}, []
    stopped = False
    for name in names:
        if f"{name}_ok" in fields:
            outcomes[name] = ("ran", "")
        elif f"{name}_error" in fields:
            outcomes[name] = ("failed", fields[f"{name}_error"])
        elif f"{name}_skipped" in fields:
            outcomes[name] = ("skipped", fields[f"{name}_skipped"])
        elif f"{name}_hang" in fields or (name in began and not stopped):
            outcomes[name] = ("hang", "GPU work did not finish")
            stopped = True
        else:
            pending.append(name)
    return outcomes, pending, fields


def compare(before, after, repeat=None):
    """Sort two runs over the same requests. Each maps request -> outcome dict.

    `repeat`, if given, is the baseline translation run a second time: a
    request whose baseline disagrees with itself, or doesn't finish the second
    time, is unstable whatever the candidate did, so a race or a loaded host
    can't pass for a changed result or a lost program.

    An outcome has `status` (ran, skipped, failed, hang, refused), optional
    `reason`, `digest` when it ran and `hlsl` (the translation's digest) when
    it translated.
    """
    kinds = {name: [] for name in ("same", "equivalent", "changed", "unstable", "lost", "gained", "neither")}
    for request in sorted(set(before) | set(after)):
        old, new = before.get(request, {"status": "missing"}), after.get(request, {"status": "missing"})
        ran_old, ran_new = old["status"] == "ran", new["status"] == "ran"
        again = (repeat or {}).get(request)
        if ran_old and again is not None and (again["status"] != "ran" or again["digest"] != old["digest"]):
            # The baseline didn't repeat itself: other results or no result.
            kinds["unstable"].append(request)
        elif ran_old and ran_new:
            if old["digest"] == new["digest"]:
                # A translation that changed and wrote the same is equivalent for these inputs.
                kinds["same" if old.get("hlsl") == new.get("hlsl") else "equivalent"].append(request)
            elif old.get("hlsl") == new.get("hlsl"):
                # The same program over the same inputs wrote something else.
                kinds["unstable"].append(request)
            else:
                kinds["changed"].append(request)
        elif ran_old:
            kinds["lost"].append(request)
        elif ran_new:
            kinds["gained"].append(request)
        else:
            kinds["neither"].append(request)
    return kinds


def blocking(kinds):
    """What stops a change: results that moved, and programs that no longer run."""
    problems = []
    if kinds["changed"]:
        problems.append(f"{len(kinds['changed'])} recorded shaders compute different results, "
                        f"e.g. {kinds['changed'][0]}")
    if kinds["lost"]:
        problems.append(f"{len(kinds['lost'])} recorded shaders ran before and no longer run, e.g. {kinds['lost'][0]}")
    return problems


def reasons(outcomes):
    """Counts of why requests did not run, normalised so similar reasons group."""
    counts = {}
    for outcome in outcomes.values():
        if outcome["status"] != "ran":
            reason = f"{outcome['status']}: " + re.sub(r"0x[0-9a-f]+|\d+", "N", outcome.get("reason", ""))[:120]
            counts[reason] = counts.get(reason, 0) + 1
    return sorted(counts.items(), key=lambda item: (-item[1], item[0]))
