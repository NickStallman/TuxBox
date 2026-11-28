#!/usr/bin/env python3
"""Read back what translated image instructions return from textures of known contents.

Generate the input HLSL with:
  cargo test --manifest-path tools/shader-compiler/Cargo.toml --target x86_64-unknown-linux-gnu image_ops_for_the_gpu_check -- --nocapture
Save the text between BEGIN_HLSL and END_HLSL as the --hlsl file. It holds one
program after each `//// name kind registers | expected` line.

Each program loads an image and a sampler descriptor and samples, gathers,
loads or queries the image, or updates a texel atomically and loads it again.
Every texel of every level holds a different value and the sampler takes the
nearest texel, so a wrong texel, level, channel or operand order shows. Only
the output is instrumented. No game data is needed.
"""
from pathlib import Path
import argparse
import sys, subprocess, struct, json, hashlib
from tempfile import TemporaryDirectory

p = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(p))
from build import find_dxc
from frosty_gpu_guest import run_gpu_guest

WORDS = 384
FIRST_WORD = 96  # the output view starts 384 bytes into the buffer


def bits(value):
    return struct.unpack("<I", struct.pack("<f", value))[0]


def texel(token):
    """`x,y,m` is the red channel of a texel, which holds 100 m + 10 y + x; `g:`,
    `b:` and `a:` name the channels the guest derives from it."""
    channel, _, where = token.rpartition(":")
    x, y, level = (int(part) for part in where.split(","))
    red = 100.0 * level + 10.0 * y + x
    return bits({"": red, "g": red + 0.5, "b": -red, "a": 1000.0 + red}[channel])


parser = argparse.ArgumentParser(description=__doc__)
for name in ("hlsl", "provider", "proton", "prefix", "compiler-dir", "work", "output"):
    parser.add_argument("--" + name, type=Path, required=True)
parser.add_argument("--adapter", required=True)
args = parser.parse_args()
w = args.work.resolve()
w.mkdir(parents=True, exist_ok=True)
dxc = Path(find_dxc(None)).resolve()


def replace(text, old, new):
    assert text.count(old) == 1, old
    return text.replace(old, new)


programs = {}
for section in args.hlsl.read_text().split("//// ")[1:]:
    header, source = section.split("\n", 1)
    left, right = header.split(" | ")
    name, kind, *registers = left.split()
    wanted = [texel(token) if "," in token else int(token, 16) for token in right.split()]
    assert len(wanted) == len(registers), name
    threads = "[numthreads(1, 1, 1)]" if kind == "uint" else "[numthreads(32, 1, 1)]"
    source = replace(source, threads, "RWByteAddressBuffer output : register(u0);\n" + threads)
    stores = "".join(f"    output.Store({4 * j}u, {register});\n" for j, register in enumerate(registers))
    body, closing = source.rstrip().rsplit("}", 1)
    assert not closing.strip(), name
    programs[name] = (kind, wanted, body + stores + "}\n")
assert programs, "no programs in the --hlsl file"

guest = (p / "tools/reference/independent-shaders/compute.cpp").read_text()
guest = replace(guest, "D3D12_DESCRIPTOR_RANGE range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0};", """std::string image_kind;
    if (!(std::ifstream(name + ".kind") >> image_kind))
        throw std::runtime_error("missing image kind for " + name);
    // `uint` is a writable R32_UINT image; `float` and `compare` are read through a sampler.
    const bool writable = image_kind == "uint", compared = image_kind == "compare";
    if (!writable && !compared && image_kind != "float")
        throw std::runtime_error("invalid image kind " + image_kind);
    D3D12_DESCRIPTOR_RANGE range[]{{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0},
        {writable ? D3D12_DESCRIPTOR_RANGE_TYPE_UAV : D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 1, 1, 0, 1}};
    D3D12_DESCRIPTOR_RANGE sampler_range{D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER, 1, 1, 0, 0};""")
guest = replace(guest, "table.DescriptorTable = {1, &range};", "table.DescriptorTable = {2, range};")
guest = replace(guest, "    D3D12_ROOT_SIGNATURE_DESC description{1, &table, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};",
                """    D3D12_ROOT_PARAMETER parameters[2]{table, {}};
    parameters[1].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    parameters[1].DescriptorTable = {1, &sampler_range};
    D3D12_ROOT_SIGNATURE_DESC description{2, parameters, 0, nullptr, D3D12_ROOT_SIGNATURE_FLAG_NONE};""")
