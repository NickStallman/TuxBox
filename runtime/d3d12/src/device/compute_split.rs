//! The pipeline a split compute dispatch runs (see `tuxbox_dxbc::group_base`).
//!
//! A console dispatch with more than 65,535 groups in a dimension is recorded
//! as several desktop dispatches. Each needs the console group ID it starts
//! at, so the program is translated again against its pipeline root with the
//! group base constants appended, and runs on that root. Few programs ever
//! meet such a dispatch, so the variant is made at the first one.
use super::compute::RootSource;
use std::sync::{Arc, OnceLock};
use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{Interface, Result};

/// What a translated compute pipeline needs to make its split variant.
pub(crate) struct GroupBase {
    device: ID3D12Device,
    /// The console program.
    shader: Arc<[u8]>,
    /// The root the pipeline was created with; the variant's extends it.
    root: RootSource,
    node_mask: u32,
    flags: D3D12_PIPELINE_STATE_FLAGS,
    /// The host's wave range, for the variant's narrow-wave translation.
    waves: Option<(u32, u32)>,
    variant: OnceLock<std::result::Result<Variant, String>>,
}

pub(crate) struct Variant {
    pub(crate) pipeline: ID3D12PipelineState,
    pub(crate) root: ID3D12RootSignature,
    /// The root parameter of the group base constants.
    pub(crate) slot: u32,
}

impl GroupBase {
    pub(super) fn new(
        device: &ID3D12Device, shader: &[u8], root: RootSource, node_mask: u32,
        flags: D3D12_PIPELINE_STATE_FLAGS, waves: Option<(u32, u32)>,
    ) -> Self {
        Self { device: device.clone(), shader: shader.into(), root, node_mask, flags, waves, variant: OnceLock::new() }
    }

    /// The split variant, made at the first call. `Err` says why there is none.
    pub(crate) fn variant(&self) -> std::result::Result<&Variant, &str> {
        self.variant.get_or_init(|| self.make()).as_ref().map_err(String::as_str)
    }

    fn make(&self) -> std::result::Result<Variant, String> {
        let (root, slot, layout) = unsafe { with_group_base(&self.device, &self.root, self.node_mask) }
            .map_err(|error| format!("the root cannot take the group base ({:#010x})", error.code().0))?;
        let translation = super::shader_cache::compute_with_root(&self.shader, layout.payload())
            .ok_or("the program has no translation for the root with the group base")?;
        let (code, request) = match self.waves {
            Some(host) if super::compute::needs_narrow_waves(super::shader_cache::portable_wave_range(&translation.code), host) => {
                super::shader_cache::narrow_wave_variant(&translation.code).unwrap_or((translation.code, translation.request))
            }
            _ => (translation.code, translation.request),
        };
        if let (Some(required), Some(host)) = (super::shader_cache::portable_wave_range(&code), self.waves)
            && let Some(refusal) = super::compute::check_wave_range(required, host)
        {
            return Err(refusal.detail);
        }
        let short = super::shader_cache::source_digest(&self.shader).and_then(|hash| hash.get(..8).map(str::to_owned));
        let label = format!("CS {} group base request={request} bytes={}", short.as_deref().unwrap_or("unknown"), code.len());
        let (device, signature, flags, node_mask) = (self.device.clone(), Some(root.clone()), self.flags, self.node_mask);
        let pipeline = crate::big_stack::run_logged(&label, move || {
            super::compute::owned_compile(device, signature, code, node_mask, flags)
        })
        .map_err(|error| format!("the driver refused the variant ({:#010x})", error.code().0))?;
        crate::diagnostics::log(&format!("created the group base variant of {label}"));
        Ok(Variant { pipeline, root, slot })
    }
}

/// `root` with the group base constants appended as its last parameter,
/// the native root, that parameter's index and the serialized form.
unsafe fn with_group_base(
    device: &ID3D12Device, root: &RootSource, node_mask: u32,
) -> Result<(ID3D12RootSignature, u32, RootSource)> {
    use tuxbox_dxbc::group_base::{REGISTER, SPACE, WORDS};
    let bytes = root.serialized();
    let mut reader = None::<ID3D12VersionedRootSignatureDeserializer>;
    unsafe {
        D3D12CreateVersionedRootSignatureDeserializer(
            bytes.as_ptr().cast(),
            bytes.len(),
            &ID3D12VersionedRootSignatureDeserializer::IID,
            (&raw mut reader).cast(),
        )?;
        let reader = reader.ok_or(E_POINTER)?;
        let original = reader.GetUnconvertedRootSignatureDesc().as_ref().ok_or(E_POINTER)?;
        let mut desc = match original.Version {
            D3D_ROOT_SIGNATURE_VERSION_1_0 => *reader.GetRootSignatureDescAtVersion(D3D_ROOT_SIGNATURE_VERSION_1_1)?,
            D3D_ROOT_SIGNATURE_VERSION_1_1 | D3D_ROOT_SIGNATURE_VERSION_1_2 => *original,
            _ => return Err(E_INVALIDARG.into()),
        };
        let (count, params) = if desc.Version == D3D_ROOT_SIGNATURE_VERSION_1_2 {
            (desc.Anonymous.Desc_1_2.NumParameters, desc.Anonymous.Desc_1_2.pParameters)
        } else {
            (desc.Anonymous.Desc_1_1.NumParameters, desc.Anonymous.Desc_1_1.pParameters)
        };
        let mut params = if count == 0 {
            Vec::new()
        } else if params.is_null() {
            return Err(E_INVALIDARG.into());
        } else {
            core::slice::from_raw_parts(params, count as usize).to_vec()
        };
        params.push(D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: REGISTER, RegisterSpace: SPACE, Num32BitValues: WORDS },
            },
        });
        if desc.Version == D3D_ROOT_SIGNATURE_VERSION_1_2 {
            desc.Anonymous.Desc_1_2.NumParameters = params.len() as u32;
            desc.Anonymous.Desc_1_2.pParameters = params.as_ptr();
        } else {
            desc.Anonymous.Desc_1_1.NumParameters = params.len() as u32;
            desc.Anonymous.Desc_1_1.pParameters = params.as_ptr();
        }
        // The serializer refuses a root that would cost more than 64 DWORDs.
        let mut blob = None;
        D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
        let blob = blob.ok_or(E_POINTER)?;
        let bytes = core::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize());
        Ok((device.CreateRootSignature(node_mask, bytes)?, count, RootSource::new(bytes)))
    }
}
