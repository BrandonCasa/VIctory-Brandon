//! Byte-offset buffer views from the Agility SDK. The workspace's Windows
//! projection ends at ID3D12Device14. These layouts are the prefix used by
//! ID3D12Device15 in the shipping SDK's d3d12.h.

use core::ffi::c_void;
use std::sync::OnceLock;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

#[repr(transparent)]
#[derive(Clone)]
pub(super) struct Device15(IUnknown);

// SAFETY: Device15 owns the COM reference that QueryInterface returns.
unsafe impl Interface for Device15 {
    type Vtable = Device15Vtable;
    const IID: GUID = GUID::from_u128(0x76cff76f_1e9b_4450_8cdc_34f1af788e5b);
}
// SAFETY: ID3D12Device descriptor creation is free-threaded.
unsafe impl Send for Device15 {}
unsafe impl Sync for Device15 {}

#[repr(C)]
pub(super) struct Device15Vtable {
    base: ID3D12Device14_Vtbl,
    register_trim_callback: usize,
    unregister_trim_callback: usize,
    try_srv: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        *const Srv,
        D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> HRESULT,
    try_uav: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        *mut c_void,
        *const Uav,
        D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> HRESULT,
}

#[repr(C)]
struct Srv {
    format: DXGI_FORMAT,
    dimension: D3D12_SRV_DIMENSION,
    mapping: u32,
    // D3D12_BUFFER_SRV_BYTE_OFFSET
    offset: u64,
    size: u64,
    stride: u32,
    flags: D3D12_BUFFER_SRV_FLAGS,
}

#[repr(C)]
struct Uav {
    format: DXGI_FORMAT,
    dimension: D3D12_UAV_DIMENSION,
    // D3D12_BUFFER_UAV_BYTE_OFFSET
    offset: u64,
    size: u64,
    stride: u32,
    counter_offset: u64,
    flags: D3D12_BUFFER_UAV_FLAGS,
}

pub(super) fn device() -> Option<&'static Device15> {
    static DEVICE: OnceLock<Option<Device15>> = OnceLock::new();
    let device = &crate::host::host()?.device;
    DEVICE
        .get_or_init(|| {
            // D3D12_FEATURE_DATA_D3D12_OPTIONS22 contains two BOOLs and two
            // UINTs. The second field is CreateByteOffsetViewsSupported.
            let mut options = [0u32; 4];
            unsafe {
                device
                    .CheckFeatureSupport(D3D12_FEATURE(65), options.as_mut_ptr().cast(), 16)
                    .ok()?;
            }
            (options[1] != 0).then(|| device.cast().ok()).flatten()
        })
        .as_ref()
}

impl Device15 {
    pub(super) fn srv(
        &self,
        buffer: &ID3D12Resource,
        format: DXGI_FORMAT,
        mapping: u32,
        offset: u64,
        size: u64,
        at: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Option<()> {
        let desc = Srv {
            format,
            dimension: D3D12_SRV_DIMENSION(12),
            mapping,
            offset,
            size,
            stride: 0,
            flags: D3D12_BUFFER_SRV_FLAG_NONE,
        };
        // SAFETY: live COM inputs, descriptor union selected by dimension.
        unsafe { (self.vtable().try_srv)(self.as_raw(), buffer.as_raw(), &desc, at) }
            .ok()
            .ok()
    }

    pub(super) fn uav(
        &self,
        buffer: &ID3D12Resource,
        format: DXGI_FORMAT,
        offset: u64,
        size: u64,
        at: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Option<()> {
        let desc = Uav {
            format,
            dimension: D3D12_UAV_DIMENSION(9),
            offset,
            size,
            stride: 0,
            counter_offset: 0,
            flags: D3D12_BUFFER_UAV_FLAG_NONE,
        };
        // SAFETY: a typed buffer UAV with no counter.
        unsafe {
            (self.vtable().try_uav)(
                self.as_raw(),
                buffer.as_raw(),
                core::ptr::null_mut(),
                &desc,
                at,
            )
        }
        .ok()
        .ok()
    }
}