guest = replace(guest, "D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1,", "D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 2,")
guest = replace(guest, "    D3D12_COMMAND_QUEUE_DESC qd{};", """    // An 8 by 8 image. The sampled ones have a second level of 4 by 4.
    const UINT16 levels = writable ? 1 : 2;
    const UINT channels = (writable || compared) ? 1 : 4;
    D3D12_RESOURCE_DESC image_desc{};
    image_desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    image_desc.Width = image_desc.Height = 8;
    image_desc.DepthOrArraySize = 1;
    image_desc.MipLevels = levels;
    image_desc.Format = writable ? DXGI_FORMAT_R32_UINT : compared ? DXGI_FORMAT_R32_FLOAT : DXGI_FORMAT_R32G32B32A32_FLOAT;
    image_desc.SampleDesc = {1, 0};
    image_desc.Flags = writable ? D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS : D3D12_RESOURCE_FLAG_NONE;
    D3D12_HEAP_PROPERTIES image_heap{};
    image_heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    ComPtr<ID3D12Resource> image;
    check(device->CreateCommittedResource(&image_heap, D3D12_HEAP_FLAG_NONE, &image_desc, D3D12_RESOURCE_STATE_COPY_DEST,
                                          nullptr, IID_PPV_ARGS(&image)), "create image");
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT footprints[2]{};
    UINT64 image_bytes = 0;
    device->GetCopyableFootprints(&image_desc, 0, levels, 0, footprints, nullptr, nullptr, &image_bytes);
    auto image_seed = buffer(device, static_cast<UINT>(image_bytes), D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    {
        BYTE *mapped{};
        check(image_seed->Map(0, nullptr, reinterpret_cast<void **>(&mapped)), "map image");
        for (UINT level = 0; level < levels; ++level)
            for (UINT y = 0; y < footprints[level].Footprint.Height; ++y)
                for (UINT x = 0; x < footprints[level].Footprint.Width; ++x) {
                    BYTE *at = mapped + footprints[level].Offset + y * footprints[level].Footprint.RowPitch + x * channels * 4;
                    if (writable) {
                        const UINT word = 0x1000u + 16u * y + x;
                        std::memcpy(at, &word, 4);
                    } else {
                        // Red is 100 level + 10 y + x; the other channels follow from it.
                        const float red = 100.0f * level + 10.0f * y + x;
                        const float texel[4]{red, red + 0.5f, -red, 1000.0f + red};
                        std::memcpy(at, texel, channels * 4);
                    }
                }
        image_seed->Unmap(0, nullptr);
    }
    auto image_handle = heap->GetCPUDescriptorHandleForHeapStart();
    image_handle.ptr += device->GetDescriptorHandleIncrementSize(hd.Type);
    if (writable)
        device->CreateUnorderedAccessView(image.Get(), nullptr, nullptr, image_handle);
    else
        device->CreateShaderResourceView(image.Get(), nullptr, image_handle);
    // The nearest texel, so every result is one stored value.
    D3D12_DESCRIPTOR_HEAP_DESC sampler_hd{D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, 1, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    ComPtr<ID3D12DescriptorHeap> sampler_heap;
    check(device->CreateDescriptorHeap(&sampler_hd, IID_PPV_ARGS(&sampler_heap)), "create sampler heap");
    D3D12_SAMPLER_DESC sampler{};
    sampler.Filter = compared ? D3D12_FILTER_COMPARISON_MIN_MAG_MIP_POINT : D3D12_FILTER_MIN_MAG_MIP_POINT;
    sampler.AddressU = sampler.AddressV = sampler.AddressW = D3D12_TEXTURE_ADDRESS_MODE_CLAMP;
    sampler.ComparisonFunc = compared ? D3D12_COMPARISON_FUNC_LESS_EQUAL : D3D12_COMPARISON_FUNC_NEVER;
    sampler.MaxLOD = D3D12_FLOAT32_MAX;
    device->CreateSampler(&sampler, sampler_heap->GetCPUDescriptorHandleForHeapStart());
    D3D12_COMMAND_QUEUE_DESC qd{};""")
