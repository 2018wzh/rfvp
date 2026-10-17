//! Offscreen embedding of the native primitive renderer.
use std::sync::Arc;

use glam::{vec3, vec4};
use wgpu::util::DeviceExt;

use super::{GpuCommonResources, PosVertex, RenderTarget, VertexSource};
use crate::rendering::gpu_prim::GpuPrimRenderer;
use crate::subsystem::resources::motion_manager::{DissolveType, MotionManager};

pub struct HostedGpuRenderer {
    resources: Arc<GpuCommonResources>,
    primitives: GpuPrimRenderer,
    target: RenderTarget,
    readback: Option<super::render_target::RenderTargetReadback>,
    frame_pending: bool,
    fill_vertices: wgpu::Buffer,
}

impl HostedGpuRenderer {
    pub fn new(resources: Arc<GpuCommonResources>, size: (u32, u32)) -> Self {
        let (w, h) = (size.0 as f32, size.1 as f32);
        let vertices =
            [(0., 0.), (w, 0.), (w, h), (0., 0.), (w, h), (0., h)].map(|(x, y)| PosVertex {
                position: vec3(x, y, 0.),
            });
        let fill_vertices =
            resources
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("rfvp hosted dissolve vertices"),
                    contents: bytemuck::cast_slice(&vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });
        Self {
            primitives: GpuPrimRenderer::new(resources.clone(), size),
            target: RenderTarget::new(&resources, size, size, Some("rfvp hosted target")),
            readback: None,
            frame_pending: false,
            resources,
            fill_vertices,
        }
    }

    pub fn render(&mut self, motion: &MotionManager) -> Result<Vec<u8>, &'static str> {
        // Synchronous boot/resume capture; ordinary frames use submit/poll.
        self.cancel_pending();
        self.submit(motion)?;
        self.wait_for_frame()
    }

    pub fn capture_current_thumbnail(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, &'static str> {
        let bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("RFVP_THUMBNAIL_SIZE")?;
        if width == 0 || height == 0 || bytes > 64 * 1024 * 1024 {
            return Err("RFVP_THUMBNAIL_SIZE");
        }
        let (source_width, source_height, pixels) = self.capture_current_frame()?;
        if (width, height) == (source_width, source_height) {
            return Ok(pixels);
        }
        let source = image::RgbaImage::from_raw(source_width, source_height, pixels)
            .ok_or("RFVP_THUMBNAIL_SOURCE")?;
        Ok(image::imageops::resize(
            &source,
            width,
            height,
            image::imageops::FilterType::Triangle,
        )
        .into_raw())
    }

    fn capture_current_frame(&mut self) -> Result<(u32, u32, Vec<u8>), &'static str> {
        self.cancel_pending();
        let mut encoder =
            self.resources
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("rfvp native target snapshot"),
                });
        let readback = self.target.encode_readback_rgba8(
            &self.resources.device,
            &mut encoder,
            &mut self.readback,
        );
        let dimensions = (readback.width, readback.height);
        self.resources.queue.submit(Some(encoder.finish()));
        readback.begin_mapping()?;
        self.frame_pending = true;
        Ok((dimensions.0, dimensions.1, self.wait_for_frame()?))
    }

    fn wait_for_frame(&mut self) -> Result<Vec<u8>, &'static str> {
        let mut pixels = Vec::new();
        loop {
            if self.poll_frame_into(&mut pixels)? {
                return Ok(pixels);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub fn frame_pending(&self) -> bool {
        self.frame_pending
    }

    pub fn is_graph_uploaded(&self, graph_id: u16, generation: u64) -> bool {
        self.primitives.is_graph_uploaded(graph_id, generation)
    }

    pub fn cancel_pending(&mut self) {
        if self.frame_pending {
            if let Some(readback) = self.readback.as_mut() {
                readback.cancel_mapping();
            }
            self.frame_pending = false;
        }
    }

    pub fn poll_frame_into(&mut self, pixels: &mut Vec<u8>) -> Result<bool, &'static str> {
        if !self.frame_pending {
            return Ok(false);
        }
        let result = self
            .readback
            .as_mut()
            .expect("pending readback exists")
            .poll_rgba8_into(&self.resources.device, pixels);
        if !matches!(result, Ok(false)) {
            self.frame_pending = false;
        }
        result
    }

    pub fn submit(&mut self, motion: &MotionManager) -> Result<(), &'static str> {
        if self.frame_pending {
            return Err("RFVP_GPU_READBACK_PENDING");
        }
        if self.primitives.mask_wants_snapshot(motion) {
            let (width, height, pixels) = self.capture_current_frame()?;
            let frame = image::RgbaImage::from_raw(width, height, pixels)
                .ok_or("RFVP_MASK_SNAPSHOT_SOURCE")?;
            self.primitives.set_mask_snapshot(motion, frame);
        }
        self.primitives.rebuild(&self.resources, motion);
        let mut encoder =
            self.resources
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("rfvp hosted frame"),
                });
        {
            let mut pass = self
                .target
                .begin_srgb_render_pass(&mut encoder, Some("rfvp hosted primitives"));
            let projection = self.target.projection_matrix();
            self.primitives.draw_virtual_root0(
                &mut pass,
                &self.resources.pipelines.sprite,
                projection,
            );
            let alpha = match motion.get_dissolve_type() {
                DissolveType::None => 0.,
                DissolveType::Static
                | DissolveType::ColoredFadeIn
                | DissolveType::ColoredFadeOut => motion.get_dissolve_alpha(),
                DissolveType::MaskFadeIn
                | DissolveType::MaskFadeInOut
                | DissolveType::MaskFadeOut => 0.,
            };
            for (id, alpha) in [
                (motion.get_dissolve_color_id(), alpha),
                (
                    motion.get_dissolve2_color_id(),
                    motion.get_dissolve2_alpha(),
                ),
            ] {
                if alpha > 0. {
                    let color = motion.color_manager.get_entry(id as u8);
                    self.resources.pipelines.fill.draw(
                        &mut pass,
                        VertexSource::VertexBuffer {
                            vertex_buffer: &self.fill_vertices,
                            vertices: 0..6,
                            instances: 0..1,
                        },
                        projection,
                        vec4(
                            color.get_r() as f32 / 255.,
                            color.get_g() as f32 / 255.,
                            color.get_b() as f32 / 255.,
                            color.get_a() as f32 / 255. * alpha,
                        ),
                    );
                }
            }
            self.primitives.draw_virtual_overlay(
                &mut pass,
                &self.resources.pipelines.sprite,
                projection,
            );
        }
        let readback = self.target.encode_readback_rgba8(
            &self.resources.device,
            &mut encoder,
            &mut self.readback,
        );
        self.resources.queue.submit(Some(encoder.finish()));
        readback.begin_mapping()?;
        self.frame_pending = true;
        Ok(())
    }
}
