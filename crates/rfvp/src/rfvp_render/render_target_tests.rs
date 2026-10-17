use super::*;

fn staging() -> (wgpu::Device, wgpu::Queue, RenderTargetReadback) {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        force_fallback_adapter: false,
        ..Default::default()
    }))
    .expect("hardware adapter");
    assert!(matches!(
        adapter.get_info().device_type,
        wgpu::DeviceType::DiscreteGpu
            | wgpu::DeviceType::IntegratedGpu
            | wgpu::DeviceType::VirtualGpu
    ));
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
            .unwrap();
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("read-into fault regression"),
        size: 256 * 3,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let readback = RenderTargetReadback {
        buffer,
        width: 63,
        height: 3,
        bytes_per_row: 252,
        padded_bytes_per_row: 256,
        completion: Arc::new(AtomicU64::new(0)),
        map_generation: 0,
        map_deadline: None,
    };
    (device, queue, readback)
}

fn write(queue: &wgpu::Queue, readback: &RenderTargetReadback, seed: u8) -> Vec<u8> {
    let mut padded = Vec::new();
    let mut expected = Vec::new();
    for row in 0..3 {
        expected.extend_from_slice(&[seed + row; 252]);
        padded.extend_from_slice(&[seed + row; 252]);
        padded.extend_from_slice(&[211; 4]);
    }
    queue.write_buffer(&readback.buffer, 0, &padded);
    queue.submit([]);
    expected
}

fn complete(readback: &mut RenderTargetReadback, device: &wgpu::Device, pixels: &mut Vec<u8>) {
    while !readback.poll_rgba8_into(device, pixels).unwrap() {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "requires a hardware GPU"]
pub fn read_into_reuses_rows_and_rejects_late_canceled_completion() {
    let (device, queue, mut readback) = staging();
    let expected = write(&queue, &readback, 11);
    readback.begin_mapping().unwrap();
    let mut pixels = vec![17, 19, 23];
    complete(&mut readback, &device, &mut pixels);
    assert_eq!(pixels, expected);
    let pointer = pixels.as_ptr();
    let capacity = pixels.capacity();
    readback.begin_mapping().unwrap();
    let old_pending = (readback.map_generation << 2) | 1;
    let completion = Arc::clone(&readback.completion);
    readback.cancel_mapping();
    let expected = write(&queue, &readback, 101);
    readback.begin_mapping().unwrap();
    // Deliver the old callback's completion after the next map starts.
    let _ = completion.compare_exchange(
        old_pending,
        (old_pending & !3) | 2,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    complete(&mut readback, &device, &mut pixels);
    assert_eq!(pixels, expected);
    assert_eq!(pixels.as_ptr(), pointer);
    assert_eq!(pixels.capacity(), capacity);
}

#[test]
#[ignore = "requires a hardware GPU"]
pub fn read_into_abort_and_timeout_preserve_output_and_allow_new_map() {
    let (device, queue, mut readback) = staging();
    write(&queue, &readback, 11);
    let mut pixels = vec![17, 19, 23];
    readback.begin_mapping().unwrap();
    readback.buffer.unmap();
    assert_eq!(
        readback.poll_rgba8_into(&device, &mut pixels),
        Err("RFVP_GPU_READBACK_FAILED")
    );
    assert_eq!(pixels, [17, 19, 23]);
    readback.begin_mapping().unwrap();
    // Hold this completion notification pending; the GPU itself need not hang.
    let pending = (readback.map_generation << 2) | 1;
    readback.completion = Arc::new(AtomicU64::new(pending));
    readback.map_deadline = Some(Instant::now() - Duration::from_secs(1));
    assert_eq!(
        readback.poll_rgba8_into(&device, &mut pixels),
        Err("RFVP_GPU_READBACK_TIMEOUT")
    );
    assert_eq!(pixels, [17, 19, 23]);
    let expected = write(&queue, &readback, 101);
    readback.begin_mapping().unwrap();
    complete(&mut readback, &device, &mut pixels);
    assert_eq!(pixels, expected);
}