guest = replace(guest, "    transition(list.Get(), out.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);",
                """    transition(list.Get(), out.Get(), D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    for (UINT level = 0; level < levels; ++level) {
        D3D12_TEXTURE_COPY_LOCATION from{}, to{};
        from.pResource = image_seed.Get();
        from.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
        from.PlacedFootprint = footprints[level];
        to.pResource = image.Get();
        to.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
        to.SubresourceIndex = level;
        list->CopyTextureRegion(&to, 0, 0, 0, &from, nullptr);
    }
    transition(list.Get(), image.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
               writable ? D3D12_RESOURCE_STATE_UNORDERED_ACCESS : D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);""")
guest = replace(guest, "    ID3D12DescriptorHeap *heaps[] = {heap.Get()};\n    list->SetDescriptorHeaps(1, heaps);",
                "    ID3D12DescriptorHeap *heaps[] = {heap.Get(), sampler_heap.Get()};\n    list->SetDescriptorHeaps(2, heaps);")
guest = replace(guest, "    list->Dispatch(groups, 1, 1);",
                "    list->SetComputeRootDescriptorTable(1, sampler_heap->GetGPUDescriptorHandleForHeapStart());\n"
                "    list->Dispatch(groups, 1, 1);")


def digest(data):
    return hashlib.sha256(data).hexdigest()


proton = args.proton.resolve()
with TemporaryDirectory(dir=w) as td:
    trial = Path(td)
    (trial / "guest.cpp").write_text(guest)
    (trial / "context.h").write_text((p / "tools/reference/independent-shaders/context.h").read_text()
                                     .replace('"../compute_buffers.h"', '"' + str(p / "tools/reference/compute_buffers.h") + '"'))
    expected, shaders = {}, {}
    for name, (kind, wanted, source) in programs.items():
        (trial / f"{name}.hlsl").write_text(source)
        blob = trial / f"{name}.cso"
        # A level taken from neighbouring threads needs compute derivatives, which arrived in 6.6.
        target = "cs_6_6" if name.startswith("implicit-") else "cs_6_0"
        subprocess.run([str(dxc), "-T", target, "-E", "main", "-Fo", str(blob), str(trial / f"{name}.hlsl")], check=True, timeout=30)
        subprocess.run([str(dxc.with_name("dxv")), str(blob)], check=True, timeout=30)
        shaders[name] = digest(blob.read_bytes())
        (trial / f"{name}.kind").write_text(kind + "\n")
        want = [0x76540000 + i for i in range(WORDS)]
        want[FIRST_WORD:FIRST_WORD + len(wanted)] = wanted
        expected[name] = want
    (trial / "cases.txt").write_text("".join(f"{name} 0 raw 1\n" for name in expected))
    exe, status, fields = run_gpu_guest(trial / "guest.cpp", "image-ops.exe", trial, w, proton,
                                        args.prefix.resolve(), args.provider.resolve(), args.compiler_dir.resolve())
    print("guest", status, fields)
    assert status == 0 and fields.get("adapter") == args.adapter
    rows, wrong = [], []
    for name, want in expected.items():
        actual = (trial / f"{name}.bin").read_bytes()
        got = struct.unpack(f"<{WORDS}I", actual)
        differ = [(i - FIRST_WORD, f"{a:#x}", f"{b:#x}") for i, (a, b) in enumerate(zip(got, want)) if a != b]
        if differ:
            wrong.append((name, differ[:6]))
        rows.append({"case": name, "kind": programs[name][0], "words": len(programs[name][1]), "matched": not differ,
                     "sha256": digest(actual)})
    for name, differ in wrong:
        print(f"WRONG {name}: (word, got, wanted) {differ}")
    assert not wrong, f"{len(wrong)} of {len(rows)} cases returned the wrong words"
    receipt = {"scope": "Synthetic translated image samples, gathers, loads, size queries and atomics on images of "
                        "known contents with a nearest-texel sampler; instrumented output only",
               "adapter": fields["adapter"], "proton": (proton / "version").read_text().strip(),
               "shader_sha256": shaders, "cases": rows}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"PASS: {len(rows)} cases, {sum(row['words'] for row in rows)} words, 0 mismatches")
