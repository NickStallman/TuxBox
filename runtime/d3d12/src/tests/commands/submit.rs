use super::rejection_text;
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D12::{D3D12_COMMAND_LIST_TYPE_COMPUTE, D3D12_COMMAND_LIST_TYPE_DIRECT};

#[test]
fn a_rejection_names_its_queue_and_the_error() {
    assert_eq!(
        rejection_text(D3D12_COMMAND_LIST_TYPE_COMPUTE, E_INVALIDARG),
        "Project TuxBox: compute queue rejected a guest command-list batch (0x80070057); its later batches are dropped\n\0"
    );
    assert!(rejection_text(D3D12_COMMAND_LIST_TYPE_DIRECT, E_INVALIDARG).starts_with("Project TuxBox: direct queue "));
}
