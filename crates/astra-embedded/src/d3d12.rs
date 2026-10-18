use super::*;

unsafe extern "C" fn open(
    request: *const OpenRequest,
    host: *const D3D12Host,
    output: *mut OpenResult,
    error: *mut Error,
) -> Status {
    unsafe { open_impl(request, host, output, error) }
}

unsafe extern "C" fn release(owner: *mut c_void, consumed: u32) {
    if owner.is_null() {
        return;
    }
    let frame = unsafe { Arc::from_raw(owner.cast::<Frame>()) };
    if let Some(gpu) = &frame.gpu {
        gpu.release(consumed);
    }
}

unsafe extern "C" fn acquire(
    session: Session,
    known: FrameIdentity,
    output: *mut D3D12Frame,
    error: *mut Error,
) -> Status {
    boundary(error, "RFVP_D3D12_ACQUIRE_FAILED", || {
        let handle = unsafe { handle(session)? };
        if !handle.native.gpu_delivery {
            return Ok(failure(
                error,
                UNSUPPORTED,
                "RFVP_D3D12_NOT_CONFIGURED",
                "session uses CPU delivery",
            ));
        }
        let output = unsafe { output.as_mut() }
            .ok_or_else(|| anyhow::anyhow!("missing GPU frame output"))?;
        let snapshot = handle.native.snapshot();
        let identity = FrameIdentity {
            session_epoch: handle.epoch,
            output_epoch: 1,
            generation: snapshot.frame.as_ref().map_or(0, |frame| frame.generation),
        };
        let mut result = D3D12Frame {
            state: FRAME_PENDING,
            dxgi_format: D3D12_FORMAT_RGBA8_UNORM_SRGB,
            identity,
            timestamp_ns: 0,
            info: surface(&handle.native),
            adapter_luid: 0,
            resource_id: 0,
            lease: D3D12Lease {
                owner: std::ptr::null_mut(),
                resource_handle: 0,
                ready_fence_handle: 0,
                done_fence_handle: 0,
                ready_value: 0,
                done_value: 0,
                release: None,
            },
        };
        if let Some(frame) = &snapshot.frame {
            let gpu = frame
                .gpu
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("RFVP_GPU_FRAME_INVALID"))?;
            result.timestamp_ns = frame.timestamp_ns;
            result.adapter_luid = gpu.resource.adapter_luid;
            if known == identity {
                result.state = FRAME_UNCHANGED;
            } else if gpu.acquire() {
                result.state = FRAME_READY;
                result.resource_id = gpu.resource.resource_id;
                result.lease = D3D12Lease {
                    owner: Arc::into_raw(frame.clone()).cast_mut().cast(),
                    resource_handle: gpu.resource_handle(),
                    ready_fence_handle: gpu.ready_handle(),
                    done_fence_handle: gpu.done_handle(),
                    ready_value: gpu.value,
                    done_value: gpu.value,
                    release: Some(release),
                };
            }
        }
        *output = result;
        Ok(OK)
    })
}

pub static API: D3D12Api = D3D12Api {
    header: D3D12Api::HEADER,
    open: Some(open),
    acquire: Some(acquire),
};
