//! Windowless endpoint for the original App. VM, scene, UI, media and saves remain native.
use super::*;
use crate::rfvp_render::render_target::RenderTargetReadback;
use crate::subsystem::anzu_scene::AnzuScene;
use std::sync::atomic::AtomicBool;

pub mod session;

pub struct EmbeddedApp {
    app: Box<App>,
    readback: Option<RenderTargetReadback>,
    paused_at: Option<Instant>,
    closed: bool,
}

/// Maps a requested raster extent to a uniform scale of the logical size.
/// 0x0 keeps the native 1:1 raster.
pub fn render_scale(logical: (u32, u32), requested: (u32, u32)) -> f64 {
    if requested == (0, 0) { return 1.0; }
    (requested.0 as f64 / logical.0 as f64).min(requested.1 as f64 / logical.1 as f64).clamp(0.25, 4.0)
}

pub fn raster_size(logical: (u32, u32), requested: (u32, u32)) -> Result<(u32, u32)> {
    let scale = render_scale(logical, requested);
    let raster = ((logical.0 as f64 * scale).round() as u32, (logical.1 as f64 * scale).round() as u32);
    anyhow::ensure!(raster.0 > 0 && raster.1 > 0 && raster.0 <= 8192 && raster.1 <= 8192,
        "RFVP_RENDER_SIZE_UNSUPPORTED");
    Ok(raster)
}

