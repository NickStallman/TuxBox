#!/usr/bin/env python3
"""Read back buffer record counts that translated programs take from descriptor words.

Generate the input HLSL with:
  cargo test --manifest-path tools/shader-compiler/Cargo.toml --target x86_64-unknown-linux-gnu buffer_record_counts_for_the_gpu_check -- --nocapture
Save the text between BEGIN_HLSL and END_HLSL as the --hlsl file. It holds three
programs, each after a `//// kind` line.

Each program loads a buffer descriptor and reads word 2, the record count. Each
case binds a raw, structured or typed buffer view of known size, and the word
must be what GetDimensions means for that kind: bytes for a raw buffer, records
or elements otherwise. Only the output is instrumented. No game data is needed.
"""
from pathlib import Path
import argparse
import sys, subprocess, struct, json, hashlib
from tempfile import TemporaryDirectory

p = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(p))
from build import find_dxc
from frosty_gpu_guest import run_gpu_guest

# Bytes per record and the record counts of each kind's views. A raw view counts
# bytes in whole words; the structured program's records are 12 bytes; the typed
# view is R8_UNORM. The last of each is the largest buffer the check allocates.
KINDS = {
    "raw": (1, [4, 12, 4004, 65536, (1 << 28) - 4]),
    "structured": (12, [1, 3, 1001, 65537, (1 << 24) + 1]),
    "typed": (1, [1, 7, 1001, 65537, (1 << 28) - 1]),
}
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


sections = args.hlsl.read_text().split("//// ")[1:]
programs = {}
for section in sections:
    kind, source = section.split("\n", 1)
    assert "s10 = n0;" in source, f"the {kind} translation does not fill the record count"
    source = replace(source, "[numthreads(32, 1, 1)]", "RWByteAddressBuffer output : register(u0);\n[numthreads(32, 1, 1)]")
    # s10 is word 2 of the descriptor, read by the add that follows.
    programs[kind] = replace(source, "    // 0x0008 s_add_i32", "    output.Store(0u, s10);\n    // 0x0008 s_add_i32")
assert set(programs) == set(KINDS), sorted(programs)
guest = (p / "tools/reference/independent-shaders/compute.cpp").read_text()
guest = replace(guest, "D3D12_DESCRIPTOR_RANGE range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0};",
                "D3D12_DESCRIPTOR_RANGE range[]{{D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 0}, "
                "{D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 1, 1, 0, 1}};")
guest = replace(guest, "table.DescriptorTable = {1, &range};", "table.DescriptorTable = {2, range};")
guest = replace(guest, "D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1,", "D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 2,")
guest = replace(guest, "    D3D12_COMMAND_QUEUE_DESC qd{};", """    std::string counted_kind;
    UINT record_bytes = 0, record_count = 0;
    if (!(std::ifstream(name + ".buf") >> counted_kind >> record_bytes >> record_count))
        throw std::runtime_error("missing buffer case " + name);
    // Only the size is read, so the contents stay whatever the heap holds.
    D3D12_HEAP_PROPERTIES counted_heap{};
    counted_heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    D3D12_RESOURCE_DESC counted_desc{};
    counted_desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    counted_desc.Width = UINT64(record_bytes) * record_count;
    counted_desc.Height = 1;
    counted_desc.DepthOrArraySize = 1;
    counted_desc.MipLevels = 1;
    counted_desc.SampleDesc = {1, 0};
    counted_desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    ComPtr<ID3D12Resource> counted;
    check(device->CreateCommittedResource(&counted_heap, D3D12_HEAP_FLAG_NONE, &counted_desc,
                                          D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, nullptr,
                                          IID_PPV_ARGS(&counted)), "create counted buffer");
    D3D12_SHADER_RESOURCE_VIEW_DESC srv{};
    srv.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
    srv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    if (counted_kind == "raw") {
        srv.Format = DXGI_FORMAT_R32_TYPELESS;
        srv.Buffer.NumElements = record_count / 4;
        srv.Buffer.Flags = D3D12_BUFFER_SRV_FLAG_RAW;
    } else if (counted_kind == "structured") {
        srv.Format = DXGI_FORMAT_UNKNOWN;
        srv.Buffer.NumElements = record_count;
        srv.Buffer.StructureByteStride = record_bytes;
    } else if (counted_kind == "typed") {
        srv.Format = DXGI_FORMAT_R8_UNORM;
        srv.Buffer.NumElements = record_count;
    } else {
        throw std::runtime_error("invalid buffer kind " + counted_kind);
    }
    auto srv_handle = heap->GetCPUDescriptorHandleForHeapStart();
    srv_handle.ptr += device->GetDescriptorHandleIncrementSize(hd.Type);
    device->CreateShaderResourceView(counted.Get(), &srv, srv_handle);
    D3D12_COMMAND_QUEUE_DESC qd{};""")


def digest(data):
    return hashlib.sha256(data).hexdigest()


proton = args.proton.resolve()
with TemporaryDirectory(dir=w) as td:
    trial = Path(td)
    (trial / "guest.cpp").write_text(guest)
    (trial / "context.h").write_text((p / "tools/reference/independent-shaders/context.h").read_text()
                                     .replace('"../compute_buffers.h"', '"' + str(p / "tools/reference/compute_buffers.h") + '"'))
    blobs = {}
    for kind, source in programs.items():
        (trial / f"{kind}.hlsl").write_text(source)
        blobs[kind] = trial / f"{kind}.cso"
        subprocess.run([str(dxc), "-T", "cs_6_0", "-E", "main", "-Fo", str(blobs[kind]), str(trial / f"{kind}.hlsl")], check=True, timeout=30)
        subprocess.run([str(dxc.with_name("dxv")), str(blobs[kind])], check=True, timeout=30)
    expected = {}
    for kind, (record_bytes, counts) in KINDS.items():
        for case, count in enumerate(counts):
            name = f"{kind}{case:02}"
            want = [0x76540000 + i for i in range(WORDS)]
            want[FIRST_WORD] = count
            (trial / f"{name}.buf").write_text(f"{kind} {record_bytes} {count}\n")
            (trial / f"{name}.cso").hardlink_to(blobs[kind])
            expected[name] = (kind, count, struct.pack(f"<{WORDS}I", *want))
    (trial / "cases.txt").write_text("".join(f"{name} 0 raw 1\n" for name in expected))
    exe, status, fields = run_gpu_guest(trial / "guest.cpp", "buffer-count.exe", trial, w, proton,
                                        args.prefix.resolve(), args.provider.resolve(), args.compiler_dir.resolve())
    print("guest", status, fields)
    assert status == 0 and fields.get("adapter") == args.adapter
    rows = []
    for name, (kind, count, want) in expected.items():
        actual = (trial / f"{name}.bin").read_bytes()
        got = struct.unpack(f"<{WORDS}I", actual)
        assert actual == want, (name, kind, count, [(i, a, b) for i, (a, b) in enumerate(zip(got, struct.unpack(f"<{WORDS}I", want))) if a != b][:10])
        rows.append({"case": name, "kind": kind, "count": got[FIRST_WORD], "matched": True, "sha256": digest(actual)})
    receipt = {"scope": "Synthetic translated reads of a buffer descriptor's record count for raw, structured "
                        "and typed views; instrumented output only",
               "adapter": fields["adapter"], "proton": (proton / "version").read_text().strip(),
               "shader_sha256": {kind: digest(blob.read_bytes()) for kind, blob in blobs.items()}, "cases": rows}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"PASS: {len(rows)} cases, {len(rows) * WORDS} words, 0 mismatches")
