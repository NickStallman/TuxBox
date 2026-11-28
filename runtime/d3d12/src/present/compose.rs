//! Draw a title's planes over one another into the back buffer.
//!
//! A console title can present its scaled 3D picture and a full-size interface
//! as separate planes, which the display engine scales and puts together.
//! ProjectScarlett does this in Series S mode.

use super::Layer;
use std::mem::{ManuallyDrop, size_of};
use windows::Win32::Foundation::{E_FAIL, E_POINTER, RECT};
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::core::{Interface, Result, s};

pub(crate) const MAX_LAYERS: usize = 4;
const FRAMES: usize = 2;

const VERTEX: &str = "cbuffer C : register(b0) { float4 dst; float4 src; };
struct V { float4 p : SV_Position; float2 t : TEXCOORD0; };
V main(uint i : SV_VertexID) {
  float2 u = float2(i & 1, i >> 1), q = lerp(dst.xy, dst.zw, u);
  V v; v.p = float4(q.x * 2 - 1, 1 - q.y * 2, 0, 1); v.t = lerp(src.xy, src.zw, u); return v; }";
const PIXEL: &str = "Texture2D<float4> T : register(t0); SamplerState S : register(s0);
float4 main(float4 p : SV_Position, float2 t : TEXCOORD0) : SV_Target { return T.Sample(S, t); }";

pub(super) struct Composer {
    root: ID3D12RootSignature,
    opaque: ID3D12PipelineState,
    blended: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    targets: ID3D12DescriptorHeap,
    view_step: usize,
    target_step: usize,
}

impl Composer {
    pub(super) fn new(device: &ID3D12Device, format: DXGI_FORMAT) -> Result<Self> {
        let vertex = crate::device::fallback::compile(VERTEX, s!("vs_5_0")).ok_or(E_FAIL)?;
        let pixel = crate::device::fallback::compile(PIXEL, s!("ps_5_0")).ok_or(E_FAIL)?;
        // SAFETY: every descriptor outlives the call that reads it.
        unsafe {
            let root = root_signature(device)?;
            let heap = |kind, count: usize, flags| {
                device.CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: count as u32,
                    Flags: flags,
                    NodeMask: 0,
                })
            };
            Ok(Self {
                opaque: pipeline(device, &root, format, &vertex, &pixel, false)?,
                blended: pipeline(device, &root, format, &vertex, &pixel, true)?,
                views: heap(
                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    FRAMES * MAX_LAYERS,
                    D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                )?,
                targets: heap(D3D12_DESCRIPTOR_HEAP_TYPE_RTV, FRAMES, D3D12_DESCRIPTOR_HEAP_FLAG_NONE)?,
                view_step: device
                    .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                    as usize,
                target_step: device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize,
                root,
            })
        }
    }

    /// Record the layers, the first at the back. `frame` has retired, so its descriptors are free.
    pub(super) fn record(
        &self,
        device: &ID3D12Device,
        list: &ID3D12GraphicsCommandList,
        frame: usize,
        buffer: &ID3D12Resource,
        (width, height): (u32, u32),
        layers: &[Layer],
    ) {
        let mut sources: Vec<&ID3D12Resource> = Vec::with_capacity(layers.len());
        for layer in layers {
            if sources.iter().all(|seen| seen.as_raw() != layer.source.as_raw()) {
                sources.push(layer.source);
            }
        }
        // SAFETY: the list is open, its resources belong to this device and the caller retains them.
        unsafe {
            let target = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: self.targets.GetCPUDescriptorHandleForHeapStart().ptr + frame * self.target_step,
            };
            device.CreateRenderTargetView(buffer, None, target);
            transition(list, &sources, buffer, true);
            list.OMSetRenderTargets(1, Some(&target), false, None);
            list.ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0], None);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                Width: width as f32,
                Height: height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            list.RSSetScissorRects(&[RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 }]);
            list.SetDescriptorHeaps(&[Some(self.views.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            for (index, layer) in layers.iter().take(MAX_LAYERS).enumerate() {
                let slot = (frame * MAX_LAYERS + index) * self.view_step;
                device.CreateShaderResourceView(
                    layer.source,
                    None,
                    D3D12_CPU_DESCRIPTOR_HANDLE {
                        ptr: self.views.GetCPUDescriptorHandleForHeapStart().ptr + slot,
                    },
                );
                let desc = layer.source.GetDesc();
                let (w, h) = (desc.Width as f32, desc.Height as f32);
                let from = layer.from.map_or([0.0, 0.0, 1.0, 1.0], |r| {
                    [r.left as f32 / w, r.top as f32 / h, r.right as f32 / w, r.bottom as f32 / h]
                });
                let constants = [layer.to.unwrap_or([0.0, 0.0, 1.0, 1.0]), from];
                list.SetPipelineState(if index == 0 { &self.opaque } else { &self.blended });
                list.SetGraphicsRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
                list.SetGraphicsRootDescriptorTable(
                    1,
                    D3D12_GPU_DESCRIPTOR_HANDLE {
                        ptr: self.views.GetGPUDescriptorHandleForHeapStart().ptr + slot as u64,
                    },
                );
                list.DrawInstanced(4, 1, 0, 0);
            }
            transition(list, &sources, buffer, false);
        }
    }
}