impl EmbeddedApp {
    pub fn open(game_root: &str, nls: Nls, render_extent: (u32, u32), translation_table: &str, subtitle_map: &str,
        cancellation: Arc<AtomicBool>) -> Result<Self> {
        anyhow::ensure!(!cancellation.load(std::sync::atomic::Ordering::Acquire), "RFVP_SESSION_CANCELLED");
        crate::utils::file::set_base_path(game_root);
        let mut parser = crate::boot::load_script(nls)?;
        if !translation_table.is_empty() {
            let relative = std::path::Path::new(translation_table);
            anyhow::ensure!(translation_table.len() <= 4096 && !relative.is_absolute() && !translation_table.contains([':', '\\', '\0'])
                && relative.components().all(|part| matches!(part, std::path::Component::Normal(_))),
                "RFVP_TRANSLATION_PATH_INVALID");
            let root = std::fs::canonicalize(game_root)?;
            let path = std::fs::canonicalize(root.join(relative))?;
            anyhow::ensure!(path.starts_with(&root), "RFVP_TRANSLATION_PATH_OUTSIDE_ROOT");
            let file = std::fs::File::open(path)?;
            anyhow::ensure!(file.metadata()?.len() <= 64 * 1024 * 1024, "RFVP_TRANSLATION_TOO_LARGE");
            let mut bytes = Vec::new();
            use std::io::Read;
            file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 64 * 1024 * 1024, "RFVP_TRANSLATION_TOO_LARGE");
            parser.set_translation_table(&bytes)?;
        }
        anyhow::ensure!(!cancellation.load(std::sync::atomic::Ordering::Acquire), "RFVP_SESSION_CANCELLED");
        let title = parser.get_title();
        let logical = parser.get_screen_size();
        anyhow::ensure!(logical.0 != 0 && logical.1 != 0, "RFVP_LOGICAL_SIZE_INVALID");
        let raster = raster_size(logical, render_extent)?;
        #[cfg(feature = "subtitles")]
        let subtitles = if subtitle_map.is_empty() { None } else {
            Some(crate::subtitles::Subtitles::load(game_root, subtitle_map)?)
        };
        #[cfg(not(feature = "subtitles"))]
        anyhow::ensure!(subtitle_map.is_empty(), "RFVP_SUBTITLE_SUPPORT_NOT_COMPILED");
        // The embedding application owns logging. Do not initialize the desktop logger here.
        let builder = AppBuilder::new_boxed(crate::boot::app_config(&title, logical))
            .with_scene::<AnzuScene>()
            .with_script_engine(ThreadManager::new())
            .with_window_title(&title)
            .with_window_size(logical)
            .with_parser(parser)
            .with_text_hidpi_enabled(true)
            .with_vfs(nls)?;
        #[allow(unused_mut)]
        let mut app = builder.build_embedded(raster, cancellation)?;
        #[cfg(feature = "subtitles")]
        { app.subtitles = subtitles; }
        Ok(Self { app, readback: None, paused_at: None, closed: false })
    }

    pub fn logical_size(&self) -> (u32, u32) { self.app.virtual_size }
    pub fn raster_size(&self) -> (u32, u32) {
        (self.app.surface_config.width, self.app.surface_config.height)
    }
    pub fn context_epoch(&self) -> u64 { self.app.vm_worker.timeline_epoch() }
    pub fn take_window_mode(&mut self) -> Option<i32> {
        let mode = gd_write(&self.app.game_data).take_pending_render_flag();
        if let Some(mode @ (1 | 2)) = mode { self.app.last_fullscreen_flag = mode; }
        mode
    }

    pub fn take_pointer(&mut self) -> Option<session::PointerRequest> {
        let mut gd = gd_write(&self.app.game_data);
        let request = session::PointerRequest {
            position: gd.window_ref().new_cursor_pos(),
            visible: gd.window_ref().new_cursor_visible(),
        };
        gd.window_mut().reset_future_settings();
        (request.position.is_some() || request.visible.is_some()).then_some(request)
    }

    pub fn step(&mut self, elapsed: Duration) -> Result<bool> {
        anyhow::ensure!(self.paused_at.is_none(), "RFVP_SESSION_PAUSED");
        // Set even zero explicitly: native Time must never charge suspension time.
        gd_write(&self.app.game_data).time_mut_ref().set_external_delta(elapsed);
        self.app.try_host_step(0)
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        if paused == self.paused_at.is_some() { return Ok(()); }
        let mut gd = gd_write(&self.app.game_data);
        gd.audio_manager.backend().set_paused(paused)?;
        if paused {
            self.paused_at = Some(Instant::now());
            gd.inputs_manager.set_flash();
        } else if let Some(start) = self.paused_at.take() {
            gd.video_manager.resume_after(start.elapsed());
            gd.time_mut_ref().set_external_delta(Duration::ZERO);
        }
        Ok(())
    }

    pub fn input_key(&mut self, physical: PhysicalKey, logical: winit::keyboard::Key, pressed: bool, repeat: bool) {
        if self.app.exit_confirm_ui.handle_key(physical, pressed, repeat) {
            if let Some(outcome) = self.app.exit_confirm_ui.take_outcome() {
                self.app.apply_exit_confirm_outcome(outcome);
            }
            return;
        }
        let mut gd = gd_write(&self.app.game_data);
        if self.app.legacy_save_load_ui.handle_key(physical, pressed, repeat, &mut gd) {
            if !self.app.legacy_save_load_ui.is_active() { set_legacy_save_load_menu_visible(false); }
            return;
        }
        if physical == PhysicalKey::Code(KeyCode::F11) && pressed && !repeat {
            let current = gd.get_render_flag();
            let last = if matches!(self.app.last_fullscreen_flag, 1 | 2) { self.app.last_fullscreen_flag } else { 1 };
            gd.set_render_flag(if matches!(current, 1 | 2) { 0 } else { last });
        }
        if pressed { gd.inputs_manager.notify_keydown(logical, repeat); }
        else { gd.inputs_manager.notify_keyup(logical); }
    }

    pub fn input_window_event(&mut self, event: &WindowEvent) {
        let raster = self.raster_size();
        if self.app.exit_confirm_ui.handle_window_event(event, raster, self.app.virtual_size) {
            if let Some(outcome) = self.app.exit_confirm_ui.take_outcome() {
                self.app.apply_exit_confirm_outcome(outcome);
            }
            return;
        }
        let mut gd = gd_write(&self.app.game_data);
        if self.app.legacy_save_load_ui.handle_window_event(event, raster, self.app.virtual_size, &mut gd) {
            if !self.app.legacy_save_load_ui.is_active() { set_legacy_save_load_menu_visible(false); }
            return;
        }
        // Window resize is a Presenter concern. Only frame-pixel input reaches this mapper.
        update_input_events(event, &mut gd, raster, self.app.virtual_size);
    }

    pub fn begin_capture(&mut self) -> Result<()> {
        let PresentationTarget::Embedded(target) = &self.app.surface else { unreachable!() };
        let mut encoder = self.app.resources.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("rfvp complete UI readback") });
        let readback = target.encode_readback_rgba8(&self.app.resources.device, &mut encoder, &mut self.readback);
        self.app.resources.queue.submit(Some(encoder.finish()));
        readback.begin_mapping().map_err(anyhow::Error::msg)
    }

    pub fn poll_capture(&mut self, pixels: &mut Vec<u8>) -> Result<bool> {
        self.readback.as_mut().context("RFVP_CAPTURE_NOT_STARTED")?
            .poll_rgba8_into(&self.app.resources.device, pixels).map_err(anyhow::Error::msg)
    }

    pub fn cancel_capture(&mut self) {
        if let Some(readback) = self.readback.as_mut() { readback.cancel_mapping(); }
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if self.closed { return Ok(()); }
        self.closed = true;
        self.cancel_capture();
        self.app.vm_worker.shutdown();
        let mut guard = gd_write(&self.app.game_data);
        let gd = &mut **guard;
        gd.video_manager.stop(&mut gd.motion_manager);
        let audio_result = gd.audio_manager.backend().shutdown();
        let save_result = crate::subsystem::global_savedata::save_global_savedata_v1(gd);
        audio_result?;
        save_result?;
        Ok(())
    }
}

