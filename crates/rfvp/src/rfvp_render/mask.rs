use super::{GpuCommonResources, GpuTexture, RenderTarget};
use crate::subsystem::resources::motion_manager::{DissolveType, MotionManager};
use image::GenericImageView;

/// Original RFVP mask transitions composite a frozen previous frame, not a color.
pub struct MaskDissolve {
    pipeline: wgpu::RenderPipeline,
    previous: RenderTarget,
    mask: Option<GpuTexture>,
    generation: Option<u64>,
    parameters: Option<[u32; 2]>,
}

impl MaskDissolve {
    pub fn new(resources: &GpuCommonResources, size: (u32, u32)) -> Self {
        let device = &resources.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rfvp mask dissolve"),
            source: wgpu::ShaderSource::Wgsl(include_str!("pipelines/mask.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rfvp mask dissolve"),
            bind_group_layouts: &[
                &resources.bind_group_layouts.texture,
                &resources.bind_group_layouts.texture,
            ],
            push_constant_ranges: &[wgpu::PushConstantRange {
                stages: wgpu::ShaderStages::FRAGMENT,
                range: 0..8,
            }],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rfvp mask dissolve"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: RenderTarget::FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });
        Self {
            pipeline,
            previous: RenderTarget::new(resources, size, size, Some("rfvp previous frame")),
            mask: None,
            generation: None,
            parameters: None,
        }
    }

    pub fn prepare(
        &mut self,
        resources: &GpuCommonResources,
        current: &RenderTarget,
        motion: &MotionManager,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<(), &'static str> {
        let mode = motion.get_dissolve_type();
        if !matches!(
            mode,
            DissolveType::MaskFadeIn | DissolveType::MaskFadeInOut | DissolveType::MaskFadeOut
        ) {
            self.parameters = None;
            return Ok(());
        }
        let (generation, phase) = motion.dissolve_render_state();
        if self.generation != Some(generation) {
            let graph = motion.get_dissolve_mask_graph();
            let image = graph.texture.as_ref().ok_or("RFVP_GPU_MASK_MISSING")?;
            if image.dimensions() != current.logical_size() {
                return Err("RFVP_GPU_MASK_DIMENSIONS");
            }
            self.mask = Some(GpuTexture::new_rgba8_unorm(
                resources,
                image,
                Some("rfvp dissolve mask"),
            ));
            if self.previous.backing_size() != current.backing_size() {
                self.previous = RenderTarget::new(
                    resources,
                    current.logical_size(),
                    current.backing_size(),
                    Some("rfvp previous frame"),
                );
            }
            current.copy_to(encoder, &self.previous);
            self.generation = Some(generation);
        }
        self.parameters = Some([mode as u32, phase]);
        Ok(())
    }

    pub fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        let Some(parameters) = self.parameters else {
            return;
        };
        let mask = self.mask.as_ref().expect("prepared mask");
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, self.previous.bind_group().raw(), &[]);
        pass.set_bind_group(1, mask.bind_group().raw(), &[]);
        pass.set_push_constants(
            wgpu::ShaderStages::FRAGMENT,
            0,
            bytemuck::bytes_of(&parameters),
        );
        pass.draw(0..3, 0..1);
    }
}
