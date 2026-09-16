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
    fill_vertices: wgpu::Buffer,
    mask: super::mask::MaskDissolve,
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
            mask: super::mask::MaskDissolve::new(&resources, size),
            resources,
            fill_vertices,
        }
    }

    pub fn render(&mut self, motion: &MotionManager) -> Result<Vec<u8>, &'static str> {
        self.primitives.rebuild(&self.resources, motion);
        let mut encoder =
            self.resources
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("rfvp hosted frame"),
                });
        self.mask
            .prepare(&self.resources, &self.target, motion, &mut encoder)?;
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
            self.mask.draw(&mut pass);
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
        let readback = self
            .target
            .encode_readback_rgba8(&self.resources.device, &mut encoder);
        self.resources.queue.submit(Some(encoder.finish()));
        readback.try_map_to_rgba8(&self.resources.device)
    }
}
