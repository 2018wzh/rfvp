use crate::rfvp_render::RenderTarget;

pub(super) enum PresentationTarget {
    Window(wgpu::Surface<'static>),
    Embedded(RenderTarget),
}

impl From<wgpu::Surface<'static>> for PresentationTarget {
    fn from(surface: wgpu::Surface<'static>) -> Self {
        Self::Window(surface)
    }
}

impl PresentationTarget {
    pub fn configure(&self, device: &wgpu::Device, config: &wgpu::SurfaceConfiguration) {
        match self {
            Self::Window(surface) => surface.configure(device, config),
            Self::Embedded(target) => assert_eq!(target.backing_size(), (config.width, config.height)),
        }
    }

    #[cfg(target_os = "android")]
    pub fn get_capabilities(&self, adapter: &wgpu::Adapter) -> wgpu::SurfaceCapabilities {
        match self {
            Self::Window(surface) => surface.get_capabilities(adapter),
            Self::Embedded(_) => unreachable!("embedded output has no Android surface"),
        }
    }
}