unsafe fn transition(
    list: &ID3D12GraphicsCommandList,
    sources: &[&ID3D12Resource],
    buffer: &ID3D12Resource,
    begin: bool,
) {
    let barrier = |resource: &ID3D12Resource, idle, busy| D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: 0,
                StateBefore: if begin { idle } else { busy },
                StateAfter: if begin { busy } else { idle },
            }),
        },
    };
    let mut barriers: Vec<_> = sources
        .iter()
        .map(|source| barrier(source, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE))
        .collect();
    barriers.push(barrier(buffer, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET));
    // SAFETY: every union arm is Transition. Release its owned pointer after D3D12 copied the descriptor.
    unsafe {
        list.ResourceBarrier(&barriers);
        for barrier in &mut barriers {
            ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
        }
    }
}

unsafe fn root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature> {
    let range = D3D12_DESCRIPTOR_RANGE {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
        NumDescriptors: 1,
        BaseShaderRegister: 0,
        RegisterSpace: 0,
        OffsetInDescriptorsFromTableStart: 0,
    };
    let parameters = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_VERTEX,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 8 },
            },
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE { NumDescriptorRanges: 1, pDescriptorRanges: &range },
            },
        },
    ];
    let sampler = D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_LINEAR_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        ..Default::default()
    };
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: parameters.len() as u32,
        pParameters: parameters.as_ptr(),
        NumStaticSamplers: 1,
        pStaticSamplers: &sampler,
        Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };
    // SAFETY: the descriptor and everything it points at live until the call returns.
    unsafe {
        let mut blob = None;
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)?;
        let blob = blob.ok_or(E_POINTER)?;
        device.CreateRootSignature(
            0,
            std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()),
        )
    }
}

unsafe fn pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    format: DXGI_FORMAT,
    vertex: &[u8],
    pixel: &[u8],
    blend: bool,
) -> Result<ID3D12PipelineState> {
    let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root.clone())),
        VS: crate::device::fallback::bytecode(vertex),
        PS: crate::device::fallback::bytecode(pixel),
        SampleMask: u32::MAX,
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    desc.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
    desc.RasterizerState.CullMode = D3D12_CULL_MODE_NONE;
    desc.RTVFormats[0] = format;
    // An interface drawn over transparent black is premultiplied by its alpha.
    desc.BlendState.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: blend.into(),
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
        ..Default::default()
    };
    const _: () = assert!(size_of::<[[f32; 4]; 2]>() == 32);
    // SAFETY: the descriptor's pointers outlive the call; release the root reference it owns afterwards.
    unsafe {
        let state = device.CreateGraphicsPipelineState(&desc);
        ManuallyDrop::drop(&mut desc.pRootSignature);
        state
    }
}
