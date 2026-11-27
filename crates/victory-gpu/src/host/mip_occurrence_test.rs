use super::*;
use std::sync::Arc;

pub(crate) fn run(
    host: &Host,
    target: crate::object::This,
    copy: Arc<crate::resource::HostTexture>,
) {
    let read =
        D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
    let rt = D3D12_RESOURCE_STATE_RENDER_TARGET;
    let src = D3D12_RESOURCE_STATE_COPY_SOURCE;
    let image = &copy.texture;
    let view = |mip| D3D12_RENDER_TARGET_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_RTV {
                MipSlice: mip,
                PlaneSlice: 0,
            },
        },
    };
    let back = host.read_back_buffer(1536).unwrap();
    let body = host.new_list().unwrap();
    crate::list::ensure_state(&body, target, 4);
    // The repeated body writes mip 0 and copies one pixel from each of its
    // two entry states. The second iteration supplies different mip 1 data.
    unsafe {
        body.list.ClearRenderTargetView(
            host.rtv(Some(image), Some(&view(0))),
            &[1., 0., 0., 1.],
            None,
        );
        for (mip, state, offset) in [(0, rt, 0), (1, read, 1024)] {
            body.list
                .ResourceBarrier(&[subresource_transition(image, mip, state, src)]);
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(image),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: mip,
                },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&back),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: offset,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            Width: 1,
                            Height: 1,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let pixel = D3D12_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: 1,
                bottom: 1,
                back: 1,
            };
            body.list
                .CopyTextureRegion(&to, 0, 0, 0, &from, Some(&pixel));
            body.list
                .ResourceBarrier(&[subresource_transition(image, mip, src, state)]);
        }
    }
    body.close();
    let mut sampled = LateSamples::default();
    sampled.native.insert(copy.serial, copy.clone());
    sampled.split_mips.insert((copy.serial, 1));
    let mut kept = PreludeResources::default();
    for (i, color) in [[0., 1., 0., 1.], [0., 0., 1., 1.]].into_iter().enumerate() {
        let seed = host.new_list().unwrap();
        crate::list::ensure_state(&seed, target, 4);
        unsafe {
            seed.list
                .ClearRenderTargetView(host.rtv(Some(image), Some(&view(1))), &color, None);
        }
        seed.close();
        host.execute(&[&seed]);
        let restore = host.sample_mip_epilogue(&sampled, &mut kept).unwrap();
        let enter = body.prelude(&[], &sampled, i != 0, &mut kept).unwrap();
        let commands = [Some(enter), Some(body.list.cast().unwrap()), Some(restore)];
        let _queue = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            host.queue.ExecuteCommandLists(&commands);
            host.queue.Signal(&fence, 1).unwrap();
            fence.SetEventOnCompletion(1, HANDLE::default()).unwrap();
            host.device.GetDeviceRemovedReason().unwrap();
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data)).unwrap();
            let pixel0 = data.cast::<[u8; 4]>().read();
            let pixel1 = data.cast::<u8>().add(1024).cast::<[u8; 4]>().read();
            back.Unmap(0, None);
            assert_eq!(pixel0, [255, 0, 0, 255]);
            assert_eq!(
                pixel1,
                if i == 0 {
                    [0, 255, 0, 255]
                } else {
                    [0, 0, 255, 255]
                }
            );
        }
        assert_eq!(copy.state.load(Ordering::Relaxed), 4);
        assert_eq!(host.log_messages(), None);
    }
    // Two independent restores plus the second occurrence's separate entry.
    assert_eq!(kept.lists.len(), 3);
    let adapter = unsafe {
        CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter1>(host.device.GetAdapterLuid()))
            .and_then(|a| a.GetDesc1())
    }
    .unwrap();
    eprintln!(
        "mip entry/restore verified on {} (debug queue enabled: {})",
        String::from_utf16_lossy(&adapter.Description).trim_end_matches('\0'),
        host.messages.is_some()
    );
}
