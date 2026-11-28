// Run translated compute shaders over generated resources and save what they write.
//
// `cases.txt` names one case per line. Each case has:
//   NAME.cso    DXIL with its root signature embedded (one CBV/SRV/UAV table, then one sampler table)
//   NAME.bind   one resource per line: kind dimension format count width height depth stride
//   NAME.grid   the thread groups to dispatch, as three numbers
//   NAME.minwave  optional: the wave width the program needs
//   NAME.rR.eE.seed  the initial contents of element E of resource R, laid out tightly
// Every writable resource is read back to NAME.uR.eE.bin, tightly laid out.
// `result.txt` records `begin=NAME` before each case and `NAME_ok`, `NAME_error`,
// `NAME_skipped` or `NAME_hang` after it, so a run cut short shows where it stopped.
#include "../compute_buffers.h"
#include <atomic>
#include <chrono>
#include <fstream>
#include <iterator>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

using namespace compute_buffers;

namespace {
// A pipeline build or dispatch that never returns can leave driver threads
// running after the main thread exits, and the process then never ends. The
// watchdog records the case as a hang and ends the whole process instead.
constexpr auto case_limit = std::chrono::seconds(90);
std::atomic<long long> deadline{0};
std::atomic<FILE *> watched{nullptr};
std::string current;

void watchdog() {
    for (;;) {
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        const long long due = deadline.load();
        const long long now = std::chrono::steady_clock::now().time_since_epoch().count();
        if (due && now > due) {
            if (FILE *receipt = watched.load()) {
                std::fprintf(receipt, "%s_hang=1\n", current.c_str());
                std::fflush(receipt);
            }
            TerminateProcess(GetCurrentProcess(), 3);
        }
    }
}

struct Binding {
    std::string kind, dimension;
    UINT format{}, count{}, width{}, height{}, depth{}, stride{};
    bool writable() const { return kind.rfind("uav_", 0) == 0; }
    bool sampler() const { return kind.rfind("sampler", 0) == 0; }
    bool texture() const { return kind == "srv_tex" || kind == "uav_tex"; }
    UINT element_bytes() const {
        switch (format) {
        case DXGI_FORMAT_R32G32B32A32_FLOAT: case DXGI_FORMAT_R32G32B32A32_UINT: case DXGI_FORMAT_R32G32B32A32_SINT: return 16;
        case DXGI_FORMAT_R32G32_FLOAT: case DXGI_FORMAT_R32G32_UINT: case DXGI_FORMAT_R32G32_SINT: return 8;
        default: return 4;
        }
    }
    UINT64 bytes() const {
        if (texture()) return UINT64(width) * height * depth * element_bytes();
        if (kind == "srv_typed" || kind == "uav_typed") return UINT64(width) * element_bytes();
        if (kind == "srv_struct" || kind == "uav_struct") return UINT64(width) * stride;
        return width;  // constant and raw buffers give their size in bytes
    }
};

std::vector<char> read_file(const std::string &path) {
    std::ifstream input(path, std::ios::binary);
    std::vector<char> data{std::istreambuf_iterator<char>{input}, {}};
    if (!input.good() && !input.eof()) throw std::runtime_error("read " + path);
    return data;
}

std::vector<Binding> read_bindings(const std::string &name) {
    std::ifstream input(name + ".bind");
    if (!input) throw std::runtime_error("missing bindings for " + name);
    std::vector<Binding> out;
    std::string line;
    while (std::getline(input, line)) {
        if (line.empty()) continue;
        std::istringstream fields(line);
        Binding b;
        if (!(fields >> b.kind >> b.dimension >> b.format >> b.count >> b.width >> b.height >> b.depth >> b.stride))
            throw std::runtime_error("invalid binding line for " + name);
        out.push_back(b);
    }
    return out;
}

D3D12_RESOURCE_DESC texture_desc(const Binding &b) {
    D3D12_RESOURCE_DESC d{};
    d.Dimension = b.dimension == "3d" ? D3D12_RESOURCE_DIMENSION_TEXTURE3D
                : (b.dimension == "1d" || b.dimension == "1darray") ? D3D12_RESOURCE_DIMENSION_TEXTURE1D
                : D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    d.Width = b.width;
    d.Height = b.height;
    d.DepthOrArraySize = static_cast<UINT16>(b.depth);
    d.MipLevels = 1;
    d.Format = static_cast<DXGI_FORMAT>(b.format);
    d.SampleDesc.Count = 1;
    d.Flags = b.writable() ? D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS : D3D12_RESOURCE_FLAG_NONE;
    return d;
}

struct Element {
    ComPtr<ID3D12Resource> resource, staging, readback;
    std::vector<D3D12_PLACED_SUBRESOURCE_FOOTPRINT> layouts;
    std::vector<UINT> rows;
    std::vector<UINT64> row_bytes;
    UINT64 total{};
};

// Footprints of every subresource: array slices, cube faces, or the one 3D/1D/2D image.
void footprints(ID3D12Device *device, const D3D12_RESOURCE_DESC &desc, Element &e) {
    const UINT count = desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D ? 1 : desc.DepthOrArraySize;
    e.layouts.resize(count);
    e.rows.resize(count);
    e.row_bytes.resize(count);
    device->GetCopyableFootprints(&desc, 0, count, 0, e.layouts.data(), e.rows.data(), e.row_bytes.data(), &e.total);
}

// Copy between a tight byte image and a footprinted staging buffer.
void pack(const Element &e, std::vector<char> &tight, char *staged, bool to_staging) {
    size_t offset = 0;
    for (size_t s = 0; s < e.layouts.size(); ++s) {
        const auto &f = e.layouts[s];
        for (UINT z = 0; z < f.Footprint.Depth; ++z)
            for (UINT y = 0; y < e.rows[s]; ++y) {
                char *line = staged + f.Offset + (UINT64(z) * e.rows[s] + y) * f.Footprint.RowPitch;
                if (to_staging) std::memcpy(line, tight.data() + offset, e.row_bytes[s]);
                else std::memcpy(tight.data() + offset, line, e.row_bytes[s]);
                offset += e.row_bytes[s];
            }
    }
}

void copy_texture(ID3D12GraphicsCommandList *list, const Element &e, bool upload) {
    for (UINT s = 0; s < e.layouts.size(); ++s) {
        D3D12_TEXTURE_COPY_LOCATION image{}, linear{};
        image.pResource = e.resource.Get();
        image.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
        image.SubresourceIndex = s;
        linear.pResource = upload ? e.staging.Get() : e.readback.Get();
        linear.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
        linear.PlacedFootprint = e.layouts[s];
        if (upload) list->CopyTextureRegion(&image, 0, 0, 0, &linear, nullptr);
        else list->CopyTextureRegion(&linear, 0, 0, 0, &image, nullptr);
    }
}

void create_view(ID3D12Device *device, const Binding &b, ID3D12Resource *resource, D3D12_CPU_DESCRIPTOR_HANDLE at) {
    const auto format = static_cast<DXGI_FORMAT>(b.format);
    if (b.kind == "cbv") {
        D3D12_CONSTANT_BUFFER_VIEW_DESC v{resource->GetGPUVirtualAddress(), b.width};
        device->CreateConstantBufferView(&v, at);
        return;
    }
    if (b.writable()) {
        D3D12_UNORDERED_ACCESS_VIEW_DESC v{};
        v.Format = format;
        if (b.kind == "uav_tex") {
            if (b.dimension == "1d") { v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D; }
            else if (b.dimension == "1darray") { v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY; v.Texture1DArray.ArraySize = b.depth; }
            else if (b.dimension == "2d") { v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2D; }
            else if (b.dimension == "2darray") { v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY; v.Texture2DArray.ArraySize = b.depth; }
            else { v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D; v.Texture3D.WSize = b.depth; }
        } else {
            v.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
            if (b.kind == "uav_raw") { v.Format = DXGI_FORMAT_R32_TYPELESS; v.Buffer.NumElements = b.width / 4; v.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW; }
            else if (b.kind == "uav_struct") { v.Format = DXGI_FORMAT_UNKNOWN; v.Buffer.NumElements = b.width; v.Buffer.StructureByteStride = b.stride; }
            else { v.Buffer.NumElements = b.width; }
        }
        device->CreateUnorderedAccessView(resource, nullptr, &v, at);
        return;
    }
    D3D12_SHADER_RESOURCE_VIEW_DESC v{};
    v.Format = format;
    v.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    if (b.kind == "srv_tex") {
        if (b.dimension == "1d") { v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE1D; v.Texture1D.MipLevels = 1; }
        else if (b.dimension == "1darray") { v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE1DARRAY; v.Texture1DArray = {0, 1, 0, b.depth, 0}; }
        else if (b.dimension == "2d") { v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D; v.Texture2D.MipLevels = 1; }
        else if (b.dimension == "2darray") { v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DARRAY; v.Texture2DArray.MipLevels = 1; v.Texture2DArray.ArraySize = b.depth; }
        else if (b.dimension == "3d") { v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE3D; v.Texture3D.MipLevels = 1; }
        else if (b.dimension == "cube") { v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURECUBE; v.TextureCube.MipLevels = 1; }
        else { v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURECUBEARRAY; v.TextureCubeArray.MipLevels = 1; v.TextureCubeArray.NumCubes = b.depth / 6; }
    } else {
        v.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
        if (b.kind == "srv_raw") { v.Format = DXGI_FORMAT_R32_TYPELESS; v.Buffer.NumElements = b.width / 4; v.Buffer.Flags = D3D12_BUFFER_SRV_FLAG_RAW; }
        else if (b.kind == "srv_struct") { v.Format = DXGI_FORMAT_UNKNOWN; v.Buffer.NumElements = b.width; v.Buffer.StructureByteStride = b.stride; }
        else { v.Buffer.NumElements = b.width; }
    }
    device->CreateShaderResourceView(resource, &v, at);
}

void run_case(ID3D12Device *device, ID3D12CommandQueue *queue, const std::string &name) {
    const auto code = read_file(name + ".cso");
    if (code.empty()) throw std::runtime_error("missing shader");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0, code.data(), code.size(), IID_PPV_ARGS(&root)), "root signature");
    D3D12_COMPUTE_PIPELINE_STATE_DESC pd{};
    pd.pRootSignature = root.Get();
    pd.CS = {code.data(), code.size()};
    ComPtr<ID3D12PipelineState> pipeline;
    check(device->CreateComputePipelineState(&pd, IID_PPV_ARGS(&pipeline)), "compute pipeline");
    UINT groups[3]{};
    {
        std::ifstream grid(name + ".grid");
        if (!(grid >> groups[0] >> groups[1] >> groups[2])) throw std::runtime_error("missing grid");
    }
    const auto bindings = read_bindings(name);
    UINT views = 0, samplers = 0;
    for (const auto &b : bindings) (b.sampler() ? samplers : views) += b.count;
    ComPtr<ID3D12DescriptorHeap> view_heap, sampler_heap;
    D3D12_DESCRIPTOR_HEAP_DESC vh{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, views ? views : 1, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    check(device->CreateDescriptorHeap(&vh, IID_PPV_ARGS(&view_heap)), "view heap");
    D3D12_DESCRIPTOR_HEAP_DESC sh{D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, samplers ? samplers : 1, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, 0};
    check(device->CreateDescriptorHeap(&sh, IID_PPV_ARGS(&sampler_heap)), "sampler heap");
    const UINT view_step = device->GetDescriptorHandleIncrementSize(vh.Type);
    const UINT sampler_step = device->GetDescriptorHandleIncrementSize(sh.Type);
    auto view_at = view_heap->GetCPUDescriptorHandleForHeapStart();
    auto sampler_at = sampler_heap->GetCPUDescriptorHandleForHeapStart();

    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> list;
    check(device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&allocator)), "allocator");
    check(device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, allocator.Get(), pipeline.Get(), IID_PPV_ARGS(&list)), "list");

    std::vector<std::vector<Element>> elements(bindings.size());
    for (size_t r = 0; r < bindings.size(); ++r) {
        const auto &b = bindings[r];
        for (UINT e = 0; e < b.count; ++e) {
            if (b.sampler()) {
                D3D12_SAMPLER_DESC s{};
                const bool compare = b.kind == "sampler_cmp";
                s.Filter = compare ? D3D12_FILTER_COMPARISON_MIN_MAG_MIP_LINEAR : D3D12_FILTER_MIN_MAG_MIP_LINEAR;
                s.AddressU = s.AddressV = s.AddressW = D3D12_TEXTURE_ADDRESS_MODE_WRAP;
                s.ComparisonFunc = compare ? D3D12_COMPARISON_FUNC_LESS_EQUAL : D3D12_COMPARISON_FUNC_NEVER;
                s.MaxLOD = D3D12_FLOAT32_MAX;
                device->CreateSampler(&s, sampler_at);
                sampler_at.ptr += sampler_step;
                continue;
            }
            Element el;
            std::vector<char> seed = read_file(name + ".r" + std::to_string(r) + ".e" + std::to_string(e) + ".seed");
            if (seed.size() != b.bytes()) throw std::runtime_error("seed size for resource " + std::to_string(r));
            if (b.texture()) {
                const auto desc = texture_desc(b);
                D3D12_HEAP_PROPERTIES heap{};
                heap.Type = D3D12_HEAP_TYPE_DEFAULT;
                check(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST,
                                                      nullptr, IID_PPV_ARGS(&el.resource)), "texture");
                footprints(device, desc, el);
                el.staging = buffer(device, static_cast<UINT>(el.total), D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
                char *mapped{};
                D3D12_RANGE none{};
                check(el.staging->Map(0, &none, reinterpret_cast<void **>(&mapped)), "map staging");
                std::memset(mapped, 0, static_cast<size_t>(el.total));
                pack(el, seed, mapped, true);
                el.staging->Unmap(0, nullptr);
                copy_texture(list.Get(), el, true);
            } else {
                // Constant buffers round up to 256 bytes; the seed covers the whole buffer.
                el.total = b.bytes();
                el.resource = buffer(device, static_cast<UINT>(el.total), D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST,
                                     b.writable() ? D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS : D3D12_RESOURCE_FLAG_NONE);
                el.staging = buffer(device, static_cast<UINT>(el.total), D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
                upload(el.staging.Get(), seed.data(), seed.size());
                list->CopyBufferRegion(el.resource.Get(), 0, el.staging.Get(), 0, el.total);
            }
            transition(list.Get(), el.resource.Get(), D3D12_RESOURCE_STATE_COPY_DEST,
                       b.writable() ? D3D12_RESOURCE_STATE_UNORDERED_ACCESS
                       : b.kind == "cbv" ? D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER
                       : D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
            create_view(device, b, el.resource.Get(), view_at);
            view_at.ptr += view_step;
            elements[r].push_back(std::move(el));
        }
    }
    ID3D12DescriptorHeap *heaps[] = {view_heap.Get(), sampler_heap.Get()};
    list->SetDescriptorHeaps(2, heaps);
    list->SetComputeRootSignature(root.Get());
    UINT parameter = 0;
    if (views) list->SetComputeRootDescriptorTable(parameter++, view_heap->GetGPUDescriptorHandleForHeapStart());
    if (samplers) list->SetComputeRootDescriptorTable(parameter++, sampler_heap->GetGPUDescriptorHandleForHeapStart());
    list->Dispatch(groups[0], groups[1], groups[2]);
    for (size_t r = 0; r < bindings.size(); ++r) {
        if (!bindings[r].writable()) continue;
        for (auto &el : elements[r]) {
            transition(list.Get(), el.resource.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
            el.readback = buffer(device, static_cast<UINT>(el.total), D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
            if (bindings[r].texture()) copy_texture(list.Get(), el, false);
            else list->CopyBufferRegion(el.readback.Get(), 0, el.resource.Get(), 0, el.total);
        }
    }
    submit_and_wait(device, queue, list.Get());
    for (size_t r = 0; r < bindings.size(); ++r) {
        if (!bindings[r].writable()) continue;
        for (size_t e = 0; e < elements[r].size(); ++e) {
            auto &el = elements[r][e];
            std::vector<char> tight(static_cast<size_t>(bindings[r].bytes()));
            char *mapped{};
            D3D12_RANGE all{0, static_cast<SIZE_T>(el.total)}, none{};
            check(el.readback->Map(0, &all, reinterpret_cast<void **>(&mapped)), "map readback");
            if (bindings[r].texture()) pack(el, tight, mapped, false);
            else std::memcpy(tight.data(), mapped, tight.size());
            el.readback->Unmap(0, &none);
            std::ofstream output(name + ".u" + std::to_string(r) + ".e" + std::to_string(e) + ".bin", std::ios::binary);
            output.write(tight.data(), static_cast<std::streamsize>(tight.size()));
            if (!output) throw std::runtime_error("write output");
        }
    }
}
} // namespace

int main() {
    FILE *receipt{};
    if (fopen_s(&receipt, "result.txt", "wb")) return 2;
    int status = 0;
    try {
        ComPtr<ID3D12Device> device;
        check(D3D12CreateDevice(nullptr, D3D_FEATURE_LEVEL_12_0, IID_PPV_ARGS(&device)), "device");
        ComPtr<IDXGIFactory4> factory;
        ComPtr<IDXGIAdapter1> adapter;
        DXGI_ADAPTER_DESC1 gpu{};
        check(CreateDXGIFactory1(IID_PPV_ARGS(&factory)), "factory");
        check(factory->EnumAdapterByLuid(device->GetAdapterLuid(), IID_PPV_ARGS(&adapter)), "used adapter");
        check(adapter->GetDesc1(&gpu), "adapter description");
        std::fprintf(receipt, "adapter=%04x:%04x\n", gpu.VendorId, gpu.DeviceId);
        D3D12_FEATURE_DATA_D3D12_OPTIONS1 waves{};
        check(device->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS1, &waves, sizeof(waves)), "wave lane range");
        std::fprintf(receipt, "wave_lanes_min=%u\nwave_lanes_max=%u\n", waves.WaveLaneCountMin, waves.WaveLaneCountMax);
        std::fflush(receipt);
        D3D12_COMMAND_QUEUE_DESC qd{};
        ComPtr<ID3D12CommandQueue> queue;
        check(device->CreateCommandQueue(&qd, IID_PPV_ARGS(&queue)), "queue");
        watched = receipt;
        std::thread(watchdog).detach();
        std::ifstream cases("cases.txt");
        std::string name;
        while (cases >> name) {
            UINT needed = 0;
            if (std::ifstream(name + ".minwave") >> needed && needed > waves.WaveLaneCountMax) {
                std::fprintf(receipt, "%s_skipped=needs %u-lane waves\n", name.c_str(), needed);
                continue;
            }
            std::fprintf(receipt, "begin=%s\n", name.c_str());
            std::fflush(receipt);
            current = name;
            deadline = (std::chrono::steady_clock::now() + case_limit).time_since_epoch().count();
            try {
                run_case(device.Get(), queue.Get(), name);
                deadline = 0;
                std::fprintf(receipt, "%s_ok=1\n", name.c_str());
            } catch (const std::exception &error) {
                deadline = 0;
                // A fence that never completes leaves the device unusable for later cases.
                if (std::string(error.what()) == "GPU fence failed") {
                    std::fprintf(receipt, "%s_hang=1\n", name.c_str());
                    std::fclose(receipt);
                    return 3;
                }
                std::fprintf(receipt, "%s_error=%s\n", name.c_str(), error.what());
            }
            std::fflush(receipt);
        }
    } catch (const std::exception &error) {
        std::fprintf(receipt, "error=%s\n", error.what());
        status = 1;
    }
    return std::fclose(receipt) ? 2 : status;
}
