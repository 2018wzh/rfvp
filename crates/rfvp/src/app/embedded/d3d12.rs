//! Independent-device D3D12 exports. Native scene/history textures never enter this pool.
use super::session::Frame;
use anyhow::{Context, Result};
use std::{
    mem::ManuallyDrop,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::Interface,
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Graphics::{Direct3D12::*, Dxgi::Common::*},
    },
};

#[derive(Clone, Copy)]
pub struct Host {
    pub adapter_luid: u64,
    pub max_width: u32,
    pub max_height: u32,
    pub dxgi_format: u32,
    pub copy_usage: u32,
}

struct SharedHandle(usize);
impl Drop for SharedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(HANDLE(self.0 as *mut _));
        }
    }
}

/// No wgpu texture, native App, device, VM or Host callback is retained by a lease.
pub struct Resource {
    _resource: ID3D12Resource,
    ready: ID3D12Fence,
    done: ID3D12Fence,
    resource_handle: SharedHandle,
    ready_handle: SharedHandle,
    done_handle: SharedHandle,
    pub adapter_luid: u64,
    pub resource_id: u64,
}

pub struct GpuFrame {
    pub resource: Arc<Resource>,
    pub value: u64,
    acquired: AtomicBool,
    disposition: AtomicU32,
}
impl GpuFrame {
    pub fn acquire(&self) -> bool {
        self.acquired
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub fn release(&self, disposition: u32) {
        self.disposition.store(disposition, Ordering::Release);
    }
    pub fn resource_handle(&self) -> usize {
        self.resource.resource_handle.0
    }
    pub fn ready_handle(&self) -> usize {
        self.resource.ready_handle.0
    }
    pub fn done_handle(&self) -> usize {
        self.resource.done_handle.0
    }
}

struct Slot {
    resource: Arc<Resource>,
    texture: wgpu::Texture,
    // Lists are immutable and only resubmitted after both queues have completed.
    _allocator: ID3D12CommandAllocator,
    acquire: [Option<ID3D12CommandList>; 1],
    release: [Option<ID3D12CommandList>; 1],
    frame: Option<Arc<Frame>>,
}

pub struct ExportPool {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    shutdown: ID3D12Fence,
    slots: [Slot; 3],
    size: wgpu::Extent3d,
}

fn luid(device: &ID3D12Device) -> u64 {
    let luid = unsafe { device.GetAdapterLuid() };
    (luid.HighPart as u32 as u64) << 32 | luid.LowPart as u64
}

fn shared_handle(device: &ID3D12Device, object: &impl Interface) -> Result<SharedHandle> {
    let child: ID3D12DeviceChild = object.cast()?;
    Ok(SharedHandle(
        unsafe { device.CreateSharedHandle(&child, None, 0x10000000, None)? }.0 as usize,
    ))
}

fn barrier_list(
    device: &ID3D12Device,
    allocator: &ID3D12CommandAllocator,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> Result<ID3D12CommandList> {
    let list: ID3D12GraphicsCommandList =
        unsafe { device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, allocator, None)? };
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    unsafe {
        list.ResourceBarrier(std::slice::from_ref(&barrier));
        // Windows bindings use ManuallyDrop for COM members inside C unions.
        ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
        list.Close()?;
    }
    Ok(list.cast()?)
}

impl ExportPool {
    pub fn new(device: &wgpu::Device, host: Host, width: u32, height: u32) -> Result<Self> {
        anyhow::ensure!(
            host.dxgi_format == DXGI_FORMAT_R8G8B8A8_UNORM_SRGB.0 as u32 && host.copy_usage == 3,
            "RFVP_D3D12_FORMAT_UNSUPPORTED"
        );
        anyhow::ensure!(
            width <= host.max_width && height <= host.max_height,
            "RFVP_D3D12_HOST_LIMIT"
        );
        let (raw, queue) = unsafe {
            device.as_hal::<wgpu::hal::api::Dx12, _, _>(|hal| {
                let hal = hal.context("RFVP_D3D12_BACKEND_REQUIRED")?;
                let raw = ID3D12Device::from_raw_borrowed(&hal.raw_device().as_mut_ptr().cast())
                    .unwrap()
                    .clone();
                let queue =
                    ID3D12CommandQueue::from_raw_borrowed(&hal.raw_queue().as_mut_ptr().cast())
                        .unwrap()
                        .clone();
                Ok::<_, anyhow::Error>((raw, queue))
            })
        }
        .context("RFVP_D3D12_BACKEND_REQUIRED")??;
        anyhow::ensure!(
            luid(&raw) == host.adapter_luid,
            "RFVP_D3D12_ADAPTER_MISMATCH"
        );
        let mut support = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
            ..Default::default()
        };
        unsafe {
            raw.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_SUPPORT,
                (&mut support as *mut D3D12_FEATURE_DATA_FORMAT_SUPPORT).cast(),
                std::mem::size_of_val(&support) as u32,
            )?;
        }
        anyhow::ensure!(
            support.Support1.contains(D3D12_FORMAT_SUPPORT1_TEXTURE2D),
            "RFVP_D3D12_FORMAT_UNSUPPORTED"
        );
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let mut slots = Vec::with_capacity(3);
        for id in 1..=3 {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: width as u64,
                Height: height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_NONE,
                ..Default::default()
            };
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                CreationNodeMask: 1,
                VisibleNodeMask: 1,
                ..Default::default()
            };
            let mut resource: Option<ID3D12Resource> = None;
            unsafe {
                raw.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_SHARED,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut resource,
                )?;
            }
            let resource = resource.context("RFVP_D3D12_RESOURCE_MISSING")?;
            let ready: ID3D12Fence = unsafe { raw.CreateFence(0, D3D12_FENCE_FLAG_SHARED)? };
            let done: ID3D12Fence = unsafe { raw.CreateFence(0, D3D12_FENCE_FLAG_SHARED)? };
            let resource_handle = shared_handle(&raw, &resource)?;
            let ready_handle = shared_handle(&raw, &ready)?;
            let done_handle = shared_handle(&raw, &done)?;
            let allocator = unsafe { raw.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)? };
            let acquire = barrier_list(
                &raw,
                &allocator,
                &resource,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )?;
            let release = barrier_list(
                &raw,
                &allocator,
                &resource,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COMMON,
            )?;
            // Committed shared allocations are zero-initialized by D3D12. Every exported
            // generation then overwrites the whole image before ready is signalled.
            let hal = unsafe {
                wgpu::hal::dx12::Device::texture_from_raw(
                    d3d12::Resource::from_raw(resource.as_raw().cast()),
                    wgpu::TextureFormat::Rgba8UnormSrgb,
                    wgpu::TextureDimension::D2,
                    size,
                    1,
                    1,
                )
            };
            let texture = unsafe {
                device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                    hal,
                    &wgpu::TextureDescriptor {
                        label: Some("rfvp D3D12 shared export"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8UnormSrgb,
                        usage: wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    },
                )
            };
            slots.push(Slot {
                resource: Arc::new(Resource {
                    _resource: resource,
                    ready,
                    done,
                    resource_handle,
                    ready_handle,
                    done_handle,
                    adapter_luid: host.adapter_luid,
                    resource_id: id,
                }),
                texture,
                _allocator: allocator,
                acquire: [Some(acquire)],
                release: [Some(release)],
                frame: None,
            });
        }
        let shutdown = unsafe { raw.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
        Ok(Self {
            device: raw,
            queue,
            shutdown,
            slots: slots.try_into().ok().unwrap(),
            size,
        })
    }

    pub fn publish(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: &wgpu::Texture,
        generation: u64,
        timestamp_ns: u64,
    ) -> Result<Option<Arc<Frame>>> {
        anyhow::ensure!(
            generation != 0 && generation != u64::MAX,
            "RFVP_FRAME_GENERATION_EXHAUSTED"
        );
        unsafe {
            self.device.GetDeviceRemovedReason()?;
        }
        let mut available = None;
        for (index, slot) in self.slots.iter().enumerate() {
            let reusable = if let Some(frame) = &slot.frame {
                let gpu = frame.gpu.as_ref().unwrap();
                let ready = unsafe { slot.resource.ready.GetCompletedValue() };
                let done = unsafe { slot.resource.done.GetCompletedValue() };
                anyhow::ensure!(
                    ready != u64::MAX && done != u64::MAX,
                    "RFVP_D3D12_DEVICE_LOST"
                );
                Arc::strong_count(frame) == 1
                    && ready >= gpu.value
                    && match gpu.disposition.load(Ordering::Acquire) {
                        0 => true,
                        1 => done >= gpu.value,
                        _ => false,
                    }
            } else {
                true
            };
            if reusable {
                available = Some(index);
                break;
            }
        }
        let Some(index) = available else {
            return Ok(None);
        };
        let slot = &mut self.slots[index];
        // wgpu 0.19 tracks the persistent export as COPY_DST after its first use.
        // Restore that exact state after the external COMMON interval, before
        // wgpu submits its next copy. No Host fence wait is placed on this queue.
        if slot.frame.is_some() {
            unsafe {
                self.queue.ExecuteCommandLists(&slot.acquire);
            }
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rfvp final UI to shared export"),
        });
        encoder.copy_texture_to_texture(
            source.as_image_copy(),
            slot.texture.as_image_copy(),
            self.size,
        );
        queue.submit([encoder.finish()]);
        unsafe {
            self.queue.ExecuteCommandLists(&slot.release);
            self.queue.Signal(&slot.resource.ready, generation)?;
        }
        let frame = slot.frame.get_or_insert_with(|| {
            Arc::new(Frame {
                pixels: Vec::new(),
                generation: 0,
                timestamp_ns: 0,
                gpu: Some(GpuFrame {
                    resource: slot.resource.clone(),
                    value: 0,
                    acquired: AtomicBool::new(false),
                    disposition: AtomicU32::new(0),
                }),
            })
        });
        let mutable = Arc::get_mut(frame).expect("completed export has no outstanding leases");
        mutable.generation = generation;
        mutable.timestamp_ns = timestamp_ns;
        let gpu = mutable.gpu.as_mut().unwrap();
        gpu.value = generation;
        gpu.acquired.store(false, Ordering::Release);
        gpu.disposition.store(0, Ordering::Release);
        log::debug!(target: "rfvp::embedded::d3d12", "export generation={generation} resource={} ready={generation} raster={}x{} GPU_copy_bytes={} delivery_CPU_bytes=0",
            slot.resource.resource_id, self.size.width, self.size.height, u64::from(self.size.width) * u64::from(self.size.height) * 4);
        Ok(Some(frame.clone()))
    }

    /// Only our already submitted writes are drained. Host-held leases and
    /// consumption fences never gate cancellation/close or native save/audio work.
    pub fn drain(&self) -> Result<()> {
        // Cover native render submissions made while the export pool was full,
        // not just the last exported ready value. There is no external Wait.
        unsafe {
            self.queue.Signal(&self.shutdown, 1)?;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            unsafe {
                self.device.GetDeviceRemovedReason()?;
            }
            let completed = unsafe { self.shutdown.GetCompletedValue() };
            anyhow::ensure!(completed != u64::MAX, "RFVP_D3D12_DEVICE_LOST");
            if completed >= 1 {
                return Ok(());
            }
            anyhow::ensure!(Instant::now() < deadline, "RFVP_D3D12_DRAIN_TIMEOUT");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
