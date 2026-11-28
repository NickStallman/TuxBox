#!/usr/bin/env python3
"""Read back texture sizes that a translated program takes from descriptor words.

Generate the input HLSL with:
  cargo test --manifest-path tools/shader-compiler/Cargo.toml --target x86_64-unknown-linux-gnu texture_size_from_descriptor_fields_for_the_gpu_check -- --nocapture
Save the text between BEGIN_HLSL and END_HLSL as the --hlsl file.

The program loads an image descriptor, extracts its width, height and base
level fields and computes (field + 1) >> level. Each case binds a texture of
known size through a view that starts at a known mip, and the two results must
be that mip's size. A null view stands for an empty console descriptor, whose
zero fields give one texel each way. Only the output is instrumented. No game data is needed.
"""
from pathlib import Path
import argparse
import sys, subprocess, struct, json, hashlib
from tempfile import TemporaryDirectory

p = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(p))
from build import find_dxc
from frosty_gpu_guest import run_gpu_guest

# Width, height, mip levels of the texture and the view's first mip. The widths
# cover both halves of the field (two bits in word 1, the rest in word 2). A
# width of zero binds a null view.
CASES = [(256, 128, 1, 0), (640, 360, 4, 0), (640, 360, 4, 2), (4096, 2048, 6, 3), (1, 1, 1, 0),
         (16384, 8, 1, 0), (8, 16384, 1, 0), (1023, 513, 3, 1), (6, 7, 1, 0), (0, 0, 1, 0)]
WORDS = 384
FIRST_WORD = 96  # the output view starts 384 bytes into the buffer

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


src = args.hlsl.read_text()
assert "tex1.GetDimensions(0u, " in src, "the translation does not fill the size fields"
src = replace(src, "Texture2D<float4> tex1 : register(t1);",
              "Texture2D<float4> tex1 : register(t1);\nRWByteAddressBuffer output : register(u0);")
# s3 and s6 hold the width and the height once both shifts have run.
src = replace(src, "    // 0x003c v_mov_b32",
              "    output.Store(0u, s3);\n    output.Store(4u, s6);\n    // 0x003c v_mov_b32")
guest = (p / "tools/reference/independent-shaders/compute.cpp").read_text()
guest = replace(guest, "D3D12_DESCRIPTOR_RANGE range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0};",
                "D3D12_DESCRIPTOR_RANGE range[]{{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0}, "
                "{D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 1, 1, 0, 1}};")
guest = replace(guest, "table.DescriptorTable = {1, &range};", "table.DescriptorTable = {2, range};")
guest = replace(guest, "D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1,", "D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 2,")
guest = replace(guest, "    D3D12_COMMAND_QUEUE_DESC qd{};", """    UINT texture_width = 0, texture_height = 0, texture_mips = 0, first_mip = 0;
    if (!(std::ifstream(name + ".tex") >> texture_width >> texture_height >> texture_mips >> first_mip))
        throw std::runtime_error("missing texture case " + name);
    D3D12_HEAP_PROPERTIES texture_heap{};
    texture_heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    D3D12_RESOURCE_DESC texture_desc{};
    texture_desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    texture_desc.Width = texture_width;
    texture_desc.Height = texture_height;
    texture_desc.DepthOrArraySize = 1;
    texture_desc.MipLevels = static_cast<UINT16>(texture_mips);
    texture_desc.Format = DXGI_FORMAT_R8_UNORM;
    texture_desc.SampleDesc = {1, 0};
    ComPtr<ID3D12Resource> texture;
    if (texture_width) {
        check(device->CreateCommittedResource(&texture_heap, D3D12_HEAP_FLAG_NONE, &texture_desc,
                                              D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, nullptr,
                                              IID_PPV_ARGS(&texture)), "create sized texture");
    }
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.Format = DXGI_FORMAT_R8_UNORM;
    srv.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    srv.Texture2D.MostDetailedMip = first_mip;
    srv.Texture2D.MipLevels = texture_mips - first_mip;
    auto srv_handle = heap->GetCPUDescriptorHandleForHeapStart();
    srv_handle.ptr += device->GetDescriptorHandleIncrementSize(hd.Type);
    device->CreateShaderResourceView(texture.Get(), &srv, srv_handle);
    D3D12_COMMAND_QUEUE_DESC qd{};""")


def digest(data):
    return hashlib.sha256(data).hexdigest()


proton = args.proton.resolve()
with TemporaryDirectory(dir=w) as td:
    trial = Path(td)
    (trial / "control.hlsl").write_text(src)
    (trial / "guest.cpp").write_text(guest)
    (trial / "context.h").write_text((p / "tools/reference/independent-shaders/context.h").read_text()
                                     .replace('"../compute_buffers.h"', '"' + str(p / "tools/reference/compute_buffers.h") + '"'))
    blob = trial / "control.cso"
    subprocess.run([str(dxc), "-T", "cs_6_0", "-E", "main", "-Fo", str(blob), str(trial / "control.hlsl")], check=True, timeout=30)
    subprocess.run([str(dxc.with_name("dxv")), str(blob)], check=True, timeout=30)
    expected = {}
    for case, (width, height, mips, first) in enumerate(CASES):
        name = f"case{case:02}"
        want = [0x76540000 + i for i in range(WORDS)]
        # The size of the view's first mip, as D3D12 defines it.
        want[FIRST_WORD:FIRST_WORD + 2] = [max(1, width >> first), max(1, height >> first)]
        (trial / f"{name}.tex").write_text(f"{width} {height} {mips} {first}\n")
        (trial / f"{name}.cso").hardlink_to(blob)
        expected[name] = struct.pack(f"<{WORDS}I", *want)
    (trial / "cases.txt").write_text("".join(f"{name} 0 raw 1\n" for name in expected))
    exe, status, fields = run_gpu_guest(trial / "guest.cpp", "descriptor-size.exe", trial, w, proton,
                                        args.prefix.resolve(), args.provider.resolve(), args.compiler_dir.resolve())
    print("guest", status, fields)
    assert status == 0 and fields.get("adapter") == args.adapter
    rows = []
    for (name, want), case in zip(expected.items(), CASES):
        actual = (trial / f"{name}.bin").read_bytes()
        got = struct.unpack(f"<{WORDS}I", actual)
        assert actual == want, (name, case, [(i, a, b) for i, (a, b) in enumerate(zip(got, struct.unpack(f"<{WORDS}I", want))) if a != b][:10])
        rows.append({"case": name, "texture": list(case), "size": list(got[FIRST_WORD:FIRST_WORD + 2]),
                     "matched": True, "sha256": digest(actual)})
    receipt = {"scope": "Synthetic translated read of an image descriptor's width, height and base level; "
                        "instrumented output only",
               "adapter": fields["adapter"], "proton": (proton / "version").read_text().strip(),
               "shader_sha256": digest(blob.read_bytes()), "cases": rows}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"PASS: {len(rows)} cases, {len(rows) * WORDS} words, 0 mismatches")