impl Drop for EmbeddedApp {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            log::error!("native embedded shutdown: {error:#}");
        }
    }
}

impl AppBuilder {
    fn build_embedded(mut self, raster: (u32, u32), cancellation: Arc<AtomicBool>) -> Result<Box<App>> {
        anyhow::ensure!(!cancellation.load(std::sync::atomic::Ordering::Acquire), "RFVP_SESSION_CANCELLED");
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let adapter = futures::executor::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })).context("RFVP_GPU_ADAPTER_UNAVAILABLE")?;
        let adapter_info = adapter.get_info();
        anyhow::ensure!(adapter_info.device_type != wgpu::DeviceType::Cpu, "RFVP_HARDWARE_GPU_REQUIRED");
        log::info!(target: "rfvp::embedded", "Native GPU {:?}; logical {:?}; raster {:?}", adapter_info, self.size, raster);
        let mut limits = wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits());
        limits.max_push_constant_size = 80;
        anyhow::ensure!(raster.0 <= limits.max_texture_dimension_2d && raster.1 <= limits.max_texture_dimension_2d,
            "RFVP_RENDER_SIZE_EXCEEDS_DEVICE");
        let (device, queue) = futures::executor::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rfvp embedded native device"), required_features: wgpu::Features::PUSH_CONSTANTS,
            required_limits: limits,
        }, None))?;
        anyhow::ensure!(!cancellation.load(std::sync::atomic::Ordering::Acquire), "RFVP_SESSION_CANCELLED");
        let queue = Arc::new(queue);
        let bind_group_layouts = BindGroupLayouts::new(&device);
        let pipelines = Pipelines::new(&device, &queue, &bind_group_layouts, RenderTarget::FORMAT);
        let resources = Arc::new(GpuCommonResources { device, queue,
            render_buffer_size: RwLock::new(self.size), bind_group_layouts, pipelines });
        let render_target = RenderTarget::new(&resources, self.size, raster, Some("rfvp native scene"));
        let final_target = RenderTarget::new(&resources, raster, raster, Some("rfvp complete game UI"));
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT, format: RenderTarget::FORMAT,
            width: raster.0, height: raster.1, present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque, view_formats: vec![], desired_maximum_frame_latency: 1,
        };
        GLOBAL.lock().map_err(|_| anyhow::anyhow!("RFVP_GLOBAL_STATE_POISONED"))?
            .init_with(self.parser.get_non_volatile_global_count(), self.parser.get_volatile_global_count());
        take_pending_exit_dialog_request();
        crate::subsystem::components::syscalls::legacy::reset_native_session_state()?;
        self.world.fontface_manager.init_fontface()?;
        crate::subsystem::global_savedata::try_load_global_savedata_v1(&mut self.world)?;
        self.script_engine.start_main(self.parser.get_entry_point());
        self.world.nls = self.parser.nls.clone();
        self.world.set_can_fullscreen(false);
        let w = self.size.0 as f32;
        let h = self.size.1 as f32;
        let vertices = [PosVertex { position: vec3(0.0, 0.0, 0.0) }, PosVertex { position: vec3(w, 0.0, 0.0) },
            PosVertex { position: vec3(w, h, 0.0) }, PosVertex { position: vec3(0.0, h, 0.0) }];
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let dissolve_vertex_buffer = resources.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rfvp dissolve quad VB"), contents: bytemuck::cast_slice(&vertices), usage: wgpu::BufferUsages::VERTEX });
        let dissolve_index_buffer = resources.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rfvp dissolve quad IB"), contents: bytemuck::cast_slice(&indices), usage: wgpu::BufferUsages::INDEX });
        let builtin_exit_ui_enabled = self.parser.get_all_syscalls().values().any(|call| call.name == "ConfigEtc");
        let ui_font = self.world.fontface_manager.get_font(crate::subsystem::resources::text_manager::FONTFACE_MS_GOTHIC);
        let legacy_save_load_ui = LegacySaveLoadUi::new(&resources, self.size, ui_font.clone());
        let exit_confirm_ui = ExitConfirmUi::new(&resources, self.size, ui_font);
        let game_data = Arc::new(RwLock::new(self.world));
        let vm_worker = VmWorker::spawn_with_cancellation(game_data.clone(), self.parser, self.script_engine, cancellation);
        let mut app = Box::new(App {
            config: self.config, game_data, title: self.title, vm_worker,
            pending_vm_frame_ms: 0, pending_vm_frame_ms_valid: false, scheduler: self.scheduler,
            layer_machine: SceneMachine { current_scene: self.scene }, window: None,
            native_scale_factor: raster.0 as f64 / self.size.0 as f64, pump_mode: true,
            windowed_restore: None, last_fullscreen_flag: 0, render_target, save_readback: None,
            prim_renderer: GpuPrimRenderer::new(resources.clone(), self.size), resources,
            surface: PresentationTarget::Embedded(final_target), surface_config,
            #[cfg(any(target_os = "ios", target_os = "android"))]
            wgpu_instance: Some(instance),
            #[cfg(any(target_os = "ios", target_os = "android"))]
            wgpu_adapter: Some(adapter),
            #[cfg(target_os = "android")]
            android_native_window: None,
            virtual_size: self.size, render_tree: RenderTree::new(), dissolve_vertex_buffer, dissolve_index_buffer,
            dissolve_num_indices: indices.len() as u32, debug_hud: None, hud_window: None, hud_surface: None,
            hud_surface_config: None, hud_visible: false,
            debug_ring: log_ring::get().unwrap_or_else(|| log_ring::init(4096)), debug_frame_no: 0,
            last_dt_ms: 0.0, hud_cursor_pos: None, hud_pointer_down: false, hud_scroll_delta_y: 0.0,
            last_dissolve_type: DissolveType::None, last_dissolve2_transitioning: false,
            legacy_save_load_ui, exit_confirm_ui, builtin_exit_ui_enabled, pending_app_exit: false,
            #[cfg(feature = "subtitles")]
            subtitles: None,
        });
        app.setup();
        Ok(app)
    }
}
