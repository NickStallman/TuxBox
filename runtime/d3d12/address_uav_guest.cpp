// Write through a structured UAV that a title made at its own address.
// ProjectScarlett makes a UAV of two 20-byte records 432 bytes into an
// allocation. Neither the allocation's 64 KiB page nor any 128-byte boundary
// before the view is a whole number of records from it, so on a host whose
// tight buffer alignment is 128 bytes or more no import puts it on an element.
#include "compute_fixture.h"
#include "frame_cli.h"
#include "address_uav.h"
#include <cstddef>
namespace {
using namespace frame_abi;
using namespace compute_fixture;
using Pipeline = HRESULT(WINAPI*)(ID3D12Device*,const D3D12_COMPUTE_PIPELINE_STATE_DESC*,UINT,const void*,REFIID,void**);
using BindHeaps = void(WINAPI*)(ID3D12GraphicsCommandList*,UINT,ID3D12DescriptorHeap* const*);
using Root = void(WINAPI*)(ID3D12GraphicsCommandList*,ID3D12RootSignature*);
using Table = void(WINAPI*)(ID3D12GraphicsCommandList*,UINT,D3D12_GPU_DESCRIPTOR_HANDLE);
using GpuHandle = UINT64(WINAPI*)(ID3D12DescriptorHeap*);
// D3D12XBOX_UNORDERED_ACCESS_VIEW_DESC: the standard description, then the
// buffer's address, a counter address and a memory type.
struct AddressUav {
    D3D12_UNORDERED_ACCESS_VIEW_DESC view;
    UINT64 address;
    UINT64 counter_address;
    UINT memory_type;
};
static_assert(offsetof(AddressUav,address)==40);
// CreatePlacedRawUnorderedAccessViewX.
using PlaceUav = void(WINAPI*)(ID3D12Device*,const D3D12_RESOURCE_DESC*,const AddressUav*,D3D12_CPU_DESCRIPTOR_HANDLE);
constexpr UINT start=432, stride=20, records=2, bytes=stride*records, size=0x10000, guard=0x5a5a5a5au;
std::string run(const wchar_t* requested,bool) {
    DXGI_ADAPTER_DESC1 selected{};
    constexpr std::array<UINT,18> creation{0x20004,0,UINT_MAX,0,0,0,UINT_MAX,UINT_MAX,0,1,0,0,0,0,0,0,0,0};
    auto device=create_device(requested,selected,creation.data());
    D3D12_DESCRIPTOR_RANGE range{D3D12_DESCRIPTOR_RANGE_TYPE_UAV,1,0,0,0};
    D3D12_ROOT_PARAMETER param{};
    param.ParameterType=D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    param.DescriptorTable={1,&range};
    D3D12_ROOT_SIGNATURE_DESC rd{1,&param,0,nullptr,D3D12_ROOT_SIGNATURE_FLAG_NONE};
    ComPtr<ID3DBlob> blob;
    check(D3D12SerializeRootSignature(&rd,D3D_ROOT_SIGNATURE_VERSION_1,&blob,nullptr),"serialize root");
    ComPtr<ID3D12RootSignature> root;
    check(device->CreateRootSignature(0,blob->GetBufferPointer(),blob->GetBufferSize(),IID_PPV_ARGS(&root)),"root");
    D3D12_COMPUTE_PIPELINE_STATE_DESC ps{};
    ps.pRootSignature=root.Get();ps.CS={address_uav_dxil,sizeof(address_uav_dxil)};
    ComPtr<ID3D12PipelineState> pipeline;
    check(method<Pipeline>(device.Get(),62)(device.Get(),&ps,0,nullptr,IID_PPV_ARGS(&pipeline)),"pipeline");
    D3D12_DESCRIPTOR_HEAP_DESC hd{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,1,D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,1};
    ComPtr<ID3D12DescriptorHeap> views;
    check(device->CreateDescriptorHeap(&hd,IID_PPV_ARGS(&views)),"descriptor heap");
    const D3D12_CPU_DESCRIPTOR_HANDLE cpu{method<CpuHandle>(views.Get(),9)(views.Get())};
    const D3D12_GPU_DESCRIPTOR_HANDLE gpu{method<GpuHandle>(views.Get(),10)(views.Get())};
    struct Memory {
        UINT* data=static_cast<UINT*>(VirtualAlloc(nullptr,size,MEM_RESERVE|MEM_COMMIT,PAGE_READWRITE));
        ~Memory(){if(data)VirtualFree(data,0,MEM_RELEASE);}
    } memory;
    if(!memory.data)throw std::runtime_error("title memory");
    for(UINT i=0;i<size/4;++i)memory.data[i]=guard;
    D3D12_RESOURCE_DESC buffer{};
    buffer.Dimension=D3D12_RESOURCE_DIMENSION_BUFFER;buffer.Width=bytes;buffer.Height=1;buffer.DepthOrArraySize=1;
    buffer.MipLevels=1;buffer.SampleDesc.Count=1;buffer.Layout=D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    buffer.Flags=D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    AddressUav uav{};
    uav.view.Format=DXGI_FORMAT_UNKNOWN;
    uav.view.ViewDimension=D3D12_UAV_DIMENSION_BUFFER;
    uav.view.Buffer.NumElements=records;
    uav.view.Buffer.StructureByteStride=stride;
    uav.address=reinterpret_cast<UINT64>(memory.data)+start;
    method<PlaceUav>(device.Get(),71)(device.Get(),&buffer,&uav,cpu);
    D3D12_COMMAND_QUEUE_DESC q{};q.Type=D3D12_COMMAND_LIST_TYPE_COMPUTE;
    ComPtr<ID3D12CommandQueue> queue;check(device->CreateCommandQueue(&q,IID_PPV_ARGS(&queue)),"queue");
    ComPtr<ID3D12Fence> fence;check(device->CreateFence(0,D3D12_FENCE_FLAG_NONE,IID_PPV_ARGS(&fence)),"fence");
    ComPtr<ID3D12CommandAllocator> allocator;
    check(device->CreateCommandAllocator(q.Type,IID_PPV_ARGS(&allocator)),"allocator");
    const UINT desc[]{D3D12_COMMAND_LIST_TYPE_COMPUTE,0,1,0};
    ComPtr<ID3D12GraphicsCommandList> list;
    check(method<CreateList>(device.Get(),73)(device.Get(),desc,allocator.Get(),pipeline.Get(),IID_PPV_ARGS(&list)),"list");
    ID3D12DescriptorHeap* heaps[]{views.Get()};
    method<BindHeaps>(list.Get(),32)(list.Get(),1,heaps);
    method<Root>(list.Get(),33)(list.Get(),root.Get());
    method<Table>(list.Get(),35)(list.Get(),0,gpu);
    list->Dispatch(1,1,1);
    check(list->Close(),"close");
    ID3D12CommandList* batch[]{list.Get()};queue->ExecuteCommandLists(1,batch);
    check(queue->Signal(fence.Get(),1),"signal");
    wait_for_fence(fence.Get(),1);
    // The records land at the title address and nothing around them changes.
    UINT right=0,guards=0;
    for(UINT i=0;i<size/4;++i){
        const UINT byte=i*4;
        if(byte>=start&&byte<start+bytes){
            const UINT record=(byte-start)/stride,word=(byte-start)%stride/4;
            if(memory.data[i]==0xa0000000u+word+record*16u)++right;
        }else if(memory.data[i]==guard)++guards;
    }
    constexpr UINT words=bytes/4,guard_words=size/4-bytes/4;
    if(right!=words||guards!=guard_words){
        char detail[200];
        std::snprintf(detail,sizeof(detail),"address UAV words=%u/%u guards=%u/%u (first record word %08x, word before %08x)",
            right,words,guards,guard_words,memory.data[start/4],memory.data[start/4-1]);
        throw std::runtime_error(detail);
    }
    char receipt[160];
    std::snprintf(receipt,sizeof(receipt),"d3d12-address-uav PASS pci=%04x:%04x words=%u/%u guards=%u start=%u stride=%u",
        selected.VendorId,selected.DeviceId,right,words,guards,start,stride);
    return receipt;
}
}
int wmain(int argc,wchar_t** argv){
    return tuxbox_test::frame_main(argc,argv,"d3d12-address-uav",false,run);
}
